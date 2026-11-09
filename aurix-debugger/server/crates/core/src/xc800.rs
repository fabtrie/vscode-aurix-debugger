//! SCR target: the TC3xx standby controller, an XC800 (8051) core (TC3xx UM 49).
//!
//! Unlike the other cores, the SCR is not debugged over the host bus. Its OCDS sits behind its
//! own debug port, which the SoC DAP reaches once the application started the SCR in an OCDS
//! boot mode (PMSWCR4.SCRCFG = 0x0A/0x0B); halting, stepping, breakpoints and memory go through
//! the adapter's [`ScrDebug`] interface. An adapter without one cannot debug the SCR, and the
//! target says so when it is examined.
//!
//! Addresses are the SCR's virtual address map ([`scr_space`]): code (XRAM) at 0x0000-0xFFFF,
//! IRAM at 0x20000, SFRs from 0x40000. They overlap host addresses, so they mean SCR memory only
//! when accessed through this target.
//!
//! Registers: the values the debug monitor saved when the SCR stopped ([`ScrRegs`]), R0..R7
//! included. Breakpoints are the OCDS's hardware
//! breakpoints; there are no watchpoints. A step over a call runs to a temporary hardware
//! breakpoint behind it.

use isa::xc800;
use tas::Batch;

use crate::adapter::{scr_space, Io, ScrDebug, ScrReg, ScrRegs, ScrStatus, ScrStop};
use crate::arch::{RegDef, XC800_BANK_REG, XC800_REGS, XC800_REG_PC, XC800_SAVED_REG};
use crate::error::{other, Error, Result};
use crate::target::{DebugReason, PollOps, RunCfg, RunMode, TargetState, Transition};

/// The adapter's SCR interface, or why there is none.
fn scr<'a>(io: &'a mut Io) -> Result<&'a mut dyn ScrDebug> {
    let name = io.adapter.driver_name();
    io.adapter.scr_debug().ok_or_else(|| other(format!("the {name} adapter cannot debug the SCR")))
}

/// The saved register a [`XC800_REGS`] entry stands for (`dptr` is two of them).
fn saved_reg(def: &RegDef) -> Option<ScrReg> {
    if def.offset < XC800_SAVED_REG {
        return Some(ScrReg::R((def.offset - XC800_BANK_REG) as u8));
    }
    match def.offset - XC800_SAVED_REG {
        0 => Some(ScrReg::Acc),
        1 => Some(ScrReg::B),
        2 => Some(ScrReg::Sp),
        3 => Some(ScrReg::Psw),
        4 => Some(ScrReg::Dpl),
        5 => Some(ScrReg::Dph),
        6 => Some(ScrReg::Pc),
        _ => None,
    }
}

fn saved_value(r: &ScrRegs, def: &RegDef) -> u32 {
    if def.offset < XC800_SAVED_REG {
        return r.r[(def.offset - XC800_BANK_REG) as usize & 7] as u32;
    }
    match def.offset - XC800_SAVED_REG {
        0 => r.acc as u32,
        1 => r.b as u32,
        2 => r.sp as u32,
        3 => r.psw as u32,
        4 => r.dpl as u32,
        5 => r.dph as u32,
        6 => r.pc as u32,
        _ => (r.dph as u32) << 8 | r.dpl as u32,
    }
}

pub struct Xc800 {
    pub coreid: u32,
    pub dbgbase: Option<u32>,
    pub ocmts: Option<String>,
    pub state: TargetState,
    pub reason: DebugReason,
    pub examined: bool,
    pub run: RunCfg,
    pub pc: u16,
    /// Saved registers, while halted.
    regs: Option<ScrRegs>,
    /// Breakpoint address per hardware slot.
    slots: Vec<Option<u16>>,
    /// Slot of the temporary breakpoint of a step over a call.
    over_slot: Option<usize>,
}

impl Default for Xc800 {
    fn default() -> Self {
        Self::new()
    }
}

impl Xc800 {
    pub fn new() -> Self {
        Xc800 {
            coreid: 0,
            dbgbase: None,
            ocmts: None,
            state: TargetState::Unknown,
            reason: DebugReason::Undefined,
            examined: false,
            run: RunCfg::default(),
            pc: 0,
            regs: None,
            slots: Vec::new(),
            over_slot: None,
        }
    }

    /// The whole virtual address map is SCR memory.
    pub fn owns_address(&self, addr: u32) -> bool {
        addr < scr_space::END
    }

    pub fn invalidate(&mut self) {
        self.regs = None;
    }

    pub fn examine(&mut self, io: &mut Io) -> Result<()> {
        let s = scr(io)?;
        s.attach()?;
        let n = s.hw_breakpoint_slots();
        let st = s.status()?;
        self.slots.resize(n, None);
        self.slots.truncate(n);
        self.over_slot = None;
        self.state = TargetState::Unknown;
        self.apply(st);
        self.examined = true;
        Ok(())
    }

    /// The SCR is polled through [`ScrDebug::status`], not with bus reads in the shared batch
    /// (see [`crate::target::Core::polls_directly`]).
    pub fn poll_queue(&self, _b: &mut Batch) -> PollOps {
        PollOps::default()
    }

    pub fn poll_apply(&mut self, _vals: &[u32]) -> Transition {
        Transition::None
    }

    pub fn poll(&mut self, io: &mut Io) -> Result<Transition> {
        let st = scr(io)?.status()?;
        let tr = self.apply(st);
        if st.halted {
            if let Some(slot) = self.over_slot.take() {
                scr(io)?.clear_hw_breakpoint(slot)?;
                if self.reason == DebugReason::Breakpoint && !self.slots.contains(&Some(self.pc)) {
                    self.reason = DebugReason::SingleStep;
                }
            }
        }
        Ok(tr)
    }

    /// Updates the run state from a status.
    fn apply(&mut self, st: ScrStatus) -> Transition {
        let prev = self.state;
        if st.halted {
            self.pc = st.pc;
            self.state = TargetState::Halted;
            if prev != TargetState::Halted {
                self.invalidate();
                self.reason = match st.reason {
                    ScrStop::Breakpoint => DebugReason::Breakpoint,
                    ScrStop::Step => DebugReason::SingleStep,
                    ScrStop::Request => DebugReason::DbgRq,
                    ScrStop::Other => DebugReason::Undefined,
                };
                return Transition::Halted { debug: false, boot: false };
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

    pub fn hit_trigger(&self) -> Option<usize> {
        if self.reason != DebugReason::Breakpoint {
            return None;
        }
        self.slots.iter().position(|s| *s == Some(self.pc))
    }

    /// Requests the halt; the poll reports it.
    pub fn halt(&mut self, io: &mut Io) -> Result<()> {
        if self.state == TargetState::Halted {
            return Ok(());
        }
        scr(io)?.halt()?;
        self.reason = DebugReason::DbgRq;
        Ok(())
    }

    fn require_halted(&self) -> Result<()> {
        if self.state != TargetState::Halted {
            return Err(Error::NotHalted(String::new()));
        }
        Ok(())
    }

    fn set_pc(&mut self, io: &mut Io, addr: u32) -> Result<()> {
        let pc = u16::try_from(addr).map_err(|_| other(format!("0x{addr:x} is no SCR code address (0..0xFFFF)")))?;
        scr(io)?.write_reg(ScrReg::Pc, pc)?;
        self.pc = pc;
        self.invalidate();
        Ok(())
    }

    fn running(&mut self) {
        self.invalidate();
        self.state = TargetState::Running;
        self.reason = DebugReason::NotHalted;
    }

    pub fn resume(&mut self, io: &mut Io, addr: Option<u32>, _mode: RunMode) -> Result<()> {
        if self.state == TargetState::Running {
            return Ok(());
        }
        self.require_halted()?;
        if let Some(a) = addr {
            self.set_pc(io, a)?;
        }
        scr(io)?.resume()?;
        self.running();
        Ok(())
    }

    /// Executes one instruction. For [`RunMode::StepOver`] a call runs to a temporary breakpoint
    /// behind it instead (false: still running, the poll reports the halt); without a free
    /// breakpoint it is stepped into.
    pub fn step(&mut self, io: &mut Io, addr: Option<u32>, mode: RunMode) -> Result<bool> {
        self.require_halted()?;
        if let Some(a) = addr {
            self.set_pc(io, a)?;
        }
        if mode == RunMode::StepOver {
            let code = scr(io)?.read(scr_space::CODE + self.pc as u32, 3)?;
            let insn = xc800::decode(&code, self.pc as u32);
            let free = self.slots.iter().position(Option::is_none);
            if let (Some(i), Some(slot)) = (insn.filter(|i| i.flow == xc800::Flow::Call), free) {
                let next = (self.pc as u32 + i.len) as u16;
                let s = scr(io)?;
                s.set_hw_breakpoint(slot, next)?;
                if let Err(e) = s.resume() {
                    let _ = s.clear_hw_breakpoint(slot);
                    return Err(e);
                }
                self.over_slot = Some(slot);
                self.running();
                return Ok(false);
            }
        }
        let s = scr(io)?;
        s.step()?;
        let st = s.status()?;
        self.invalidate();
        if st.halted {
            self.pc = st.pc;
            self.state = TargetState::Halted;
            self.reason = DebugReason::SingleStep;
            Ok(true)
        } else {
            self.state = TargetState::Running;
            self.reason = DebugReason::NotHalted;
            Ok(false)
        }
    }

    pub fn irq_masked(&self) -> bool {
        false
    }

    pub fn unmask_irq(&mut self, _io: &mut Io) -> Result<()> {
        Ok(())
    }

    // --- Registers ----------------------------------------------------------------------------

    fn saved(&mut self, io: &mut Io) -> Result<ScrRegs> {
        if let Some(r) = self.regs {
            return Ok(r);
        }
        self.require_halted()?;
        let r = scr(io)?.read_regs()?;
        self.regs = Some(r);
        Ok(r)
    }

    pub fn read_reg(&mut self, io: &mut Io, idx: usize) -> Result<u32> {
        Ok(self.read_regs(io, &[idx])?[0])
    }

    pub fn read_regs(&mut self, io: &mut Io, idxs: &[usize]) -> Result<Vec<u32>> {
        if let Some(i) = idxs.iter().find(|&&i| i >= XC800_REGS.len()) {
            return Err(other(format!("invalid register {i}")));
        }
        let r = self.saved(io)?;
        Ok(idxs.iter().map(|&i| saved_value(&r, &XC800_REGS[i])).collect())
    }

    pub fn write_reg(&mut self, io: &mut Io, idx: usize, value: u32) -> Result<()> {
        let def = *XC800_REGS.get(idx).ok_or_else(|| other(format!("invalid register {idx}")))?;
        self.require_halted()?;
        if idx == XC800_REG_PC {
            self.set_pc(io, value & 0xFFFF)?;
        } else if let Some(reg) = saved_reg(&def) {
            scr(io)?.write_reg(reg, value as u16 & 0xFF)?;
        } else {
            // dptr
            let s = scr(io)?;
            s.write_reg(ScrReg::Dpl, value as u16 & 0xFF)?;
            s.write_reg(ScrReg::Dph, (value as u16 >> 8) & 0xFF)?;
        }
        self.invalidate();
        Ok(())
    }

    // --- Memory -------------------------------------------------------------------------------

    pub fn read_memory(&mut self, io: &mut Io, addr: u32, len: usize) -> Result<Vec<u8>> {
        scr(io)?.read(addr, len)
    }

    pub fn write_memory(&mut self, io: &mut Io, addr: u32, data: &[u8]) -> Result<()> {
        scr(io)?.write(addr, data)?;
        self.invalidate();
        Ok(())
    }

    // --- Breakpoints --------------------------------------------------------------------------

    pub fn add_breakpoint(&mut self, io: &mut Io, addr: u32, len: u32) -> Result<u8> {
        if len > 3 {
            return Err(Error::Resource("Breakpoint ranges are not supported on the SCR".into()));
        }
        let a = u16::try_from(addr).map_err(|_| other(format!("0x{addr:x} is no SCR code address (0..0xFFFF)")))?;
        if self.slots.contains(&Some(a)) {
            return Err(Error::Resource(format!("Breakpoint at 0x{a:x} exists")));
        }
        let slot = (0..self.slots.len())
            .find(|&i| self.slots[i].is_none() && self.over_slot != Some(i))
            .ok_or_else(|| Error::Resource(format!("All {} SCR hardware breakpoints are in use", self.slots.len())))?;
        scr(io)?.set_hw_breakpoint(slot, a)?;
        self.slots[slot] = Some(a);
        Ok(slot as u8)
    }

    /// The hardware breakpoints, for `xc800 triggers`.
    pub fn describe_breakpoints(&self) -> Vec<String> {
        self.slots
            .iter()
            .enumerate()
            .map(|(i, a)| match (a, self.over_slot == Some(i)) {
                (Some(a), _) => format!("HWBP{i}: on  addr=0x{a:04x}"),
                (None, true) => format!("HWBP{i}: step over"),
                (None, false) => format!("HWBP{i}: off"),
            })
            .collect()
    }

    pub fn add_watchpoint(&mut self, _io: &mut Io, _addr: u32, _len: u32, _access: u32) -> Result<u8> {
        Err(Error::Resource("The SCR has no watchpoints".into()))
    }

    pub fn remove_trigger(&mut self, io: &mut Io, slot: u8) -> Result<()> {
        if self.slots.get_mut(slot as usize).and_then(Option::take).is_some() {
            scr(io)?.clear_hw_breakpoint(slot as usize)?;
        }
        Ok(())
    }

    /// Sets the breakpoints again (after the SCR was reset or attached again).
    pub fn restore_triggers(&mut self, io: &mut Io) -> Result<()> {
        let set: Vec<(usize, u16)> = self.slots.iter().enumerate().filter_map(|(i, a)| a.map(|a| (i, a))).collect();
        if set.is_empty() {
            return Ok(());
        }
        let s = scr(io)?;
        for (i, a) in set {
            s.set_hw_breakpoint(i, a)?;
        }
        Ok(())
    }

    pub fn assert_reset(&mut self) {
        self.invalidate();
        self.state = TargetState::Reset;
        self.reason = DebugReason::Reset;
        self.over_slot = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arch::XC800;

    #[test]
    fn register_table() {
        assert_eq!(XC800.regs[XC800.pc].name, "pc");
        assert_eq!(XC800.regs[XC800.sp].name, "sp");
        assert_eq!(XC800.regs[crate::arch::XC800_REG_PSW].name, "psw");
        assert_eq!(XC800.reg_index("a"), XC800.reg_index("acc"));
        let r = ScrRegs { acc: 1, b: 2, sp: 3, psw: 4, dpl: 0x5F, dph: 0x01, pc: 0x7C, r: [0, 0, 0, 0, 0, 0, 0, 0xB4] };
        let v = |n: &str| saved_value(&r, &XC800.regs[XC800.reg_index(n).unwrap()]);
        assert_eq!((v("acc"), v("b"), v("sp"), v("psw"), v("pc"), v("dptr")), (1, 2, 3, 4, 0x7C, 0x15F));
        assert_eq!(saved_reg(&XC800.regs[XC800.reg_index("dptr").unwrap()]), None);
        assert_eq!(saved_reg(&XC800.regs[XC800.reg_index("dph").unwrap()]), Some(ScrReg::Dph));
        assert_eq!(saved_reg(&XC800.regs[XC800.reg_index("r7").unwrap()]), Some(ScrReg::R(7)));
        assert_eq!(v("r7"), 0xB4);
    }

    #[test]
    fn run_states() {
        let mut c = Xc800::new();
        let st = |halted, pc, reason| ScrStatus { halted, pc, reason };
        assert_eq!(c.apply(st(true, 0, ScrStop::Request)), Transition::Halted { debug: false, boot: false });
        assert_eq!((c.state, c.reason, c.pc), (TargetState::Halted, DebugReason::DbgRq, 0));
        assert_eq!(c.apply(st(true, 0, ScrStop::Request)), Transition::None);
        assert_eq!(c.apply(st(false, 0, ScrStop::Request)), Transition::Running);
        c.slots = vec![None, Some(0xEE)];
        assert_eq!(c.apply(st(true, 0xEE, ScrStop::Breakpoint)), Transition::Halted { debug: false, boot: false });
        assert_eq!(c.hit_trigger(), Some(1));
        assert!(c.owns_address(0) && c.owns_address(scr_space::sfr(0xE0)) && !c.owns_address(0x7000_0000));
    }
}
