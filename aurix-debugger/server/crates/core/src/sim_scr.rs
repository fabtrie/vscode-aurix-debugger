//! The SCR (XC800 standby controller) of the simulated TC37x, behind [`ScrDebug`].
//!
//! It is reachable only once "the application" started it in an OCDS boot mode
//! ([`SimScr::enabled`]); attaching stops it at 0x0000 as the boot ROM's start breakpoint does on
//! the chip. While it runs, every status query executes a few instructions of its program (an
//! 8051 subset: flow, `MOV SP`, `INC Rn`), so breakpoints, steps and a step over a call behave as
//! on the hardware.

use std::collections::HashMap;

use isa::xc800::{self, Flow};

use crate::adapter::{scr_space, ScrDebug, ScrReg, ScrRegs, ScrStatus, ScrStop};
use crate::error::{other, Result};

/// Hardware breakpoints of the model.
pub const SIM_SCR_BREAKPOINTS: usize = 4;
/// Instructions the running SCR executes per status query.
const RUN_PER_QUERY: usize = 16;

/// The program the model runs:
///
/// ```text
/// 0000  LJMP 0x0079
/// 0079  MOV  SP, #0x20
/// 007C  LCALL 0x0100
/// 007F  SJMP 0x007C
/// 0100  INC  R7
/// 0101  RET
/// ```
pub const SIM_SCR_PROGRAM: &[(u16, &[u8])] =
    &[(0x0000, &[0x02, 0x00, 0x79]), (0x0079, &[0x75, 0xD4, 0x20, 0x12, 0x01, 0x00, 0x80, 0xFB]), (0x0100, &[0x0F, 0x22])];

#[derive(Debug)]
pub struct SimScr {
    /// The application started the SCR in an OCDS boot mode (PMSWCR4.SCRCFG = 0x0A).
    pub enabled: bool,
    pub attached: bool,
    pub halted: bool,
    pub reason: ScrStop,
    pub regs: ScrRegs,
    /// XRAM, seen as code and as external data.
    pub xram: Vec<u8>,
    pub iram: [u8; 256],
    pub sfr: HashMap<u32, u8>,
    pub bps: [Option<u16>; SIM_SCR_BREAKPOINTS],
    /// Interface calls, for tests.
    pub calls: Vec<&'static str>,
}

impl Default for SimScr {
    fn default() -> Self {
        Self::new()
    }
}

impl SimScr {
    pub fn new() -> Self {
        let mut xram = vec![0xFF; 0x1_0000];
        for (at, code) in SIM_SCR_PROGRAM {
            xram[*at as usize..*at as usize + code.len()].copy_from_slice(code);
        }
        SimScr {
            enabled: false,
            attached: false,
            halted: false,
            reason: ScrStop::Other,
            regs: ScrRegs { sp: 0x07, ..Default::default() },
            xram,
            iram: [0; 256],
            sfr: HashMap::new(),
            bps: [None; SIM_SCR_BREAKPOINTS],
            calls: Vec::new(),
        }
    }

    fn check(&self) -> Result<()> {
        if !self.attached {
            return Err(other("SCR: not attached"));
        }
        Ok(())
    }

    fn check_halted(&self) -> Result<()> {
        self.check()?;
        if !self.halted {
            return Err(other("SCR: not halted"));
        }
        Ok(())
    }

    /// Executes the instruction at the PC.
    fn exec(&mut self) {
        let pc = self.regs.pc;
        let at = pc as usize;
        let code = [self.xram[at], self.xram[(at + 1) & 0xFFFF], self.xram[(at + 2) & 0xFFFF]];
        let Some(i) = xc800::decode(&code, pc as u32) else { return };
        let next = (pc as u32 + i.len) as u16;
        let bank = ((self.regs.psw >> 3) & 3) as usize * 8;
        self.regs.pc = match i.flow {
            Flow::Jump | Flow::Branch => i.target.map_or(next, |t| t as u16),
            Flow::Call => {
                self.regs.sp = self.regs.sp.wrapping_add(1);
                self.iram[self.regs.sp as usize] = next as u8;
                self.regs.sp = self.regs.sp.wrapping_add(1);
                self.iram[self.regs.sp as usize] = (next >> 8) as u8;
                i.target.map_or(next, |t| t as u16)
            }
            Flow::Ret => {
                let hi = self.iram[self.regs.sp as usize];
                self.regs.sp = self.regs.sp.wrapping_sub(1);
                let lo = self.iram[self.regs.sp as usize];
                self.regs.sp = self.regs.sp.wrapping_sub(1);
                u16::from_be_bytes([hi, lo])
            }
            _ => {
                match code[0] {
                    0x75 if code[1] == 0xD4 => self.regs.sp = code[2],
                    0x08..=0x0F => {
                        let r = bank + (code[0] & 7) as usize;
                        self.iram[r] = self.iram[r].wrapping_add(1);
                    }
                    _ => {}
                }
                next
            }
        };
    }

    fn at_breakpoint(&self) -> bool {
        self.bps.contains(&Some(self.regs.pc))
    }
}

impl ScrDebug for SimScr {
    fn attach(&mut self) -> Result<()> {
        self.calls.push("attach");
        if !self.enabled {
            return Err(other("SCR: no debug access (not started in an OCDS boot mode)"));
        }
        if !self.attached {
            self.attached = true;
            self.halted = true;
            self.reason = ScrStop::Request;
            self.regs.pc = 0;
        }
        Ok(())
    }

    fn status(&mut self) -> Result<ScrStatus> {
        self.check()?;
        if !self.halted {
            for _ in 0..RUN_PER_QUERY {
                self.exec();
                if self.at_breakpoint() {
                    self.halted = true;
                    self.reason = ScrStop::Breakpoint;
                    break;
                }
            }
        }
        Ok(ScrStatus { halted: self.halted, pc: self.regs.pc, reason: self.reason })
    }

    fn halt(&mut self) -> Result<()> {
        self.check()?;
        self.calls.push("halt");
        if !self.halted {
            self.halted = true;
            self.reason = ScrStop::Request;
        }
        Ok(())
    }

    fn resume(&mut self) -> Result<()> {
        self.check_halted()?;
        self.calls.push("resume");
        // Leaves the instruction at the PC first, even under a breakpoint.
        self.exec();
        self.halted = self.at_breakpoint();
        self.reason = ScrStop::Breakpoint;
        Ok(())
    }

    fn step(&mut self) -> Result<()> {
        self.check_halted()?;
        self.calls.push("step");
        self.exec();
        self.reason = ScrStop::Step;
        Ok(())
    }

    fn read_regs(&mut self) -> Result<ScrRegs> {
        self.check_halted()?;
        let bank = ((self.regs.psw >> 3) & 3) as usize * 8;
        let mut r = self.regs;
        r.r.copy_from_slice(&self.iram[bank..bank + 8]);
        Ok(r)
    }

    fn write_reg(&mut self, reg: ScrReg, value: u16) -> Result<()> {
        self.check_halted()?;
        let r = &mut self.regs;
        match reg {
            ScrReg::Acc => r.acc = value as u8,
            ScrReg::B => r.b = value as u8,
            ScrReg::Sp => r.sp = value as u8,
            ScrReg::Psw => r.psw = value as u8,
            ScrReg::Dpl => r.dpl = value as u8,
            ScrReg::Dph => r.dph = value as u8,
            ScrReg::Pc => r.pc = value,
            ScrReg::R(n) => {
                let bank = ((r.psw >> 3) & 3) as usize * 8;
                self.iram[bank + (n as usize & 7)] = value as u8;
            }
        }
        Ok(())
    }

    fn hw_breakpoint_slots(&self) -> usize {
        SIM_SCR_BREAKPOINTS
    }

    fn set_hw_breakpoint(&mut self, slot: usize, addr: u16) -> Result<()> {
        self.check()?;
        *self.bps.get_mut(slot).ok_or_else(|| other(format!("SCR: no breakpoint {slot}")))? = Some(addr);
        Ok(())
    }

    fn clear_hw_breakpoint(&mut self, slot: usize) -> Result<()> {
        self.check()?;
        *self.bps.get_mut(slot).ok_or_else(|| other(format!("SCR: no breakpoint {slot}")))? = None;
        Ok(())
    }

    fn read(&mut self, vaddr: u32, len: usize) -> Result<Vec<u8>> {
        self.check()?;
        (0..len as u32)
            .map(|i| {
                let a = vaddr + i;
                match a {
                    _ if a < scr_space::IRAM => Ok(self.xram[(a & 0xFFFF) as usize]),
                    _ if a < scr_space::IRAM + 0x100 => Ok(self.iram[(a - scr_space::IRAM) as usize]),
                    _ if (scr_space::SFR..scr_space::END).contains(&a) => Ok(self.sfr.get(&a).copied().unwrap_or(0)),
                    _ => Err(other(format!("SCR: no memory at virtual address 0x{a:05x}"))),
                }
            })
            .collect()
    }

    fn write(&mut self, vaddr: u32, data: &[u8]) -> Result<()> {
        self.check()?;
        for (i, &b) in data.iter().enumerate() {
            let a = vaddr + i as u32;
            match a {
                _ if a < scr_space::IRAM => self.xram[(a & 0xFFFF) as usize] = b,
                _ if a < scr_space::IRAM + 0x100 => self.iram[(a - scr_space::IRAM) as usize] = b,
                _ if (scr_space::SFR..scr_space::END).contains(&a) => {
                    self.sfr.insert(a, b);
                }
                _ => return Err(other(format!("SCR: no memory at virtual address 0x{a:05x}"))),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_runs_and_stops_at_breakpoints() {
        let mut s = SimScr::new();
        assert!(s.attach().is_err());
        s.enabled = true;
        s.attach().unwrap();
        assert_eq!(s.status().unwrap(), ScrStatus { halted: true, pc: 0, reason: ScrStop::Request });
        s.step().unwrap();
        s.step().unwrap();
        assert_eq!((s.regs.pc, s.regs.sp), (0x7C, 0x20));
        s.set_hw_breakpoint(1, 0x0101).unwrap();
        s.resume().unwrap();
        let st = s.status().unwrap();
        assert_eq!((st.halted, st.pc, st.reason), (true, 0x0101, ScrStop::Breakpoint));
        assert_eq!(s.iram[7], 1, "INC R7 ran once");
        // Return address 0x007F on the stack, low byte first.
        assert_eq!(s.read(scr_space::IRAM + 0x21, 2).unwrap(), vec![0x7F, 0x00]);
        s.clear_hw_breakpoint(1).unwrap();
        s.resume().unwrap();
        assert!(!s.status().unwrap().halted);
        s.halt().unwrap();
        assert!(s.status().unwrap().halted);
    }
}
