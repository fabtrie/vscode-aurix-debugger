//! `adapter driver sim`: a small TC37x model for offline tests.
//!
//! Models OSTATE/OEC, CPU_ID, DBGSR halt/resume/SUSP, a running PC and PC
//! triggers (a resume with an armed trigger halts immediately at it).
//! Every 32-bit write is recorded in [`SimAdapter::writes`].
//!
//! The HSM ([`SimHsm`]) is reached through its debug window: DBGBASE selects
//! the upper 16 bits of the HSM address, only aligned word access works and
//! every access fails while HSM debugging is disabled.

use std::any::Any;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use tas::{Batch, BatchOp, OpResult, PL0_ERR_DATA, PL0_ERR_NO_ERROR};

use crate::adapter::{Adapter, AdapterSettings, BatchLimits, DeviceInfo};
use crate::arch::{OEC, OSTATE};
use crate::cortex_m::*;
use crate::error::Result;
use crate::sim_mcs::SimGtm;

const NUM_CORES: usize = 3;

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
    /// SBC whose RSTB sees the debugger reset: resets the device again [`SBC_LOOP_DELAY`] after
    /// each halting reset, this many times (then it is in deep fail-safe).
    pub sbc_loop_resets: u32,
    sbc_reset_at: Option<Instant>,
    pub device_id: u32,
    pub hsm: SimHsm,
    pub gtm: SimGtm,
    initialized: bool,
    srst: bool,
}

/// Delay of a reset of the simulated SBC reset loop after a halting reset.
pub const SBC_LOOP_DELAY: Duration = Duration::from_millis(200);

impl Default for SimAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl SimAdapter {
    pub fn new() -> Self {
        SimAdapter {
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
            sbc_loop_resets: 0,
            sbc_reset_at: None,
            device_id: 0x1020_7083,
            hsm: SimHsm::new(),
            gtm: SimGtm::new(),
            initialized: false,
            srst: false,
        }
    }

    /// Seeds a memory word, for tests that need a known value at an address.
    pub fn set_mem(&mut self, addr: u32, value: u32) {
        self.mem.insert(addr & !3, value);
    }

    /// Simulates an external (watchdog) reset: OCDS disabled, cores running.
    pub fn external_reset(&mut self) {
        self.ostate = 0;
        self.halted = [false; NUM_CORES];
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

    fn resume_core(&mut self, i: usize) {
        self.halted[i] = false;
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
        if self.sbc_reset_at.is_some_and(|t| Instant::now() >= t) {
            self.sbc_reset_at = None;
            self.external_reset();
        }
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
                    if addr_map == 0 {
                        self.reads.push((addr as u32, len));
                        // One register read per word, so side effects happen once.
                        let mut k = 0;
                        while k < len {
                            let a = addr as u32 + k as u32;
                            let base = a & !3;
                            let w = self.read32(base).to_le_bytes();
                            let start = (a - base) as usize;
                            let n = (4 - start).min(len - k);
                            data[k..k + n].copy_from_slice(&w[start..start + n]);
                            k += n;
                        }
                    }
                    out.push(OpResult { err: PL0_ERR_NO_ERROR, bytes_ok: len, data });
                }
                BatchOp::Write { addr, data, addr_map } => {
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
            self.ostate = 0;
            self.halted = [halt_after_reset, false, false];
            self.sbc_reset_at = None;
            if halt_after_reset && self.sbc_loop_resets > 0 {
                self.sbc_loop_resets -= 1;
                self.sbc_reset_at = Some(Instant::now() + SBC_LOOP_DELAY);
            }
            self.pc = [0x800A_8000, 0x8010_0000, 0x8020_0000];
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

    fn set_speed_khz(&mut self, _khz: u32) -> Result<()> {
        Ok(())
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
