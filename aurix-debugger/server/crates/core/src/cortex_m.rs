//! Cortex-M3 target: the AURIX TC3xx HSM, debugged through the HSM bridge.
//!
//! The HSM address space is visible through a 64 KB window in the host
//! address space (bridge + 0x10000): DBGBASE (bridge + 0x1010) holds the upper
//! 16 bits of the HSM address, the offset in the window the lower 16 bits
//! (HSM TS, 9.2.5). Only word access to word-aligned addresses is allowed, and
//! the window answers with bus errors unless the HSM firmware enabled HSM
//! debugging (DBGCTRL.HSM).
//!
//! On top of that the standard ARMv7-M debug registers are used: DHCSR for
//! run control, DCRSR/DCRDR for core registers, the FPB (6 comparators with
//! the HSM's FP_COMPHx extension for addresses above 512 MB) for breakpoints
//! and the DWT (4 comparators) for watchpoints.
//!
//! HSM memories are read through the HSM's unified cache. Before the debugger
//! reads them, the HSM is halted and the cache is cleaned, so no dirty cache
//! block is written back to host memory during a debug access (HSM TS,
//! 12.3.6). Host memory (PFLASH code, host RAM) has the same addresses in the
//! HSM and is always accessed through the host address, never the window.

use std::time::{Duration, Instant};

use tas::Batch;

use crate::adapter::{check, Io};
use crate::arch::*;
use crate::error::{other, Error, Result};
use crate::log_info;
use crate::target::{DebugReason, PollOps, RunCfg, RunMode, TargetState, Transition};

/// Default HSM bridge base (Bridge_EXT, TC3xx).
pub const BRIDGE_BASE: u32 = 0xF004_0000;
const DBGBASE_OFF: u32 = 0x1010;
const WINDOW_OFF: u32 = 0x1_0000;

pub const CPUID: u32 = 0xE000_ED00;
pub const DFSR: u32 = 0xE000_ED30;
pub const DHCSR: u32 = 0xE000_EDF0;
pub const DCRSR: u32 = 0xE000_EDF4;
pub const DCRDR: u32 = 0xE000_EDF8;
pub const DEMCR: u32 = 0xE000_EDFC;
pub const FP_CTRL: u32 = 0xE000_2000;
pub const DWT_CTRL: u32 = 0xE000_1000;
pub const CACHE_CONFIG: u32 = 0xE008_A000;
pub const CACHE_SC: u32 = 0xE008_A104;

pub const fn fp_comp(n: usize) -> u32 {
    0xE000_2008 + 4 * n as u32
}
/// HSM extension: bits 31:29 of comparator `n`'s address.
pub const fn fp_comph(n: usize) -> u32 {
    0xE000_2028 + 4 * n as u32
}
pub const fn dwt_comp(n: usize) -> u32 {
    0xE000_1020 + 16 * n as u32
}
pub const fn dwt_mask(n: usize) -> u32 {
    0xE000_1024 + 16 * n as u32
}
pub const fn dwt_function(n: usize) -> u32 {
    0xE000_1028 + 16 * n as u32
}

pub const DHCSR_KEY: u32 = 0xA05F << 16;
pub const C_DEBUGEN: u32 = 1 << 0;
pub const C_HALT: u32 = 1 << 1;
pub const C_STEP: u32 = 1 << 2;
pub const C_MASKINTS: u32 = 1 << 3;
pub const S_REGRDY: u32 = 1 << 16;
pub const S_HALT: u32 = 1 << 17;
pub const DCRSR_REGWNR: u32 = 1 << 16;
pub const DEMCR_TRCENA: u32 = 1 << 24;
pub const DFSR_HALTED: u32 = 1 << 0;
pub const DFSR_BKPT: u32 = 1 << 1;
pub const DFSR_DWTTRAP: u32 = 1 << 2;
pub const DFSR_ALL: u32 = 0x1F;
pub const FP_CTRL_KEY_ENABLE: u32 = 0x3;

/// Breakpoint slots are `0..`, watchpoint slots `WP_SLOT_BASE..`.
const WP_SLOT_BASE: u8 = 16;
/// FPB and DWT sizes of the TC3xx HSM (HSM TS, 12.1), used until examined.
const DEFAULT_FPB: usize = 6;
const DEFAULT_DWT: usize = 4;
const REG_TIMEOUT: Duration = Duration::from_millis(100);

/// HSM address ranges that are only visible through the debug window: boot ROM,
/// local RAM (with its bit-band alias) and the HSM peripherals and debug units.
pub fn hsm_private(addr: u32) -> bool {
    addr < 0x1000 || (0x2000_0000..0x2400_0000).contains(&addr) || (0xE000_0000..0xF000_0000).contains(&addr)
}

/// HSM memories read through the HSM cache (cleaned before debug access).
fn cached_memory(addr: u32) -> bool {
    addr < 0x1000 || (0x2000_0000..0x2400_0000).contains(&addr)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Watch {
    pub addr: u32,
    pub len: u32,
    /// Store = 1, load = 2 (as TriCore triggers).
    pub access: u32,
}

pub struct CortexM {
    pub coreid: u32,
    /// Bridge base (`-dbgbase`), default [`BRIDGE_BASE`].
    pub dbgbase: Option<u32>,
    pub ocmts: Option<String>,
    pub state: TargetState,
    pub reason: DebugReason,
    pub examined: bool,
    pub run: RunCfg,
    pub dhcsr: u32,
    pub dfsr: u32,
    /// Breakpoint address per FPB comparator.
    pub fpb: Vec<Option<u32>>,
    pub dwt: Vec<Option<Watch>>,
    cache: Vec<Option<u32>>,
    /// The HSM cache was cleaned during the current halt.
    cache_clean: bool,
    /// C_MASKINTS set by the debugger for the current run.
    maskints: bool,
}

impl Default for CortexM {
    fn default() -> Self {
        Self::new()
    }
}

impl CortexM {
    pub fn new() -> Self {
        CortexM {
            coreid: 0,
            dbgbase: None,
            ocmts: None,
            state: TargetState::Unknown,
            reason: DebugReason::Undefined,
            examined: false,
            run: RunCfg::default(),
            dhcsr: 0,
            dfsr: 0,
            fpb: vec![None; DEFAULT_FPB],
            dwt: vec![None; DEFAULT_DWT],
            cache: vec![None; CM_REGS.len()],
            cache_clean: false,
            maskints: false,
        }
    }

    fn bridge(&self) -> u32 {
        self.dbgbase.unwrap_or(BRIDGE_BASE)
    }

    fn dbgbase_reg(&self) -> u32 {
        self.bridge() + DBGBASE_OFF
    }

    /// Host address of an HSM address in the debug window.
    fn window(&self, addr: u32) -> u32 {
        self.bridge() + WINDOW_OFF + (addr & 0xFFFF)
    }

    // --- Window access ------------------------------------------------------------------------

    /// Queues word accesses to HSM addresses: DBGBASE is written before the first access
    /// and whenever the upper 16 bits change. Returns the handle of each access.
    fn queue(&self, b: &mut Batch, ops: &[(u32, Option<u32>)]) -> Vec<usize> {
        let mut base = None;
        ops.iter()
            .map(|&(addr, value)| {
                let hi = addr & 0xFFFF_0000;
                if base != Some(hi) {
                    b.write32(self.dbgbase_reg() as u64, hi);
                    base = Some(hi);
                }
                match value {
                    Some(v) => b.write32(self.window(addr) as u64, v).0,
                    None => b.read32(self.window(addr) as u64).0,
                }
            })
            .collect()
    }

    fn run_ops(&self, io: &mut Io, ops: &[(u32, Option<u32>)]) -> Result<Vec<u32>> {
        let mut b = Batch::new();
        let handles = self.queue(&mut b, ops);
        let r = io.batch(&b)?;
        // DBGBASE writes first: a bus error there means HSM debugging is not enabled.
        for (i, res) in r.iter().enumerate() {
            if !handles.contains(&i) && !res.ok() {
                return Err(other(format!(
                    "HSM debug access not available (DBGBASE write failed: {}); HSM debugging must be enabled by the HSM firmware (DBGCTRL.HSM)",
                    tas::pl_err_name(res.err)
                )));
            }
        }
        handles
            .iter()
            .zip(ops)
            .map(|(&h, &(addr, value))| {
                check(&r[h], self.window(addr) as u64, value.is_some()).map_err(|e| match e {
                    Error::Memory { write, detail, .. } => Error::Memory { addr: addr as u64, write, detail: format!("{detail} (HSM address)") },
                    e => e,
                })?;
                Ok(if value.is_some() { 0 } else { r[h].u32() })
            })
            .collect()
    }

    pub fn read32(&self, io: &mut Io, addr: u32) -> Result<u32> {
        Ok(self.run_ops(io, &[(addr, None)])?[0])
    }

    pub fn read32_many(&self, io: &mut Io, addrs: &[u32]) -> Result<Vec<u32>> {
        let ops: Vec<(u32, Option<u32>)> = addrs.iter().map(|&a| (a, None)).collect();
        self.run_ops(io, &ops)
    }

    pub fn write32_seq(&self, io: &mut Io, writes: &[(u32, u32)]) -> Result<()> {
        let ops: Vec<(u32, Option<u32>)> = writes.iter().map(|&(a, v)| (a, Some(v))).collect();
        self.run_ops(io, &ops).map(|_| ())
    }

    pub fn write32(&self, io: &mut Io, addr: u32, value: u32) -> Result<()> {
        self.write32_seq(io, &[(addr, value)])
    }

    /// Cleans the HSM cache once per halt before HSM memories are accessed.
    fn prepare_memory_access(&mut self, io: &mut Io, addr: u32, len: usize) -> Result<()> {
        let end = addr.saturating_add(len.saturating_sub(1) as u32);
        if !cached_memory(addr) && !cached_memory(end) {
            return Ok(());
        }
        if self.state != TargetState::Halted {
            return Err(other(format!(
                "HSM memory at 0x{addr:08x} can only be accessed while the HSM is halted (the HSM cache must not be cleaned while it runs)"
            )));
        }
        if !self.cache_clean {
            let config = self.read32(io, CACHE_CONFIG)?;
            let sets = 1u32 << ((config >> 16) & 0xFF).min(6);
            let writes: Vec<(u32, u32)> = (0..sets).map(|s| (CACHE_SC, s << 4)).collect();
            self.write32_seq(io, &writes)?;
            self.cache_clean = true;
        }
        Ok(())
    }

    /// Reads HSM-private memory through the window (word access).
    pub fn read_memory(&mut self, io: &mut Io, addr: u32, len: usize) -> Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        self.prepare_memory_access(io, addr, len)?;
        let start = addr & !3;
        let end = addr.checked_add(len as u32 - 1).ok_or_else(|| other("address range overflow"))? | 3;
        let words: Vec<u32> = (start..=end).step_by(4).collect();
        let vals = self.read32_many(io, &words)?;
        let bytes: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()).collect();
        let off = (addr - start) as usize;
        Ok(bytes[off..off + len].to_vec())
    }

    /// Writes HSM-private memory through the window (word access, partial words merged).
    pub fn write_memory(&mut self, io: &mut Io, addr: u32, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.prepare_memory_access(io, addr, data.len())?;
        let start = addr & !3;
        let off = (addr - start) as usize;
        let total = (off + data.len() + 3) & !3;
        let mut bytes = if off != 0 || total != off + data.len() {
            self.read_memory(io, start, total)?
        } else {
            vec![0; total]
        };
        bytes[off..off + data.len()].copy_from_slice(data);
        let writes: Vec<(u32, u32)> =
            bytes.chunks(4).enumerate().map(|(i, w)| (start + 4 * i as u32, u32::from_le_bytes([w[0], w[1], w[2], w[3]]))).collect();
        self.write32_seq(io, &writes)
    }

    // --- Target -------------------------------------------------------------------------------

    pub fn invalidate(&mut self) {
        self.cache.iter_mut().for_each(|c| *c = None);
    }

    /// Checks the debug access, reads the debug unit sizes and enables debugging.
    pub fn examine(&mut self, io: &mut Io) -> Result<()> {
        let v = self.read32_many(io, &[CPUID, FP_CTRL, DWT_CTRL, DHCSR])?;
        let (cpuid, fp_ctrl, dwt_ctrl, dhcsr) = (v[0], v[1], v[2], v[3]);
        // Part number only: the TC3xx HSM reports implementer 0x49 (Infineon), not 0x41 (ARM).
        if ((cpuid >> 4) & 0xFFF) != 0xC23 {
            return Err(other(format!("Unexpected CPUID 0x{cpuid:08x} (no Cortex-M3)")));
        }
        let num_code = (((fp_ctrl >> 8) & 0x70) | ((fp_ctrl >> 4) & 0xF)) as usize;
        let num_dwt = (dwt_ctrl >> 28) as usize;
        if !self.examined {
            log_info!(
                "Cortex-M3 r{}p{} found (CPUID 0x{cpuid:08x}), {num_code} breakpoints, {num_dwt} watchpoints",
                (cpuid >> 20) & 0xF,
                cpuid & 0xF
            );
        }
        self.fpb.resize(num_code, None);
        self.dwt.resize(num_dwt, None);
        // Keep C_HALT (a halted HSM stays halted), enable debugging.
        self.write32_seq(io, &[(DHCSR, DHCSR_KEY | (dhcsr & (C_HALT | C_MASKINTS)) | C_DEBUGEN)])?;
        let demcr = self.read32(io, DEMCR)?;
        self.write32(io, DEMCR, demcr | DEMCR_TRCENA)?;
        self.invalidate();
        self.cache_clean = false;
        self.maskints = dhcsr & C_MASKINTS != 0;
        self.examined = true;
        Ok(())
    }

    /// Queues the reads needed for a poll.
    pub fn poll_queue(&self, b: &mut Batch) -> PollOps {
        let start = b.len();
        let h = self.queue(b, &[(DHCSR, None), (DFSR, None)]);
        PollOps { all: (start..b.len()).collect(), reads: h }
    }

    pub fn poll(&mut self, io: &mut Io) -> Result<Transition> {
        let v = self.read32_many(io, &[DHCSR, DFSR])?;
        Ok(self.poll_apply(&v))
    }

    /// Updates the run state from DHCSR and DFSR.
    pub fn poll_apply(&mut self, vals: &[u32]) -> Transition {
        self.dhcsr = vals[0];
        self.dfsr = vals[1];
        let halted = self.dhcsr & S_HALT != 0;
        let prev = self.state;
        if halted {
            self.state = TargetState::Halted;
            if prev != TargetState::Halted {
                self.invalidate();
                self.cache_clean = false;
                self.decode_reason();
                return Transition::Halted { debug: prev == TargetState::DebugRunning, boot: false };
            }
        } else {
            self.state = TargetState::Running;
            if prev == TargetState::Halted {
                self.reason = DebugReason::NotHalted;
                self.invalidate();
                return Transition::Running;
            }
        }
        Transition::None
    }

    fn decode_reason(&mut self) {
        if matches!(self.reason, DebugReason::SingleStep | DebugReason::DbgRq) {
            return;
        }
        self.reason = if self.dfsr & DFSR_DWTTRAP != 0 {
            DebugReason::Watchpoint
        } else if self.dfsr & DFSR_BKPT != 0 {
            DebugReason::Breakpoint
        } else {
            DebugReason::DbgRq
        };
    }

    /// Index of the FPB comparator at the halt PC for a breakpoint halt.
    pub fn hit_trigger(&self) -> Option<usize> {
        if self.reason != DebugReason::Breakpoint {
            return None;
        }
        let pc = self.cache[CM_REG_PC]?;
        self.fpb.iter().position(|b| *b == Some(pc & !1))
    }

    fn dhcsr_value(&self, halt: bool, step: bool, maskints: bool) -> u32 {
        DHCSR_KEY | C_DEBUGEN | if halt { C_HALT } else { 0 } | if step { C_STEP } else { 0 } | if maskints { C_MASKINTS } else { 0 }
    }

    pub fn halt(&mut self, io: &mut Io) -> Result<()> {
        if self.state == TargetState::Halted {
            return Ok(());
        }
        if self.run.tl1_halt {
            // Also stop the other TL1 suspend targets (TriCore cores), as a TriCore halt does.
            io.write32_seq(&[(CBS_TLC, 0x30), (CBS_TLC, 0x0)])?;
        }
        self.write32(io, DHCSR, self.dhcsr_value(true, false, self.maskints))?;
        self.reason = DebugReason::DbgRq;
        Ok(())
    }

    /// Sets the PC, clears the sticky halt reasons and C_MASKINTS for the run (only
    /// changed while halted).
    fn prepare_run(&mut self, io: &mut Io, addr: Option<u32>, maskints: bool) -> Result<()> {
        if let Some(a) = addr {
            self.write_reg(io, CM_REG_PC, a)?;
        }
        let mut w = vec![(DFSR, DFSR_ALL)];
        if maskints != self.maskints {
            w.push((DHCSR, self.dhcsr_value(true, false, maskints)));
        }
        self.write32_seq(io, &w)?;
        self.maskints = maskints;
        Ok(())
    }

    pub fn resume(&mut self, io: &mut Io, addr: Option<u32>, mode: RunMode) -> Result<()> {
        if self.state == TargetState::Running {
            return Ok(());
        }
        let maskints = self.run.irq_mask.applies(mode);
        self.prepare_run(io, addr, maskints)?;
        self.write32(io, DHCSR, self.dhcsr_value(false, false, maskints))?;
        self.invalidate();
        self.state = TargetState::Running;
        self.reason = DebugReason::NotHalted;
        Ok(())
    }

    /// Executes one instruction (C_STEP). Returns whether the core halted again.
    pub fn step(&mut self, io: &mut Io, addr: Option<u32>, mode: RunMode) -> Result<bool> {
        if self.state != TargetState::Halted {
            return Err(Error::NotHalted(String::new()));
        }
        let maskints = self.run.irq_mask.applies(mode);
        self.prepare_run(io, addr, maskints)?;
        let pc = self.read_reg(io, CM_REG_PC)? & !1;
        // A breakpoint on the current instruction would halt again before executing it.
        let masked: Vec<usize> = (0..self.fpb.len()).filter(|&i| self.fpb[i] == Some(pc)).collect();
        let mut w: Vec<(u32, u32)> = masked.iter().map(|&i| (fp_comp(i), 0)).collect();
        w.push((DHCSR, self.dhcsr_value(false, true, maskints)));
        self.write32_seq(io, &w)?;
        self.invalidate();
        self.state = TargetState::Running;
        self.reason = DebugReason::NotHalted;

        let t0 = Instant::now();
        let mut halted = false;
        while t0.elapsed() < Duration::from_millis(500) {
            let d = self.read32(io, DHCSR)?;
            if d & S_HALT != 0 {
                self.dhcsr = d;
                halted = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        // C_STEP stays set: clear it so a later resume runs freely.
        let mut w = vec![(DHCSR, self.dhcsr_value(halted, false, maskints))];
        for &i in &masked {
            w.extend(self.fpb_writes(i));
        }
        self.write32_seq(io, &w)?;
        if halted {
            self.state = TargetState::Halted;
            self.reason = DebugReason::SingleStep;
            self.cache_clean = false;
        }
        Ok(halted)
    }

    pub fn irq_masked(&self) -> bool {
        self.maskints
    }

    /// Clears C_MASKINTS (it can only be changed while the core is halted).
    pub fn unmask_irq(&mut self, io: &mut Io) -> Result<()> {
        if self.maskints && self.state == TargetState::Halted {
            self.write32(io, DHCSR, self.dhcsr_value(true, false, false))?;
        }
        self.maskints = false;
        Ok(())
    }

    // --- Registers ----------------------------------------------------------------------------

    fn require_halted(&self) -> Result<()> {
        if self.state == TargetState::Halted {
            Ok(())
        } else {
            Err(Error::NotHalted(String::new()))
        }
    }

    /// Value of a register from the raw DCRSR selector value.
    fn extract(def: &RegDef, raw: u32) -> u32 {
        match def.offset >> 8 {
            0 => raw,
            n => (raw >> (8 * (n as u32 - 1))) & 0xFF,
        }
    }

    fn read_core_reg(&self, io: &mut Io, sel: u16) -> Result<u32> {
        let t0 = Instant::now();
        loop {
            let v = self.run_ops(io, &[(DCRSR, Some(sel as u32)), (DHCSR, None), (DCRDR, None)])?;
            if v[1] & S_REGRDY != 0 {
                return Ok(v[2]);
            }
            if t0.elapsed() > REG_TIMEOUT {
                return Err(other(format!("HSM register {sel} not ready (DHCSR.S_REGRDY)")));
            }
        }
    }

    pub fn read_reg(&mut self, io: &mut Io, idx: usize) -> Result<u32> {
        Ok(self.read_regs(io, &[idx])?[0])
    }

    /// Reads registers: core registers need a halted core, debug registers are memory mapped.
    pub fn read_regs(&mut self, io: &mut Io, idxs: &[usize]) -> Result<Vec<u32>> {
        let mut out = Vec::with_capacity(idxs.len());
        // Raw values per DCRSR selector: the byte fields of selector 20 share one read.
        let mut raw_by_sel: Vec<(u16, u32)> = Vec::new();
        for &i in idxs {
            let def = CM_REGS.get(i).ok_or_else(|| other(format!("invalid register {i}")))?;
            let v = if def.group == RegGroup::Debug {
                self.read32(io, 0xE000_0000 | def.offset as u32)?
            } else {
                self.require_halted()?;
                match self.cache[i] {
                    Some(v) => v,
                    None => {
                        let sel = def.offset & 0xFF;
                        let raw = match raw_by_sel.iter().find(|(s, _)| *s == sel) {
                            Some(&(_, r)) => r,
                            None => {
                                let r = self.read_core_reg(io, sel)?;
                                raw_by_sel.push((sel, r));
                                r
                            }
                        };
                        let v = Self::extract(def, raw);
                        self.cache[i] = Some(v);
                        v
                    }
                }
            };
            out.push(v);
        }
        Ok(out)
    }

    pub fn write_reg(&mut self, io: &mut Io, idx: usize, value: u32) -> Result<()> {
        let def = *CM_REGS.get(idx).ok_or_else(|| other(format!("invalid register {idx}")))?;
        if def.group == RegGroup::Debug {
            return self.write32(io, 0xE000_0000 | def.offset as u32, value);
        }
        self.require_halted()?;
        let sel = def.offset & 0xFF;
        let raw = match def.offset >> 8 {
            0 => value,
            n => {
                let shift = 8 * (n as u32 - 1);
                let cur = self.read_core_reg(io, sel)?;
                (cur & !(0xFF << shift)) | ((value & 0xFF) << shift)
            }
        };
        let raw = if idx == CM_REG_PC { raw & !1 } else { raw };
        let t0 = Instant::now();
        loop {
            let v = self.run_ops(io, &[(DCRDR, Some(raw)), (DCRSR, Some(sel as u32 | DCRSR_REGWNR)), (DHCSR, None)])?;
            if v[2] & S_REGRDY != 0 {
                break;
            }
            if t0.elapsed() > REG_TIMEOUT {
                return Err(other(format!("HSM register {} not written (DHCSR.S_REGRDY)", def.name)));
            }
        }
        // Fields of the combined register and SP (banked) change others: read them again.
        if def.offset >> 8 != 0 || matches!(sel, 13 | 17 | 18 | 20) {
            self.invalidate();
        } else {
            self.cache[idx] = Some(if idx == CM_REG_PC { value & !1 } else { value });
        }
        Ok(())
    }

    // --- Breakpoints and watchpoints ----------------------------------------------------------

    fn fpb_writes(&self, n: usize) -> Vec<(u32, u32)> {
        match self.fpb[n] {
            Some(addr) => {
                let replace = if addr & 2 == 0 { 1 << 30 } else { 2 << 30 };
                vec![(FP_CTRL, FP_CTRL_KEY_ENABLE), (fp_comph(n), addr >> 29), (fp_comp(n), (addr & 0x1FFF_FFFC) | replace | 1)]
            }
            None => vec![(fp_comp(n), 0), (fp_comph(n), 0)],
        }
    }

    fn dwt_writes(&self, n: usize) -> Vec<(u32, u32)> {
        match self.dwt[n] {
            Some(w) => {
                let bits = w.len.max(1).next_power_of_two().trailing_zeros();
                let function = match w.access {
                    1 => 6,
                    2 => 5,
                    _ => 7,
                };
                vec![(dwt_comp(n), w.addr & !((1u32 << bits) - 1)), (dwt_mask(n), bits), (dwt_function(n), function)]
            }
            None => vec![(dwt_function(n), 0)],
        }
    }

    /// Hardware breakpoint on a Thumb instruction. Comparators are used from the
    /// highest down: the boot ROM uses FP_COMP0 for its halt-after-reset breakpoint.
    pub fn add_breakpoint(&mut self, io: &mut Io, addr: u32, len: u32) -> Result<u8> {
        if len > 4 {
            return Err(Error::Resource("Breakpoint ranges are not supported on Cortex-M".into()));
        }
        let addr = addr & !1;
        let slot = (0..self.fpb.len())
            .rev()
            .find(|&i| self.fpb[i].is_none())
            .ok_or_else(|| Error::Resource("No hardware breakpoint available".into()))?;
        self.fpb[slot] = Some(addr);
        if let Err(e) = self.write32_seq(io, &self.fpb_writes(slot)) {
            self.fpb[slot] = None;
            return Err(e);
        }
        Ok(slot as u8)
    }

    pub fn add_watchpoint(&mut self, io: &mut Io, addr: u32, len: u32, access: u32) -> Result<u8> {
        let slot = (0..self.dwt.len())
            .find(|&i| self.dwt[i].is_none())
            .ok_or_else(|| Error::Resource("No hardware watchpoint available".into()))?;
        self.dwt[slot] = Some(Watch { addr, len, access });
        if let Err(e) = self.write32_seq(io, &self.dwt_writes(slot)) {
            self.dwt[slot] = None;
            return Err(e);
        }
        Ok(WP_SLOT_BASE + slot as u8)
    }

    pub fn remove_trigger(&mut self, io: &mut Io, slot: u8) -> Result<()> {
        if slot >= WP_SLOT_BASE {
            let n = (slot - WP_SLOT_BASE) as usize;
            if self.dwt.get(n).is_some_and(|w| w.is_some()) {
                self.dwt[n] = None;
                return self.write32_seq(io, &self.dwt_writes(n));
            }
        } else if self.fpb.get(slot as usize).is_some_and(|b| b.is_some()) {
            self.fpb[slot as usize] = None;
            return self.write32_seq(io, &self.fpb_writes(slot as usize));
        }
        Ok(())
    }

    /// Re-programs all breakpoints and watchpoints (after a reset cleared them).
    pub fn restore_triggers(&self, io: &mut Io) -> Result<()> {
        let mut w = Vec::new();
        for n in (0..self.fpb.len()).filter(|&n| self.fpb[n].is_some()) {
            w.extend(self.fpb_writes(n));
        }
        for n in (0..self.dwt.len()).filter(|&n| self.dwt[n].is_some()) {
            w.extend(self.dwt_writes(n));
        }
        if w.is_empty() {
            return Ok(());
        }
        self.write32_seq(io, &w)
    }

    pub fn assert_reset(&mut self) {
        self.invalidate();
        self.state = TargetState::Reset;
        self.reason = DebugReason::Reset;
        self.cache_clean = false;
        self.maskints = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_ranges() {
        assert!(hsm_private(0x0000_0100));
        assert!(!hsm_private(0x0000_1000));
        assert!(hsm_private(0x2000_8000));
        assert!(hsm_private(0x2200_0000));
        assert!(!hsm_private(0x7000_0000));
        assert!(!hsm_private(0x8002_85d0));
        assert!(hsm_private(0xE000_EDF0));
        assert!(!hsm_private(0xF004_0034));
    }

    #[test]
    fn fpb_encoding() {
        let mut c = CortexM::new();
        // Thumb instruction at an address with bit 1 clear: lower half-word, COMPH holds bits 31:29.
        c.fpb[5] = Some(0x8002_85d0);
        assert_eq!(c.fpb_writes(5), vec![(FP_CTRL, 3), (fp_comph(5), 4), (fp_comp(5), 0x0002_85d0 | 1 << 30 | 1)]);
        c.fpb[4] = Some(0x2000_5b02);
        assert_eq!(c.fpb_writes(4)[2], (fp_comp(4), 0x0000_5b00 | 2 << 30 | 1));
        assert_eq!(c.fpb_writes(4)[1], (fp_comph(4), 1));
        c.fpb[4] = None;
        assert_eq!(c.fpb_writes(4), vec![(fp_comp(4), 0), (fp_comph(4), 0)]);
    }

    #[test]
    fn dwt_encoding() {
        let mut c = CortexM::new();
        c.dwt[0] = Some(Watch { addr: 0x2000_1235, len: 3, access: 1 });
        assert_eq!(c.dwt_writes(0), vec![(dwt_comp(0), 0x2000_1234), (dwt_mask(0), 2), (dwt_function(0), 6)]);
        c.dwt[1] = Some(Watch { addr: 0x2000_1000, len: 4, access: 3 });
        assert_eq!(c.dwt_writes(1)[2], (dwt_function(1), 7));
    }

    #[test]
    fn halt_reasons() {
        let mut c = CortexM::new();
        c.state = TargetState::Running;
        c.reason = DebugReason::NotHalted;
        assert_eq!(c.poll_apply(&[S_HALT, DFSR_BKPT]), Transition::Halted { debug: false, boot: false });
        assert_eq!(c.reason, DebugReason::Breakpoint);
        assert_eq!(c.poll_apply(&[0, 0]), Transition::Running);
        assert_eq!(c.poll_apply(&[S_HALT, DFSR_DWTTRAP | DFSR_HALTED]), Transition::Halted { debug: false, boot: false });
        assert_eq!(c.reason, DebugReason::Watchpoint);
        c.poll_apply(&[0, 0]);
        // A TL1 break-in (external) or a C_HALT request.
        c.poll_apply(&[S_HALT, 1 << 4]);
        assert_eq!(c.reason, DebugReason::DbgRq);
    }

    #[test]
    fn register_fields() {
        let def = CM_REGS.iter().find(|r| r.name == "basepri").unwrap();
        assert_eq!(CortexM::extract(def, 0x0201_8001), 0x80);
        let def = CM_REGS.iter().find(|r| r.name == "control").unwrap();
        assert_eq!(CortexM::extract(def, 0x0201_8001), 0x02);
        assert_eq!(CortexM::extract(&CM_REGS[CM_REG_PC], 0x8002_85d0), 0x8002_85d0);
    }
}
