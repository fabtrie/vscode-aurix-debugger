//! `adapter driver sim`: a small TC37x model for offline tests.
//!
//! Models OSTATE/OEC, CPU_ID, DBGSR halt/resume/SUSP, a running PC and PC
//! triggers (a resume with an armed trigger halts immediately at it).
//! Every 32-bit write is recorded in [`SimAdapter::writes`].
//!
//! The HSM ([`SimHsm`]) is reached through its debug window: DBGBASE selects
//! the upper 16 bits of the HSM address, only aligned word access works and
//! every access fails while HSM debugging is disabled.
//!
//! The board around the chip, modelled on a TC37x board with an FS26 SBC: STM0 counts and restarts at a
//! reset; the peripherals in [`SIM_PERIPHS`] have CLC/ACCEN0/OCS, and an OCS write the hardware would
//! refuse (module disabled, ACCEN0 without the debugger) fails and is recorded in
//! [`SimAdapter::violations`] (on the board it resets it); the application ([`SimApp`]) checks for the
//! debugger, locks STM0/1 and enables its modules at fixed times after it starts; the FS26
//! ([`SimFs26`]) resets the chip when a halt outlasts its watchdog window or its INIT_FS phase, and
//! goes through deep fail-safe after `dfs_limit` faults.

use std::any::Any;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use tas::{Batch, BatchOp, OpResult, PL0_ERR_DATA, PL0_ERR_NO_ERROR};

use crate::adapter::{Adapter, AdapterSettings, BatchLimits, DeviceInfo, LinkHealth};
use crate::arch::{OEC, OSTATE};
use crate::cortex_m::*;
use crate::error::Result;
use crate::sim_mcs::SimGtm;

const NUM_CORES: usize = 3;

/// Peripherals of the simulated board: (OCS, CLC, ACCEN0, enabled after reset). STM0-2 and the SMU
/// are on after a reset, the others only once the application enables them.
pub const SIM_PERIPHS: &[(u32, u32, u32, bool)] = &[
    (0xF000_10E8, 0xF000_1000, 0xF000_10FC, true),  // STM0
    (0xF000_11E8, 0xF000_1100, 0xF000_11FC, true),  // STM1
    (0xF000_12E8, 0xF000_1200, 0xF000_12FC, true),  // STM2
    (0xF003_6FE8, 0xF003_6800, 0xF003_6FFC, true),  // SMU
    (0xF000_18E8, 0xF000_1800, 0xF000_18FC, false), // GPT120
    (0xF000_1EE8, 0xF000_1E00, 0xF000_1EFC, false), // QSPI2
    (0xF000_20E8, 0xF000_2000, 0xF000_20FC, false), // QSPI4
    (0xF002_0028, 0xF002_0000, 0xF002_003C, false), // EVADC
    (0xF002_5028, 0xF002_5000, 0xF002_503C, false), // CONVCTRL
    (0xF019_FD38, 0xF019_FD00, 0xF019_FD10, false), // GTM
    (0xF020_80E8, 0xF020_8000, 0xF020_80FC, false), // CAN0
    (0xF021_80E8, 0xF021_8000, 0xF021_80FC, false), // CAN1
];

/// The simulated application's debugger flag (set at its debugger check) and SBC driver state.
pub const APP_MARKER: u32 = 0x7000_F000;
pub const APP_DRIVER_STATE: u32 = 0x6000_F000;
pub const APP_MARKER_DEBUGGER: u32 = 0xDEB0_0001;

const STM0_TIM0: u32 = 0xF000_1010;
const STM0_CAP: u32 = 0xF000_102C;

/// The application's start-up, as measured: ~25 ms after it starts it checks OCDS (OEN) and sets
/// the marker, then locks STM0/1 against the debugger (ACCEN0); ~34 ms its modules are enabled;
/// ~40 ms the FS26 is initialized (watchdog window infinite if the debugger was seen).
pub struct SimApp {
    pub check_ms: u64,
    pub enable_ms: u64,
    pub fs26_ms: u64,
    /// Start-up began (cores running after a reset, or cpu0 resumed after a halting reset).
    started: Option<Instant>,
    /// Halted at the reset vector: starts when cpu0 is resumed.
    pending: bool,
    stage: u8,
    pub saw_debugger: bool,
}

impl SimApp {
    fn new() -> Self {
        SimApp { check_ms: 25, enable_ms: 34, fs26_ms: 40, started: None, pending: false, stage: 3, saw_debugger: true }
    }
}

/// The FS26 safety SBC: its watchdog (armed when the application started without the debugger),
/// INIT_FS after an RSTB reset (a reset is due if the application has not initialized it 256 ms
/// later), deep fail-safe after `dfs_limit` faults (board power-cycled, restarts in debug mode).
pub struct SimFs26 {
    /// The debugger's reset is wired to RSTB.
    pub wired: bool,
    pub armed: bool,
    /// After power-up or deep fail-safe: no watchdog until the application initializes the FS26.
    pub debug_mode: bool,
    pub window: Duration,
    pub init_fs: Duration,
    init_fs_until: Option<Instant>,
    halted_since: Option<Instant>,
    pub faults: u32,
    pub dfs_limit: u32,
    /// Resets the FS26 caused, and deep fail-safe entries.
    pub resets: u32,
    pub dfs_entries: u32,
}

impl SimFs26 {
    pub fn new(wired: bool, armed: bool) -> Self {
        SimFs26 {
            wired,
            armed,
            debug_mode: false,
            window: Duration::from_millis(80),
            init_fs: Duration::from_millis(256),
            init_fs_until: None,
            halted_since: None,
            faults: 0,
            dfs_limit: 6,
            resets: 0,
            dfs_entries: 0,
        }
    }
}

const HSM_DBGBASE: u32 = BRIDGE_BASE + 0x1010;
const HSM_WINDOW: u32 = BRIDGE_BASE + 0x1_0000;

/// Cortex-M3 HSM behind the debug window.
pub struct SimHsm {
    /// DBGCTRL.HSM: debug window accessible.
    pub debug_enabled: bool,
    pub dbgbase: u32,
    pub halted: bool,
    pub dhcsr_ctrl: u32,
    pub dfsr: u32,
    /// r0-r12, sp, lr, pc, xpsr, msp, psp, -, CONTROL/FAULTMASK/BASEPRI/PRIMASK.
    pub regs: [u32; 21],
    dcrdr: u32,
    /// HSM memory and memory-mapped registers by HSM address.
    pub mem: HashMap<u32, u32>,
    /// Writes by HSM address (after the window translation).
    pub writes: Vec<(u32, u32)>,
    /// CACHE_SC writes since the last halt (cache clean).
    pub cache_cleans: u32,
    /// Local RAM reads while the cache was not cleaned during the current halt.
    pub uncleaned_reads: u32,
    /// Addresses that answer with a bus error: HSM addresses in the window (unused addresses inside
    /// a peripheral) and host addresses of the bridge (registers only the HSM may access).
    pub bus_errors: std::collections::HashSet<u32>,
    /// HSM addresses whose next window accesses fail, with the number of failures left (the window
    /// fails now and then while the HSM runs).
    pub flaky: HashMap<u32, u32>,
}

impl SimHsm {
    fn new() -> Self {
        let mut regs = [0; 21];
        regs[15] = 0x8002_85d0;
        SimHsm {
            debug_enabled: true,
            dbgbase: 0,
            halted: false,
            dhcsr_ctrl: 0,
            dfsr: 0,
            regs,
            dcrdr: 0,
            mem: HashMap::new(),
            writes: Vec::new(),
            cache_cleans: 0,
            uncleaned_reads: 0,
            bus_errors: Default::default(),
            flaky: HashMap::new(),
        }
    }

    fn halt(&mut self, dfsr: u32) {
        if !self.halted {
            self.cache_cleans = 0;
        }
        self.halted = true;
        self.dfsr |= dfsr;
        self.dhcsr_ctrl |= C_HALT;
    }

    /// Runs: halts at once at an armed FPB comparator (as the TriCore model does).
    fn run(&mut self) {
        self.halted = false;
        for n in 0..6 {
            let comp = *self.mem.get(&fp_comp(n)).unwrap_or(&0);
            if comp & 1 != 0 && *self.mem.get(&FP_CTRL).unwrap_or(&0) & 1 != 0 {
                let high = *self.mem.get(&fp_comph(n)).unwrap_or(&0) << 29;
                let half = if comp >> 30 == 2 { 2 } else { 0 };
                self.regs[15] = high | (comp & 0x1FFF_FFFC) | half;
                self.halt(DFSR_BKPT);
                return;
            }
        }
    }

    fn read(&mut self, addr: u32) -> u32 {
        match addr {
            CPUID => 0x492F_C230,
            FP_CTRL => 0x260 | (*self.mem.get(&FP_CTRL).unwrap_or(&0) & 1),
            DWT_CTRL => 0x4000_0000,
            CACHE_CONFIG => 0x0006_0404,
            DHCSR => {
                if !self.halted {
                    self.regs[15] = self.regs[15].wrapping_add(2);
                }
                (self.halted as u32) << 17 | S_REGRDY | (self.dhcsr_ctrl & 0xF)
            }
            DFSR => self.dfsr,
            DCRDR => self.dcrdr,
            a => {
                if (0x2000_0000..0x2001_8000).contains(&a) && self.cache_cleans < 64 {
                    self.uncleaned_reads += 1;
                }
                *self.mem.get(&a).unwrap_or(&0)
            }
        }
    }

    fn write(&mut self, addr: u32, v: u32) {
        self.writes.push((addr, v));
        match addr {
            DHCSR => {
                if v >> 16 != 0xA05F {
                    return;
                }
                self.dhcsr_ctrl = v & 0xF;
                if v & C_HALT != 0 {
                    self.halt(DFSR_HALTED);
                } else if v & C_STEP != 0 && self.halted {
                    self.regs[15] = self.regs[15].wrapping_add(2);
                    self.dhcsr_ctrl |= C_HALT;
                    self.dfsr |= DFSR_HALTED;
                } else if self.halted {
                    self.run();
                }
            }
            DFSR => self.dfsr &= !v,
            DCRDR => self.dcrdr = v,
            DCRSR => {
                let sel = (v & 0x7F) as usize;
                if sel < self.regs.len() {
                    if v & DCRSR_REGWNR != 0 {
                        self.regs[sel] = self.dcrdr;
                    } else {
                        self.dcrdr = self.regs[sel];
                    }
                }
            }
            CACHE_SC => {
                self.cache_cleans += 1;
            }
            a => {
                self.mem.insert(a, v);
            }
        }
    }
}

pub struct SimAdapter {
    mem: HashMap<u32, u32>,
    pub writes: Vec<(u32, u32)>,
    /// Host-space reads, as (address, length): lets tests assert how a batch was coalesced.
    pub reads: Vec<(u32, usize)>,
    /// Batches executed: one per probe round trip, which is what merging DAQ into the run-state
    /// poll is meant to save.
    pub execs: usize,
    /// Packet limits reported to the DAQ sampler; tests shrink this to force truncation.
    pub limits: BatchLimits,
    pub halted: [bool; NUM_CORES],
    pub susp: [bool; NUM_CORES],
    /// SYSCON.BHALT: core not started yet.
    pub bhalt: [bool; NUM_CORES],
    pub evtsrc: [u32; NUM_CORES],
    pub pc: [u32; NUM_CORES],
    pub ostate: u32,
    pub pending_resets: u64,
    pub device_id: u32,
    /// The application on the chip (start-up model), if any.
    pub app: Option<SimApp>,
    /// The FS26 on the board, if any.
    pub fs26: Option<SimFs26>,
    /// OCS writes the hardware refuses (on the board: a reset and an FS26 reset storm).
    pub violations: Vec<u32>,
    stm_epoch: Instant,
    pub hsm: SimHsm,
    pub gtm: SimGtm,
    initialized: bool,
    srst: bool,
    /// What [`Adapter::health`] reports: set by tests to drive the core's health events.
    pub health: Option<LinkHealth>,
}

/// Unmapped as on the chip, so that reads into it fail: the top of segment 9, right below the
/// non-cached PFLASH (reset vector 0xA0000000).
pub const SIM_UNMAPPED: std::ops::Range<u32> = 0x9FF0_0000..0xA000_0000;


impl Default for SimAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl SimAdapter {
    pub fn new() -> Self {
        let mut s = SimAdapter {
            mem: HashMap::new(),
            writes: Vec::new(),
            reads: Vec::new(),
            execs: 0,
            limits: BatchLimits::default(),
            halted: [false; NUM_CORES],
            susp: [false; NUM_CORES],
            bhalt: [false; NUM_CORES],
            evtsrc: [0; NUM_CORES],
            pc: [0x8000_1000, 0x8010_0000, 0x8020_0000],
            ostate: 0,
            pending_resets: 0,
            // Plain TC37x, as expected by OpenOCD's tc37x.cfg.
            device_id: 0x1020_7083,
            hsm: SimHsm::new(),
            gtm: SimGtm::new(),
            initialized: false,
            srst: false,
            health: None,
            app: Some(SimApp::new()),
            fs26: None,
            violations: Vec::new(),
            stm_epoch: Instant::now(),
        };
        // Attached to an application that has been running for a while (started with the debugger
        // attached): its modules on, STM0/1 locked.
        s.reset_periphs();
        s.app_stage(1);
        s.app_stage(2);
        s.app_stage(3);
        s
    }

    /// Peripheral registers after a power-on class reset: OCS cleared, ACCEN0 open, only the
    /// modules that are on by default enabled.
    fn reset_periphs(&mut self) {
        for &(ocs, clc, accen, on) in SIM_PERIPHS {
            self.mem.remove(&ocs);
            self.mem.insert(clc, if on { 0 } else { 3 });
            self.mem.insert(accen, 0xFFFF_FFFF);
        }
    }

    /// One step of the application's start-up (see [`SimApp`]).
    fn app_stage(&mut self, stage: u8) {
        let oen = self.ostate & 1 != 0;
        let Some(app) = self.app.as_mut() else { return };
        app.stage = stage;
        match stage {
            1 => {
                if app.started.is_some() {
                    app.saw_debugger = oen;
                }
                let marker = if app.saw_debugger { APP_MARKER_DEBUGGER } else { 0x0000_0001 };
                self.mem.insert(APP_MARKER, marker);
                self.mem.insert(0xF000_10FC, 0x4);
                self.mem.insert(0xF000_11FC, 0x40);
            }
            2 => {
                for &(_, clc, accen, on) in SIM_PERIPHS {
                    if !on {
                        self.mem.insert(clc, 0x8);
                        self.mem.insert(accen, 0x1000_0044);
                    }
                }
            }
            _ => {
                self.mem.insert(APP_DRIVER_STATE, 3);
                let saw = app.saw_debugger;
                if let Some(f) = self.fs26.as_mut() {
                    f.debug_mode = false;
                    f.init_fs_until = None;
                    f.armed = !saw;
                }
            }
        }
    }

    /// Lets the simulated application start without the debugger (watchdog armed if there is an
    /// FS26): as after a power-up with no debugger attached.
    pub fn app_started_without_debugger(&mut self) {
        if let Some(app) = self.app.as_mut() {
            app.saw_debugger = false;
        }
        self.mem.insert(APP_MARKER, 0x0000_0001);
        if let Some(f) = self.fs26.as_mut() {
            f.armed = true;
        }
    }

    /// A power-on class reset of the chip (debugger reset, FS26 RSTB): OCDS off, STM0 restarts,
    /// peripherals and the application from the beginning. `halt`: the debugger halts cpu0 at the
    /// reset vector (the application starts when it is resumed).
    fn power_on_reset(&mut self, halt: bool) {
        self.ostate = 0;
        self.halted = [halt, false, false];
        self.pc = [0x800A_8000, 0x8010_0000, 0x8020_0000];
        self.stm_epoch = Instant::now();
        self.reset_periphs();
        self.mem.insert(APP_MARKER, 0);
        self.mem.insert(APP_DRIVER_STATE, 0);
        if let Some(app) = self.app.as_mut() {
            app.stage = 0;
            app.pending = halt;
            app.started = if halt { None } else { Some(Instant::now()) };
        }
    }

    /// A reset by the FS26: the chip restarts and runs; the FS26 is in INIT_FS (or, at the fault
    /// limit, goes through deep fail-safe and restarts in debug mode).
    fn fs26_reset(&mut self) {
        self.power_on_reset(false);
        self.pending_resets += 1;
        if let Some(f) = self.fs26.as_mut() {
            f.resets += 1;
            f.halted_since = None;
            if f.faults >= f.dfs_limit {
                f.faults = 0;
                f.dfs_entries += 1;
                f.debug_mode = true;
                f.init_fs_until = None;
            } else {
                f.init_fs_until = Some(Instant::now() + f.init_fs);
            }
        }
    }

    /// Advances the application and the FS26 to now.
    fn tick(&mut self) {
        let now = Instant::now();
        if let Some(app) = self.app.as_ref() {
            if let (Some(t0), false) = (app.started, self.halted[0]) {
                let ms = now.duration_since(t0).as_millis() as u64;
                let (stage, check, enable, fs26) = (app.stage, app.check_ms, app.enable_ms, app.fs26_ms);
                if stage < 1 && ms >= check {
                    self.app_stage(1);
                }
                if stage < 2 && ms >= enable {
                    self.app_stage(2);
                }
                if stage < 3 && ms >= fs26 {
                    self.app_stage(3);
                }
            }
        }
        let any_halted = self.halted.iter().any(|h| *h);
        let Some(f) = self.fs26.as_mut() else { return };
        let mut reset = false;
        if !f.debug_mode && f.init_fs_until.is_some_and(|t| now >= t) {
            f.init_fs_until = None;
            f.faults += 1;
            reset = true;
        }
        if f.armed && !f.debug_mode && any_halted {
            let since = *f.halted_since.get_or_insert(now);
            if now.duration_since(since) >= f.window {
                f.faults += 1;
                reset = true;
            }
        } else {
            f.halted_since = None;
        }
        if reset {
            self.fs26_reset();
        }
    }

    /// Whether the hardware accepts this write (OCS of a disabled module or with ACCEN0 closed to
    /// the debugger: refused).
    fn write_allowed(&self, addr: u32) -> bool {
        match SIM_PERIPHS.iter().find(|p| p.0 == addr) {
            Some(&(_, clc, accen, _)) => {
                let c = *self.mem.get(&clc).unwrap_or(&0);
                let a = *self.mem.get(&accen).unwrap_or(&0);
                c & 2 == 0 && (a >> 28) & 1 != 0
            }
            None => true,
        }
    }

    /// Seeds a memory word, for tests that need a known value at an address.
    pub fn set_mem(&mut self, addr: u32, value: u32) {
        self.mem.insert(addr & !3, value);
    }

    /// A memory word as the simulated chip holds it (no read side effects).
    pub fn mem_word(&self, addr: u32) -> u32 {
        *self.mem.get(&(addr & !3)).unwrap_or(&0)
    }

    /// Simulates an external (watchdog) reset: OCDS disabled, cores running, STM0 restarting.
    pub fn external_reset(&mut self) {
        self.ostate = 0;
        self.halted = [false; NUM_CORES];
        self.stm_epoch = Instant::now();
        self.pending_resets += 1;
    }

    fn core_of(addr: u32) -> Option<(usize, u32)> {
        (0..NUM_CORES).find_map(|i| {
            let base = 0xF881_0000 + 0x2_0000 * i as u32;
            (addr >= base && addr < base + 0x1_0000).then(|| (i, addr - base))
        })
    }

    fn read32(&mut self, addr: u32) -> u32 {
        if addr == OSTATE {
            return self.ostate;
        }
        // STM0 at 100 MHz; reading TIM0 latches bits 63:32 into CAP.
        if addr == STM0_TIM0 {
            let ticks = (self.stm_epoch.elapsed().as_nanos() / 10) as u64;
            self.mem.insert(STM0_CAP, (ticks >> 32) as u32);
            return ticks as u32;
        }
        if let Some((i, off)) = Self::core_of(addr) {
            match off {
                0xFE18 => return 0x00C0_C021,
                0xFD00 => {
                    return (self.halted[i] as u32) << 1
                        | 1
                        | (self.susp[i] as u32) << 4
                        | if self.halted[i] { (self.evtsrc[i] & 0x1F) << 8 } else { 0 }
                }
                0xFE08 => {
                    if !self.halted[i] {
                        self.pc[i] = self.pc[i].wrapping_add(2);
                    }
                    return self.pc[i];
                }
                0xFE14 => return (self.bhalt[i] as u32) << 24,
                _ => {}
            }
        }
        *self.mem.get(&addr).unwrap_or(&0)
    }

    fn write32(&mut self, addr: u32, v: u32) {
        self.writes.push((addr, v));
        if addr == OEC {
            if self.ostate & 1 == 0 {
                self.ostate |= 1;
            }
            return;
        }
        // CBS_TLC: forcing trigger line 1 (TLSP1 = 3) halts the cores it is routed to (CBS_TRCx), as on
        // the chip; a core in boot halt stays there.
        if addr == 0xF000_0490 && v & 0x30 == 0x30 {
            for i in 0..NUM_CORES {
                let routed = self.mem.get(&(0xF000_0420 + 4 * i as u32)).is_some_and(|t| t & 0x100 != 0);
                if routed && !self.bhalt[i] && !self.halted[i] {
                    self.halted[i] = true;
                    self.evtsrc[i] = 0;
                }
            }
        }
        if let Some((i, off)) = Self::core_of(addr) {
            match off {
                0xFD00 => {
                    self.susp[i] = v & 0x10 != 0;
                    match (v >> 1) & 3 {
                        3 => {
                            self.halted[i] = true;
                            self.evtsrc[i] = 0;
                        }
                        2 => self.resume_core(i),
                        _ => {}
                    }
                    return;
                }
                0xFE08 => {
                    self.pc[i] = v;
                    return;
                }
                _ => {}
            }
        }
        self.mem.insert(addr, v);
    }

    /// HSM bridge accesses: only aligned words; DBGBASE and the debug window only while HSM
    /// debugging is enabled. `None` for other addresses and the other bridge registers.
    fn hsm_access(&mut self, op: &BatchOp) -> Option<OpResult> {
        let (addr, len, data) = match *op {
            BatchOp::Read { addr, len, addr_map: 0 } => (addr as u32, len, None),
            BatchOp::Write { addr, data, addr_map: 0 } => (addr as u32, data.len(), Some(data)),
            _ => return None,
        };
        if !(BRIDGE_BASE..BRIDGE_BASE + 0x2_0000).contains(&addr) {
            return None;
        }
        let bus_error = OpResult { err: PL0_ERR_DATA, bytes_ok: 0, data: vec![0; if data.is_some() { 0 } else { len }] };
        if len != 4 || addr & 3 != 0 {
            return Some(bus_error);
        }
        let window = (HSM_WINDOW..HSM_WINDOW + 0x1_0000).contains(&addr);
        if addr != HSM_DBGBASE && !window {
            return self.hsm.bus_errors.contains(&addr).then_some(bus_error);
        }
        if !self.hsm.debug_enabled {
            return Some(bus_error);
        }
        let hsm_addr = self.hsm.dbgbase | (addr.wrapping_sub(HSM_WINDOW) & 0xFFFF);
        if addr != HSM_DBGBASE {
            let flaky = self.hsm.flaky.get_mut(&hsm_addr).filter(|n| **n > 0).map(|n| *n -= 1).is_some();
            if flaky || self.hsm.bus_errors.contains(&hsm_addr) {
                return Some(bus_error);
            }
        }
        match data {
            Some(d) => {
                let v = u32::from_le_bytes([d[0], d[1], d[2], d[3]]);
                self.writes.push((addr, v));
                if addr == HSM_DBGBASE {
                    self.hsm.dbgbase = v & 0xFFFF_0000;
                } else {
                    self.hsm.write(hsm_addr, v);
                }
                Some(OpResult { err: PL0_ERR_NO_ERROR, bytes_ok: 4, data: Vec::new() })
            }
            None => {
                let v = if addr == HSM_DBGBASE { self.hsm.dbgbase } else { self.hsm.read(hsm_addr) };
                Some(OpResult { err: PL0_ERR_NO_ERROR, bytes_ok: 4, data: v.to_le_bytes().to_vec() })
            }
        }
    }

    /// GTM register and MCS RAM accesses (word access); `None` for other addresses.
    fn gtm_access(&mut self, op: &BatchOp) -> Option<OpResult> {
        match *op {
            BatchOp::Read { addr, len, addr_map: 0 } if SimGtm::claims(addr as u32) => {
                let mut data = Vec::with_capacity(len);
                for k in (0..len).step_by(4) {
                    let v = self.gtm.read(addr as u32 + k as u32).unwrap_or(0);
                    data.extend_from_slice(&v.to_le_bytes());
                }
                data.truncate(len);
                Some(OpResult { err: PL0_ERR_NO_ERROR, bytes_ok: len, data })
            }
            BatchOp::Write { addr, data, addr_map: 0 } if SimGtm::claims(addr as u32) => {
                for (k, chunk) in data.chunks(4).enumerate() {
                    let mut w = [0u8; 4];
                    w[..chunk.len()].copy_from_slice(chunk);
                    let (a, v) = (addr as u32 + 4 * k as u32, u32::from_le_bytes(w));
                    self.writes.push((a, v));
                    if self.gtm.write(a, v) == Some(false) {
                        return Some(OpResult { err: PL0_ERR_DATA, bytes_ok: 4 * k, data: Vec::new() });
                    }
                }
                Some(OpResult { err: PL0_ERR_NO_ERROR, bytes_ok: data.len(), data: Vec::new() })
            }
            _ => None,
        }
    }

    /// A store of core `i` (running) to `addr`: halts it at an armed store trigger as the chip
    /// does, after the store. A trigger compares the start address of the access: equal to its
    /// address, or for a range in `[TRxADR, TRx+1ADR)`. Returns whether the core halted.
    pub fn store(&mut self, i: usize, addr: u32) -> bool {
        if self.halted[i] {
            return false;
        }
        let base = 0xF881_0000 + 0x2_0000 * i as u32;
        let word = |s: &Self, a: u32| *s.mem.get(&a).unwrap_or(&0);
        let mut n = 0;
        while n < 8u32 {
            let evt = word(self, base + 0xF000 + 8 * n);
            let range = evt & (1 << 13) != 0;
            if evt & (1 << 12) == 0 && evt & 7 != 0 && (evt >> 27) & 1 != 0 {
                let lo = word(self, base + 0xF004 + 8 * n);
                let hit = if range { (lo..word(self, base + 0xF004 + 8 * (n + 1))).contains(&addr) } else { addr == lo };
                if hit {
                    self.halted[i] = true;
                    self.evtsrc[i] = 16 + n;
                    self.pc[i] = self.pc[i].wrapping_add(4);
                    return true;
                }
            }
            n += if range { 2 } else { 1 };
        }
        false
    }

    fn resume_core(&mut self, i: usize) {
        self.halted[i] = false;
        if i == 0 {
            if let Some(app) = self.app.as_mut() {
                if app.pending {
                    app.pending = false;
                    app.started = Some(Instant::now());
                }
            }
        }
        let base = 0xF881_0000 + 0x2_0000 * i as u32;
        for n in 0..8u32 {
            let evt = *self.mem.get(&(base + 0xF000 + 8 * n)).unwrap_or(&0);
            if evt & (1 << 12) != 0 && evt & 7 != 0 {
                let adr = *self.mem.get(&(base + 0xF004 + 8 * n)).unwrap_or(&0);
                self.halted[i] = true;
                self.evtsrc[i] = 16 + n;
                self.pc[i] = if evt & 8 != 0 { adr } else { adr.wrapping_add(2) };
                break;
            }
        }
    }
}

impl Adapter for SimAdapter {
    fn driver_name(&self) -> &'static str {
        "sim"
    }

    fn init(&mut self, _s: &AdapterSettings) -> Result<DeviceInfo> {
        self.initialized = true;
        Ok(DeviceInfo { device_id: self.device_id, description: "simulated TC37x".into() })
    }

    fn initialized(&self) -> bool {
        self.initialized
    }

    fn limits(&self) -> BatchLimits {
        self.limits
    }

    fn execute(&mut self, batch: &Batch) -> Result<Vec<OpResult>> {
        self.tick();
        self.execs += 1;
        let mut out = Vec::with_capacity(batch.len());
        // As on the chip, a failing bridge access fails the bridge accesses after it in the batch.
        let mut bridge_failed = false;
        for op in batch.iter() {
            let (a, len, write) = match op {
                BatchOp::Read { addr, len, .. } => (addr as u32, len, false),
                BatchOp::Write { addr, data, .. } => (addr as u32, data.len(), true),
            };
            if bridge_failed && (BRIDGE_BASE..BRIDGE_BASE + 0x2_0000).contains(&a) {
                out.push(OpResult { err: PL0_ERR_DATA, bytes_ok: 0, data: vec![0; if write { 0 } else { len }] });
                continue;
            }
            if let Some(r) = self.hsm_access(&op) {
                bridge_failed |= !r.ok();
                out.push(r);
                continue;
            }
            if let Some(r) = self.gtm_access(&op) {
                out.push(r);
                continue;
            }
            match op {
                BatchOp::Read { addr, len, addr_map } => {
                    let mut data = vec![0u8; len];
                    // A read into unmapped memory fails there; TAS reports the bytes read before.
                    let end = addr + len as u64;
                    let ok = if addr_map == 0 && addr < SIM_UNMAPPED.end as u64 && end > SIM_UNMAPPED.start as u64 {
                        (SIM_UNMAPPED.start as u64).saturating_sub(addr) as usize
                    } else {
                        len
                    };
                    if addr_map == 0 {
                        self.reads.push((addr as u32, len));
                        // One register read per word, so side effects happen once.
                        let mut k = 0;
                        while k < ok {
                            let a = addr as u32 + k as u32;
                            let base = a & !3;
                            let w = self.read32(base).to_le_bytes();
                            let start = (a - base) as usize;
                            let n = (4 - start).min(ok - k);
                            data[k..k + n].copy_from_slice(&w[start..start + n]);
                            k += n;
                        }
                    }
                    let err = if ok < len { PL0_ERR_DATA } else { PL0_ERR_NO_ERROR };
                    out.push(OpResult { err, bytes_ok: ok, data });
                }
                BatchOp::Write { addr, data, addr_map } => {
                    if addr_map == 0 && !self.write_allowed(addr as u32) {
                        self.violations.push(addr as u32);
                        out.push(OpResult { err: PL0_ERR_DATA, bytes_ok: 0, data: Vec::new() });
                        continue;
                    }
                    if addr_map == 0 {
                        for (k, chunk) in data.chunks(4).enumerate() {
                            let mut w = [0u8; 4];
                            w[..chunk.len()].copy_from_slice(chunk);
                            self.write32(addr as u32 + 4 * k as u32, u32::from_le_bytes(w));
                        }
                    }
                    out.push(OpResult { err: PL0_ERR_NO_ERROR, bytes_ok: data.len(), data: Vec::new() });
                }
            }
        }
        Ok(out)
    }

    fn assert_srst(&mut self) -> Result<()> {
        self.srst = true;
        Ok(())
    }

    fn deassert_srst(&mut self, halt_after_reset: bool) -> Result<()> {
        if self.srst {
            self.srst = false;
            // A power-on class reset: clears OCS (STM suspend included), as on the chip.
            self.power_on_reset(halt_after_reset);
            // Wired to RSTB: the FS26 restarts INIT_FS (a reset is due if the core is still halted
            // when it ends); unwired it does not see the debugger's reset.
            if let Some(f) = self.fs26.as_mut() {
                f.halted_since = None;
                if f.wired && !f.debug_mode {
                    f.init_fs_until = Some(Instant::now() + f.init_fs);
                }
            }
            // With a debugger connected the HSM boot ROM halts at its first user instruction
            // (breakpoint in FP_COMP0); the debug units are reset.
            let debug_enabled = self.hsm.debug_enabled;
            self.hsm = SimHsm::new();
            self.hsm.debug_enabled = debug_enabled;
            // The GTM is disabled after a reset until the application enables it and loads the MCS programs.
            self.gtm = SimGtm::new();
            if halt_after_reset {
                self.hsm.mem.insert(FP_CTRL, 1);
                self.hsm.mem.insert(fp_comp(0), 0x0002_85d0 | 1 << 30 | 1);
                self.hsm.mem.insert(fp_comph(0), 4);
                self.hsm.halt(DFSR_BKPT);
            }
        }
        Ok(())
    }

    fn take_resets(&mut self) -> u64 {
        std::mem::take(&mut self.pending_resets)
    }

    fn health(&self) -> Option<LinkHealth> {
        self.health
    }

    fn set_speed_khz(&mut self, _khz: u32) -> Result<()> {
        Ok(())
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
