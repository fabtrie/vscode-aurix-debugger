//! TriCore target (TC1.6.x / AURIX TC3xx) over OCDS memory access.
//!
//! Reimplements the approach of OpenOCD's `src/target/aurix/tricore.c`:
//! DBGSR halt/resume, trigger events for breakpoints, a non-break-before-make
//! PC trigger for single step, OCDS enable after resets.

use std::time::{Duration, Instant};

use tas::Batch;

use crate::adapter::Io;
use crate::arch::*;
use crate::error::{other, Error, Result};
use crate::log_info;
use crate::target::{DebugReason, RunMode, TargetState, Transition};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Trigger {
    pub enable: bool,
    /// Break before make: halt before executing the matching instruction.
    pub bbm: bool,
    /// Code (PC) trigger; otherwise a data address trigger.
    pub pc: bool,
    /// Range with the next trigger's address as upper bound.
    pub range: bool,
    /// Data access bits (store = 1, load = 2).
    pub access: u32,
    pub addr: u32,
}

pub struct Tricore {
    pub coreid: u32,
    pub dbgbase: Option<u32>,
    pub ocmts: Option<String>,
    pub version: Option<Version>,
    pub state: TargetState,
    pub reason: DebugReason,
    pub dbgsr: u32,
    pub triggers: [Trigger; NUM_TRIGGERS],
    cache: Vec<Option<u32>>,
    /// `tricore tl1_halt on`: halt requests also pulse OTGS trigger line 1.
    pub tl1_halt: bool,
    /// `tricore irq_mask`: run modes executed with interrupts masked.
    pub irq_mask: IrqMask,
    /// The application's ICR.IE while the debugger holds it cleared.
    irq_saved_ie: Option<bool>,
    /// PCXI and application IE when the first masked run started.
    irq_run: Option<(u32, bool)>,
    pub examined: bool,
}

impl Default for Tricore {
    fn default() -> Self {
        Self::new()
    }
}

impl Tricore {
    pub fn new() -> Self {
        Tricore {
            coreid: 0,
            dbgbase: None,
            ocmts: None,
            version: None,
            state: TargetState::Unknown,
            reason: DebugReason::Undefined,
            dbgsr: 0,
            triggers: [Trigger::default(); NUM_TRIGGERS],
            cache: vec![None; REGS.len()],
            tl1_halt: false,
            irq_mask: IrqMask::default(),
            irq_saved_ie: None,
            irq_run: None,
            examined: false,
        }
    }

    pub fn csfr(&self, offset: u16) -> u32 {
        self.dbgbase.unwrap_or(0).wrapping_add(0x1_0000 + offset as u32)
    }

    pub fn invalidate(&mut self) {
        self.cache.iter_mut().for_each(|c| *c = None);
    }

    fn is_v18(&self) -> bool {
        self.version == Some(Version::V1_8)
    }

    /// Reads CPU_ID (first time) and sets up the debug instruction event.
    /// OCDS itself is enabled by the caller (shared by all cores).
    pub fn examine(&mut self, io: &mut Io) -> Result<()> {
        if self.dbgbase.is_none() {
            return Err(Error::Usage("-dbgbase is not configured".into()));
        }
        if !self.examined || self.version.is_none() {
            let id = io.read32(self.csfr(CPU_ID))?;
            let v = decode_cpu_id(id).ok_or_else(|| other(format!("Invalid CPU_ID value: 0x{id:08x}")))?;
            if v == Version::V1_8 {
                return Err(other("TriCore 1.8 (TC4x) is not supported yet"));
            }
            if self.version != Some(v) {
                log_info!("Tricore version {} found (CPU_ID 0x{id:08x})", v.name());
            }
            self.version = Some(v);
        }
        self.invalidate();
        // A reset cleared ICR: nothing to restore.
        self.irq_saved_ie = None;
        self.irq_run = None;
        io.write32(self.csfr(SWEVT), if self.is_v18() { SWEVT_HALT_TC18 } else { SWEVT_HALT_TC16 })?;
        self.examined = true;
        Ok(())
    }

    /// Queues the reads needed for a poll.
    pub fn poll_queue(&self, b: &mut Batch) -> (usize, usize) {
        let d = b.read32(self.csfr(DBGSR) as u64);
        let s = b.read32(self.csfr(SYSCON) as u64);
        (d.0, s.0)
    }

    pub fn poll(&mut self, io: &mut Io) -> Result<Transition> {
        let v = io.read32_many(&[self.csfr(DBGSR), self.csfr(SYSCON)])?;
        Ok(self.poll_apply(v[0], v[1]))
    }

    /// Updates the run state from DBGSR and SYSCON (boot halt).
    pub fn poll_apply(&mut self, dbgsr: u32, syscon: u32) -> Transition {
        self.dbgsr = dbgsr;
        let debug_halted = dbgsr & DBGSR_HALTED != 0;
        let boot = !debug_halted && syscon & SYSCON_BHALT != 0;
        let halted = debug_halted || boot;
        let prev = self.state;
        if halted {
            self.state = TargetState::Halted;
            if prev != TargetState::Halted {
                self.invalidate();
                if boot {
                    self.reason = DebugReason::BootHalt;
                } else {
                    self.decode_reason();
                }
                return Transition::Halted { debug: prev == TargetState::DebugRunning, boot };
            }
            if !boot && self.reason == DebugReason::BootHalt {
                // Debug halt requested while in boot halt.
                self.decode_reason();
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
        if self.reason == DebugReason::SingleStep {
            return;
        }
        self.reason = DebugReason::DbgRq;
        if self.dbgsr & DBGSR_HALT_WRITE != 0 {
            self.reason = DebugReason::DbgRq;
        }
        match (self.dbgsr >> 8) & 0x1F {
            EVTSRC_EXTERNAL | EVTSRC_CORE_REGISTER => self.reason = DebugReason::DbgRq,
            EVTSRC_SOFTWARE => self.reason = DebugReason::Breakpoint,
            n @ 16..=23 => {
                let t = self.triggers[(n - EVTSRC_TRIGGER0) as usize];
                self.reason = if t.enable && !t.pc { DebugReason::Watchpoint } else { DebugReason::Breakpoint };
            }
            _ => {}
        }
    }

    /// Index of the trigger that caused the current halt, if any.
    pub fn hit_trigger(&self) -> Option<usize> {
        match (self.dbgsr >> 8) & 0x1F {
            n @ 16..=23 => Some((n - EVTSRC_TRIGGER0) as usize),
            _ => None,
        }
    }

    pub fn halt(&mut self, io: &mut Io) -> Result<()> {
        if self.state == TargetState::Halted {
            return Ok(());
        }
        if self.tl1_halt && !self.is_v18() {
            // A DBGSR halt is not a debug event and pulses no BRKOUT: force TL1
            // directly so TL1 capture-and-hold stops all TL1 suspend targets.
            io.write32_seq(&[(CBS_TLC, 0x30), (CBS_TLC, 0x0)])?;
        }
        io.write32(self.csfr(DBGSR), DBGSR_HALT_REQ)?;
        self.reason = DebugReason::DbgRq;
        Ok(())
    }

    pub fn resume(&mut self, io: &mut Io, addr: Option<u32>, mode: RunMode) -> Result<()> {
        if self.state == TargetState::Running {
            return Ok(());
        }
        if let Some(a) = addr {
            io.write32(self.csfr(PC), a)?;
        }
        self.prepare_irq(io, mode)?;
        if let (Some(ie), None) = (self.irq_saved_ie, self.irq_run) {
            self.irq_run = Some((io.read32(self.csfr(PCXI))?, ie));
        }
        let resume = self.resume_value(io)?;
        io.write32(self.csfr(DBGSR), resume)?;
        self.invalidate();
        self.state = TargetState::Running;
        self.reason = DebugReason::NotHalted;
        Ok(())
    }

    /// Executes one instruction. Uses a non-BBM PC trigger on the current PC,
    /// which halts after the instruction has executed. Returns whether the
    /// core halted again.
    pub fn step(&mut self, io: &mut Io, addr: Option<u32>, mode: RunMode) -> Result<bool> {
        if self.state != TargetState::Halted {
            return Err(Error::NotHalted(String::new()));
        }
        let pc = match addr {
            Some(a) => {
                io.write32(self.csfr(PC), a)?;
                a
            }
            None => self.read_reg(io, REG_PC)?,
        };
        self.prepare_irq(io, mode)?;
        // Instruction and PCXI before a masked step: its effect on the
        // interrupt state is carried over to the application's IE afterwards.
        let irq_before = match self.irq_saved_ie {
            Some(_) => Some((io.read32(pc).ok(), io.read32(self.csfr(PCXI))?)),
            None => None,
        };
        // A breakpoint on the current PC would halt again before executing it.
        let masked: Vec<usize> = (0..NUM_TRIGGERS)
            .filter(|&i| {
                let t = self.triggers[i];
                t.enable && t.pc && t.bbm && !t.range && t.addr == pc
            })
            .collect();
        for &i in &masked {
            io.write32(self.csfr(trxevt(i)), 0)?;
        }
        let slot = (0..NUM_TRIGGERS)
            .rev()
            .find(|&i| !self.triggers[i].enable)
            .ok_or_else(|| Error::Resource("No hardware event available for single step".into()))?;
        let step_trigger = Trigger { enable: true, bbm: false, pc: true, range: false, access: 0, addr: pc };
        let resume = self.resume_value(io)?;
        io.write32_seq(&[
            (self.csfr(trxadr(slot)), pc),
            (self.csfr(trxevt(slot)), self.trigger_value(&step_trigger)),
            (self.csfr(DBGSR), resume),
        ])?;
        self.invalidate();
        self.state = TargetState::Running;
        self.reason = DebugReason::NotHalted;

        let t0 = Instant::now();
        let mut halted = false;
        while t0.elapsed() < Duration::from_millis(500) {
            let d = io.read32(self.csfr(DBGSR))?;
            if d & DBGSR_HALTED != 0 {
                self.dbgsr = d;
                halted = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }

        io.write32_seq(&[(self.csfr(trxevt(slot)), 0), (self.csfr(trxadr(slot)), 0)])?;
        for &i in &masked {
            let v = self.trigger_value(&self.triggers[i]);
            io.write32(self.csfr(trxevt(i)), v)?;
        }
        if halted {
            self.state = TargetState::Halted;
            self.reason = DebugReason::SingleStep;
            if let Some((word, pcxi)) = irq_before {
                self.irq_after_step(io, word, pcxi)?;
            }
        }
        Ok(halted)
    }

    /// DBGSR value that releases the halt without changing the suspend-out
    /// signal: SUSP is set and cleared only by the cfg's event handlers, so a
    /// step keeps the STMs frozen and a resume releases them in resume-end
    /// together with the other cores.
    fn resume_value(&self, io: &mut Io) -> Result<u32> {
        Ok(DBGSR_RESUME | (io.read32(self.csfr(DBGSR))? & DBGSR_SUSP))
    }

    /// Whether the debugger currently holds this core's interrupts masked.
    pub fn irq_masked(&self) -> bool {
        self.irq_saved_ie.is_some()
    }

    /// Masks interrupts for a run in `mode` if `irq_mask` selects it, or
    /// gives the application its interrupt enable back otherwise.
    pub fn prepare_irq(&mut self, io: &mut Io, mode: RunMode) -> Result<()> {
        if !self.irq_mask.applies(mode) {
            return self.unmask_irq(io);
        }
        let icr = io.read32(self.csfr(ICR))?;
        // A masked run may have enabled interrupts itself (enable, rfe, ...).
        let hw_ie = icr & ICR_IE != 0;
        self.irq_saved_ie = Some(hw_ie || self.irq_saved_ie == Some(true));
        if hw_ie {
            io.write32(self.csfr(ICR), icr & !ICR_IE)?;
            self.cache[REG_ICR] = None;
        }
        Ok(())
    }

    /// Restores the application's ICR.IE if the debugger masked it.
    pub fn unmask_irq(&mut self, io: &mut Io) -> Result<()> {
        let Some(ie) = self.irq_saved_ie else {
            return Ok(());
        };
        let v = io.read32_many(&[self.csfr(ICR), self.csfr(PCXI)])?;
        let (icr, pcxi) = (v[0], v[1]);
        let mut w = Vec::new();
        if ie && icr & ICR_IE == 0 {
            w.push((self.csfr(ICR), icr | ICR_IE));
        }
        if let Some((run_pcxi, run_ie)) = self.irq_run {
            // Halted in a context saved during a masked run (e.g. a trap
            // handler): its PIE holds the masked IE, which RFE would restore.
            if pcxi != run_pcxi && (pcxi & PCXI_PIE != 0) != run_ie {
                w.push((self.csfr(PCXI), with_bit(pcxi, PCXI_PIE, run_ie)));
            }
        }
        if !w.is_empty() {
            io.write32_seq(&w)?;
        }
        self.irq_saved_ie = None;
        self.irq_run = None;
        self.cache[REG_ICR] = None;
        self.cache[REG_PCXI] = None;
        Ok(())
    }

    /// Applies the effect of a masked instruction step on the interrupt state.
    fn irq_after_step(&mut self, io: &mut Io, word: Option<u32>, pcxi_before: u32) -> Result<()> {
        let Some(ie) = self.irq_saved_ie else {
            return Ok(());
        };
        let v = io.read32_many(&[self.csfr(ICR), self.csfr(PCXI), self.csfr(PC), self.csfr(BTV)])?;
        let (icr, pcxi, pc, btv) = (v[0], v[1], v[2], v[3]);
        let in_trap_table = pc.wrapping_sub(btv) < TRAP_TABLE_SIZE;
        let fx = irq_step_effect(word, ie, icr & ICR_IE != 0, pcxi != pcxi_before, in_trap_table);
        let mut w = Vec::new();
        if let Some(n) = fx.set_dn {
            // DISABLE Dn saved the masked IE; the application expects its own.
            w.push((self.csfr(REGS[n].offset), ie as u32));
        }
        if fx.patch_pie {
            w.push((self.csfr(PCXI), with_bit(pcxi, PCXI_PIE, ie)));
        }
        if icr & ICR_IE != 0 {
            w.push((self.csfr(ICR), icr & !ICR_IE));
        }
        if !w.is_empty() {
            io.write32_seq(&w)?;
        }
        self.irq_saved_ie = Some(fx.ie);
        self.invalidate();
        Ok(())
    }

    /// Register value as the application sees it: ICR with its own IE while
    /// the debugger masks interrupts.
    fn app_value(&self, idx: usize, v: u32) -> u32 {
        match (idx, self.irq_saved_ie) {
            (REG_ICR, Some(true)) => v | ICR_IE,
            _ => v,
        }
    }

    pub fn read_reg(&mut self, io: &mut Io, idx: usize) -> Result<u32> {
        if self.state == TargetState::Halted {
            if let Some(v) = self.cache[idx] {
                return Ok(self.app_value(idx, v));
            }
        }
        let v = io.read32(self.csfr(REGS[idx].offset))?;
        if self.state == TargetState::Halted {
            self.cache[idx] = Some(v);
        }
        Ok(self.app_value(idx, v))
    }

    /// Reads several registers in one transaction.
    pub fn read_regs(&mut self, io: &mut Io, idxs: &[usize]) -> Result<Vec<u32>> {
        let halted = self.state == TargetState::Halted;
        let missing: Vec<usize> = idxs.iter().copied().filter(|&i| !halted || self.cache[i].is_none()).collect();
        if !missing.is_empty() {
            let addrs: Vec<u32> = missing.iter().map(|&i| self.csfr(REGS[i].offset)).collect();
            let vals = io.read32_many(&addrs)?;
            for (&i, v) in missing.iter().zip(&vals) {
                self.cache[i] = Some(*v);
            }
            if !halted {
                let out = idxs.iter().map(|&i| self.app_value(i, self.cache[i].unwrap_or(0))).collect();
                self.invalidate();
                return Ok(out);
            }
        }
        Ok(idxs.iter().map(|&i| self.app_value(i, self.cache[i].unwrap_or(0))).collect())
    }

    pub fn write_reg(&mut self, io: &mut Io, idx: usize, value: u32) -> Result<()> {
        let mut value = value;
        if idx == REG_ICR && self.irq_saved_ie.is_some() {
            // Interrupts stay masked; the write sets the application's IE.
            self.irq_saved_ie = Some(value & ICR_IE != 0);
            value &= !ICR_IE;
        }
        io.write32(self.csfr(REGS[idx].offset), value)?;
        self.cache[idx] = if self.state == TargetState::Halted { Some(value) } else { None };
        Ok(())
    }

    pub fn trigger_value(&self, t: &Trigger) -> u32 {
        (t.bbm as u32) << 3
            | (t.pc as u32) << 12
            | (t.range as u32) << 13
            | t.access << 27
            | if self.is_v18() { 1 } else { 2 }
    }

    pub fn free_triggers(&self) -> usize {
        self.triggers.iter().filter(|t| !t.enable).count()
    }

    fn find_slot(&self, range: bool) -> Option<usize> {
        (0..NUM_TRIGGERS).find(|&i| {
            if range {
                i % 2 == 0 && !self.triggers[i].enable && !self.triggers[i + 1].enable
            } else {
                !self.triggers[i].enable
            }
        })
    }

    fn program(&self, io: &mut Io, slot: usize) -> Result<()> {
        let t = self.triggers[slot];
        if t.enable {
            let mut w = Vec::with_capacity(3);
            if t.range {
                w.push((self.csfr(trxadr(slot + 1)), self.triggers[slot + 1].addr));
            }
            w.push((self.csfr(trxadr(slot)), t.addr));
            w.push((self.csfr(trxevt(slot)), self.trigger_value(&t)));
            io.write32_seq(&w)
        } else {
            let mut w = vec![(self.csfr(trxevt(slot)), 0), (self.csfr(trxadr(slot)), 0)];
            if t.range && slot + 1 < NUM_TRIGGERS {
                w.push((self.csfr(trxevt(slot + 1)), 0));
                w.push((self.csfr(trxadr(slot + 1)), 0));
            }
            io.write32_seq(&w)
        }
    }

    /// Hardware breakpoint; `len > 4` uses a range over two triggers.
    pub fn add_breakpoint(&mut self, io: &mut Io, addr: u32, len: u32) -> Result<u8> {
        let range = len > 4;
        let slot = self
            .find_slot(range)
            .ok_or_else(|| Error::Resource("No hardware event available for breakpoint".into()))?;
        self.triggers[slot] = Trigger { enable: true, bbm: true, pc: true, range, access: 0, addr };
        if range {
            self.triggers[slot + 1] = Trigger { enable: true, pc: true, addr: addr.wrapping_add(len), ..Default::default() };
        }
        if let Err(e) = self.program(io, slot) {
            self.release(slot);
            return Err(e);
        }
        Ok(slot as u8)
    }

    pub fn add_watchpoint(&mut self, io: &mut Io, addr: u32, len: u32, access: u32) -> Result<u8> {
        let range = len > 4;
        let slot = self
            .find_slot(range)
            .ok_or_else(|| Error::Resource("No hardware event available for watchpoint".into()))?;
        self.triggers[slot] = Trigger { enable: true, bbm: false, pc: false, range, access, addr };
        if range {
            self.triggers[slot + 1] = Trigger { enable: true, access, addr: addr.wrapping_add(len), ..Default::default() };
        }
        if let Err(e) = self.program(io, slot) {
            self.release(slot);
            return Err(e);
        }
        Ok(slot as u8)
    }

    fn release(&mut self, slot: usize) {
        let range = self.triggers[slot].range;
        self.triggers[slot].enable = false;
        if range && slot + 1 < NUM_TRIGGERS {
            self.triggers[slot + 1].enable = false;
        }
    }

    pub fn remove_trigger(&mut self, io: &mut Io, slot: u8) -> Result<()> {
        let slot = slot as usize;
        if slot >= NUM_TRIGGERS || !self.triggers[slot].enable {
            return Ok(());
        }
        self.release(slot);
        self.program(io, slot)
    }

    /// Re-programs all enabled triggers (after a reset cleared them).
    pub fn restore_triggers(&self, io: &mut Io) -> Result<()> {
        for i in 0..NUM_TRIGGERS {
            let t = self.triggers[i];
            let is_range_upper = i > 0 && self.triggers[i - 1].enable && self.triggers[i - 1].range;
            if t.enable && !is_range_upper {
                self.program(io, i)?;
            }
        }
        Ok(())
    }

    pub fn assert_reset(&mut self) {
        self.invalidate();
        self.state = TargetState::Reset;
        self.reason = DebugReason::Reset;
        self.irq_saved_ie = None;
        self.irq_run = None;
    }
}

fn with_bit(v: u32, bit: u32, on: bool) -> u32 {
    if on {
        v | bit
    } else {
        v & !bit
    }
}

/// `tricore irq_mask`: run modes executed with interrupts masked (ICR.IE = 0),
/// like winIDEA's DisableIRQ_Step / DisableIRQ_StepOver / DisableIRQ_RunUntilReturn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IrqMask {
    pub step: bool,
    pub step_over: bool,
    pub step_out: bool,
}

impl IrqMask {
    pub fn applies(&self, mode: RunMode) -> bool {
        match mode {
            RunMode::Continue => false,
            RunMode::Step => self.step,
            RunMode::StepOver => self.step_over,
            RunMode::StepOut => self.step_out,
        }
    }

    /// Parses `off`, `on`, or a list of `step`, `step_over`, `step_out`.
    pub fn parse(words: &[&str]) -> std::result::Result<IrqMask, String> {
        let mut m = IrqMask::default();
        for w in words {
            match *w {
                "off" => m = IrqMask::default(),
                "on" => m = IrqMask { step: true, step_over: true, step_out: true },
                "step" => m.step = true,
                "step_over" => m.step_over = true,
                "step_out" => m.step_out = true,
                other => return Err(format!("unknown run mode '{other}' (off, on, step, step_over, step_out)")),
            }
        }
        Ok(m)
    }

    pub fn describe(&self) -> String {
        let names: Vec<&str> = [(self.step, "step"), (self.step_over, "step_over"), (self.step_out, "step_out")]
            .iter()
            .filter(|(on, _)| *on)
            .map(|(_, n)| *n)
            .collect();
        if names.is_empty() {
            "off".into()
        } else {
            names.join(" ")
        }
    }
}

/// Instructions whose effect on ICR.IE is not visible in ICR after a masked step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IrqInsn {
    Disable,
    /// DISABLE Dn: Dn = ICR.IE, then IE = 0.
    DisableTo(usize),
    /// RESTORE Dn: IE = Dn[0].
    Restore,
    /// RFE: IE = PCXI.PIE of the restored context.
    Rfe,
    Bisr,
    Syscall,
    Other,
}

/// Decodes the instruction word read at the PC (16-bit instructions use the low half).
fn decode_irq_insn(word: u32) -> IrqInsn {
    if word & 1 == 0 {
        let w = word as u16;
        return match w {
            0x8000 => IrqInsn::Rfe,
            _ if w & 0xFF == 0xE0 => IrqInsn::Bisr,
            _ => IrqInsn::Other,
        };
    }
    let op2 = (word >> 22) & 0x3F;
    match (word & 0xFF, op2) {
        (0x0D, 0x0D) => IrqInsn::Disable,
        (0x0D, 0x0F) => IrqInsn::DisableTo(((word >> 8) & 0xF) as usize),
        (0x0D, 0x0E) => IrqInsn::Restore,
        (0x0D, 0x07) => IrqInsn::Rfe,
        (0xAD, 0x00) => IrqInsn::Bisr,
        (0xAD, 0x02) => IrqInsn::Syscall,
        _ => IrqInsn::Other,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IrqStepEffect {
    /// The application's IE after the step.
    ie: bool,
    /// A context was saved with the masked IE in PCXI.PIE (trap, BISR): set
    /// it to the application's IE from before the step.
    patch_pie: bool,
    /// DISABLE Dn stored the masked IE: set Dn to the application's IE.
    set_dn: Option<usize>,
}

/// Effect of one instruction stepped with IE cleared by the debugger.
/// `ie`: application IE before; `hw_ie`: ICR.IE after the step; `new_context`:
/// PCXI changed; `in_trap_table`: the PC is in the trap vector table.
fn irq_step_effect(word: Option<u32>, ie: bool, hw_ie: bool, new_context: bool, in_trap_table: bool) -> IrqStepEffect {
    let mut fx = IrqStepEffect { ie: ie || hw_ie, patch_pie: false, set_dn: None };
    match word.map_or(IrqInsn::Other, decode_irq_insn) {
        IrqInsn::Disable => fx.ie = false,
        IrqInsn::DisableTo(n) => {
            fx.ie = false;
            fx.set_dn = Some(n);
        }
        IrqInsn::Restore | IrqInsn::Rfe => fx.ie = hw_ie,
        IrqInsn::Bisr => {
            fx.ie = true;
            fx.patch_pie = new_context;
        }
        IrqInsn::Syscall => {
            fx.ie = false;
            fx.patch_pie = new_context;
        }
        IrqInsn::Other if new_context && in_trap_table => {
            fx.ie = false;
            fx.patch_pie = true;
        }
        IrqInsn::Other => {}
    }
    fx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_interrupt_instructions() {
        // Encodings from tricore-elf-as -mtc162 / objdump.
        assert_eq!(decode_irq_insn(0x0300_000D), IrqInsn::Other); // enable
        assert_eq!(decode_irq_insn(0x0340_000D), IrqInsn::Disable);
        assert_eq!(decode_irq_insn(0x03C0_030D), IrqInsn::DisableTo(3));
        assert_eq!(decode_irq_insn(0x0380_030D), IrqInsn::Restore);
        assert_eq!(decode_irq_insn(0xABCD_8000), IrqInsn::Rfe); // 16-bit, next instruction in the high half
        assert_eq!(decode_irq_insn(0x01C0_000D), IrqInsn::Rfe);
        assert_eq!(decode_irq_insn(0x0000_05E0), IrqInsn::Bisr);
        assert_eq!(decode_irq_insn(0x0012_C0AD), IrqInsn::Bisr);
        assert_eq!(decode_irq_insn(0x0080_10AD), IrqInsn::Syscall);
        assert_eq!(decode_irq_insn(0x0180_000D), IrqInsn::Other); // ret
        assert_eq!(decode_irq_insn(0x0000_9000), IrqInsn::Other); // 16-bit ret
        assert_eq!(decode_irq_insn(0x0140_000D), IrqInsn::Other); // rfm
    }

    #[test]
    fn step_effects_on_application_ie() {
        let fx = |w: u32, ie, hw, ctx, trap| irq_step_effect(Some(w), ie, hw, ctx, trap);
        let nop = 0x0000_0000;
        // Plain instruction and CALL: IE unchanged, PIE of call frames is not used by RET.
        assert_eq!(fx(nop, true, false, false, false), IrqStepEffect { ie: true, patch_pie: false, set_dn: None });
        assert_eq!(fx(0x0000_016D, true, false, true, false).patch_pie, false);
        // enable shows in ICR.
        assert!(fx(0x0300_000D, false, true, false, false).ie);
        assert!(!fx(0x0340_000D, true, false, false, false).ie);
        assert_eq!(fx(0x03C0_030D, true, false, false, false), IrqStepEffect { ie: false, patch_pie: false, set_dn: Some(3) });
        assert!(!fx(0x0380_030D, true, false, false, false).ie);
        assert!(fx(0x0380_030D, false, true, false, false).ie);
        assert!(!fx(0x0000_8000, true, false, true, false).ie);
        assert_eq!(fx(0x0080_10AD, true, false, true, true), IrqStepEffect { ie: false, patch_pie: true, set_dn: None });
        assert_eq!(fx(0x0012_C0AD, false, true, true, false), IrqStepEffect { ie: true, patch_pie: true, set_dn: None });
        // Any other instruction that ends in the trap table saved a context.
        assert_eq!(fx(nop, true, false, true, true), IrqStepEffect { ie: false, patch_pie: true, set_dn: None });
        assert!(irq_step_effect(None, true, false, false, false).ie);
    }

    #[test]
    fn irq_mask_parse_and_describe() {
        assert_eq!(IrqMask::parse(&[]).unwrap().describe(), "off");
        assert_eq!(IrqMask::parse(&["on"]).unwrap().describe(), "step step_over step_out");
        let m = IrqMask::parse(&["step", "step_out"]).unwrap();
        assert!(m.applies(RunMode::Step) && !m.applies(RunMode::StepOver) && m.applies(RunMode::StepOut));
        assert!(!m.applies(RunMode::Continue));
        assert!(IrqMask::parse(&["run_to"]).is_err());
    }
}
