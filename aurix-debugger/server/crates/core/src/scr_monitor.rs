//! The SCR's (XC800) boot ROM debug monitor, driven over JTAG (TC3xx UM 49.5.2.3/49.5.2.4).
//!
//! Decoded from winIDEA's DAP traffic and verified over the miniWiggler: the SoC JTAG
//! instruction 0xC0 takes a 3-bit setting (3), instruction 0xC1 (IOINSTR1) connects the SCR's
//! OCDS JTAG interface, and every exchange is a 16-bit DR scan:
//!
//! * TDI: byte in bits 4..11 and the monitor hardware command flags of UM Table 564 - bit 15
//!   Jena_r, bit 14 RRF_r (a frame byte for the monitor), bit 13 MMODE_r, bit 12 MBCON_r, bit 0
//!   COMRST_r. See [`HSTATE`], [`HCONFIG`], [`HMONITOR`], [`HCOMRST`], [`HRESET`].
//! * TDO (captured before the scan's own byte is taken): bits 0..3 status - bit 3 a new monitor
//!   byte is in bits 4..11, bit 2 the last host byte was not consumed yet, bit 1 monitor mode (the
//!   SCR is halted).
//!
//! Monitor frames (UM Table 563): `cmd addr15..8 addr7..0 [len] data… frame# chk 02` with
//! `cmd` = operation << 4 | virtual address bits 19..16 ([`crate::adapter::scr_space`]) and `chk`
//! the XOR of all bytes before it. The monitor answers `01 cmd addr… [len] data… frame# status
//! chk`, data being the bytes read (one per `01` the host sends) or the bytes written. Frame
//! numbers start at 0 after a monitor entry and after HCOMRST.
//!
//! The backend only provides [`MonitorLink`]; run control follows winIDEA's sequences.

use crate::adapter::ScrStop;
use crate::error::{Error, Result};

/// HSTATE: read the status bits, nothing else.
pub const HSTATE: u16 = 0x0000;
/// HCONFIG: A5h with Jena_r - enables the debug interface (MMCR2.JENA).
pub const HCONFIG: u16 = 0x8A50;
/// HMONITOR: A5h with MMODE_r - enter the monitor (halt).
pub const HMONITOR: u16 = 0x2A50;
/// HCOMRST: A5h with COMRST_r - restart the frame state machine (frame number 0).
pub const HCOMRST: u16 = 0x0A51;
/// HRESET: 5Ah with every flag - resets the SCR including OCDS (not the DAP). In an OCDS boot
/// mode the boot ROM then enters the monitor at 0x0000.
pub const HRESET: u16 = 0xF5A1;
/// HDEBUG: a frame byte for the monitor.
const RRF: u16 = 0x4000;

const TDO_NEW: u16 = 0x8;
const TDO_PENDING: u16 = 0x4;
const TDO_MONITOR: u16 = 0x2;

const OP_READ_BYTE: u8 = 0x10;
const OP_READ_BLOCK: u8 = 0x20;
const OP_WRITE_BYTE: u8 = 0x30;
const OP_WRITE_BLOCK: u8 = 0x40;

/// Monitor virtual registers (UM Table 558 0x223xx; mons_RETH/RETL = 0x33/0x34).
pub mod vreg {
    pub const DPL: u32 = 0x2231C;
    pub const DPH: u32 = 0x2231D;
    pub const B: u32 = 0x2231E;
    pub const ACC: u32 = 0x2231F;
    pub const PSW: u32 = 0x22332;
    /// The return address, high byte first.
    pub const PC: u32 = 0x22333;
    pub const SP: u32 = 0x22335;
    /// Written 0 before every step and run (winIDEA).
    pub const CONTROL: u32 = 0x2231B;
    /// Hardware breakpoint addresses, 2 bytes each, low byte first.
    pub const BREAKPOINTS: u32 = 0x22208;
}

/// OCDS SFRs with RMAP (0xF1 MMCR: `F0` step, `E0` run; 0xF3 cleared before both; 0xF4: bit 7 +
/// one enable bit per hardware breakpoint).
mod ocds {
    pub const MMCR: u32 = 0x780F1;
    pub const F3: u32 = 0x780F3;
    pub const BP_ENABLE: u32 = 0x780F4;
    /// SFR 0xD7 with RMAP: winIDEA writes 0x10 after every attach (breakpoints hit only then).
    pub const D7: u32 = 0x78057;
}

/// XRAM 0x1FE8..0x1FF7 is the monitor's scratch for SFR accesses: saved on entry, restored before
/// leaving (UM 49.5.2.4.3).
const SCRATCH: u32 = 0x1_1FE8;
const SCRATCH_LEN: usize = 16;

pub const BREAKPOINT_SLOTS: usize = 4;
const MAX_BLOCK: usize = 128;

/// The JTAG path to the SCR's OCDS interface.
pub trait MonitorLink {
    /// Selects the SCR's OCDS JTAG interface (instruction 0xC0 setting 3, then 0xC1).
    fn monitor_select(&mut self) -> Result<()>;
    /// 16-bit DR scans with instruction 0xC1; returns the TDO of each scan.
    fn monitor_scan(&mut self, tdi: &[u16]) -> Result<Vec<u16>>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Last {
    None,
    Step,
    Run,
    Halt,
}

/// Client state of one attached monitor.
#[derive(Debug, Clone)]
pub struct Monitor {
    frame: u8,
    /// Polls sent after every frame byte (grows when the monitor misses bytes).
    pace: usize,
    halted: bool,
    pc: u16,
    reason: ScrStop,
    last: Last,
    scratch: Option<[u8; SCRATCH_LEN]>,
    breakpoints: [Option<u16>; BREAKPOINT_SLOTS],
    /// The monitor answered frames since the attach: a halt request works without reset.
    armed: bool,
}

impl Default for Monitor {
    fn default() -> Self {
        Self::new()
    }
}

fn err(m: impl Into<String>) -> Error {
    Error::Adapter(format!("SCR monitor: {}", m.into()))
}

impl Monitor {
    pub fn new() -> Self {
        Monitor {
            frame: 0,
            pace: 1,
            halted: false,
            pc: 0,
            reason: ScrStop::Other,
            last: Last::None,
            scratch: None,
            breakpoints: [None; BREAKPOINT_SLOTS],
            armed: false,
        }
    }

    pub fn halted(&self) -> bool {
        self.halted
    }
    pub fn pc(&self) -> u16 {
        self.pc
    }
    pub fn reason(&self) -> ScrStop {
        self.reason
    }

    fn hw(&mut self, l: &mut dyn MonitorLink, cmd: u16) -> Result<u16> {
        Ok(l.monitor_scan(&[cmd])?[0])
    }

    /// Connects to the monitor without stopping the SCR. One found in the monitor already (left
    /// halted by an earlier session, or waiting in its boot ROM) is taken over.
    pub fn attach(&mut self, l: &mut dyn MonitorLink) -> Result<()> {
        *self = Monitor::new();
        l.monitor_select()?;
        let st = self.hw(l, HSTATE)?;
        if st == 0xFFFF {
            return Err(err("no answer on JTAG instruction 0xC1 (is the SCR started in an OCDS boot mode?)"));
        }
        if st & TDO_MONITOR != 0 {
            self.hw(l, HCOMRST)?;
            self.hw(l, HCONFIG)?;
            self.frame = 0;
            self.halted = true;
            self.reason = ScrStop::Request;
            match self.save_scratch(l).and_then(|_| self.read_pc(l)) {
                Ok(pc) => {
                    self.pc = pc;
                    self.arm(l)?;
                }
                Err(_) => self.reset_halt(l)?,
            }
        }
        Ok(())
    }

    /// winIDEA's attach: HRESET, then HCOMRST, HCONFIG, HMONITOR, HSTATE five times. The boot
    /// ROM enters the monitor at its start breakpoint (0x0000); the program continues from 0.
    pub fn reset_halt(&mut self, l: &mut dyn MonitorLink) -> Result<()> {
        *self = Monitor::new();
        let mut seq = vec![HRESET];
        for _ in 0..5 {
            seq.extend([HCOMRST, HCONFIG, HMONITOR, HSTATE]);
        }
        l.monitor_scan(&seq)?;
        if !self.wait_monitor(l, 50)? {
            return Err(err("no monitor entry after HRESET (is the SCR started in an OCDS boot mode?)"));
        }
        self.frame = 0;
        self.halted = true;
        self.reason = ScrStop::Request;
        self.save_scratch(l)?;
        self.write_pc(l, 0)?;
        self.pc = 0;
        self.arm(l)
    }

    /// First answer of the monitor in this session: the OCDS setup winIDEA writes after its
    /// attach (0xF3 = 0, 0xF4 = 0x80 + breakpoint enables, 0xD7 = 0x10).
    fn arm(&mut self, l: &mut dyn MonitorLink) -> Result<()> {
        if self.armed {
            return Ok(());
        }
        self.write(l, ocds::F3, &[0])?;
        let mask = self.bp_mask();
        self.write(l, ocds::BP_ENABLE, &[mask])?;
        self.write(l, ocds::D7, &[0x10])?;
        self.armed = true;
        Ok(())
    }

    fn bp_mask(&self) -> u8 {
        self.breakpoints.iter().enumerate().fold(0x80u8, |m, (i, b)| if b.is_some() { m | 1 << i } else { m })
    }

    fn wait_monitor(&mut self, l: &mut dyn MonitorLink, tries: usize) -> Result<bool> {
        for _ in 0..tries {
            if self.hw(l, HSTATE)? & TDO_MONITOR != 0 {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Run state; notices a halt (breakpoint, step end, halt request) and reads the stop PC.
    pub fn poll(&mut self, l: &mut dyn MonitorLink) -> Result<bool> {
        if self.halted {
            return Ok(true);
        }
        let st = self.hw(l, HSTATE)?;
        if st == 0xFFFF {
            // The JTAG setting is lost after a new DAP connect (e.g. a chip reset).
            l.monitor_select()?;
            return Ok(false);
        }
        if st & TDO_MONITOR != 0 {
            self.frame = 0;
            self.halted = true;
            match self.entered(l).and_then(|_| self.arm(l)) {
                Ok(()) => {}
                Err(e) if !self.armed => {
                    // A monitor never prepared by a debugger takes the halt but no frames (seen on
                    // a freshly powered TC377): reset the SCR into it, as winIDEA's attach does.
                    crate::log_warn!("SCR: monitor does not answer ({e}); resetting the SCR into its monitor (its program restarts at 0x0000)");
                    self.reset_halt(l)?;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(self.halted)
    }

    /// After a monitor entry: save the scratch area, read the break address and turn it into the
    /// continue address (UM 49.5.2.4.4: IP breakpoint - 2, anything else - 1), as winIDEA does.
    fn entered(&mut self, l: &mut dyn MonitorLink) -> Result<()> {
        self.save_scratch(l)?;
        let raw = self.read_pc(l)?;
        let at_bp = raw.checked_sub(2).filter(|a| self.breakpoints.contains(&Some(*a)));
        let (pc, reason) = match (self.last, at_bp) {
            (Last::Step, _) => (raw.wrapping_sub(1), ScrStop::Step),
            (_, Some(a)) => (a, ScrStop::Breakpoint),
            (Last::Halt, None) => (raw.wrapping_sub(1), ScrStop::Request),
            _ => (raw.wrapping_sub(1), ScrStop::Other),
        };
        self.write_pc(l, pc)?;
        self.pc = pc;
        self.reason = reason;
        Ok(())
    }

    fn save_scratch(&mut self, l: &mut dyn MonitorLink) -> Result<()> {
        let s = self.read(l, SCRATCH, SCRATCH_LEN)?;
        let mut a = [0u8; SCRATCH_LEN];
        a.copy_from_slice(&s);
        self.scratch = Some(a);
        Ok(())
    }

    fn read_pc(&mut self, l: &mut dyn MonitorLink) -> Result<u16> {
        let b = self.read(l, vreg::PC, 2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn write_pc(&mut self, l: &mut dyn MonitorLink, pc: u16) -> Result<()> {
        self.write(l, vreg::PC, &pc.to_be_bytes())
    }

    pub fn halt(&mut self, l: &mut dyn MonitorLink) -> Result<()> {
        if self.halted {
            return Ok(());
        }
        self.last = Last::Halt;
        if !self.armed {
            // Debug interface on (MMCR2.JENA) and a fresh frame state machine first.
            self.hw(l, HCOMRST)?;
            self.hw(l, HCONFIG)?;
        }
        self.hw(l, HMONITOR)?;
        Ok(())
    }

    /// Leaves the monitor: winIDEA's sequence, `MMCR` = `F0` for a step, `E0` to run.
    fn go(&mut self, l: &mut dyn MonitorLink, mmcr: u8) -> Result<()> {
        self.need_halted()?;
        self.write(l, ocds::F3, &[0])?;
        if let Some(s) = self.scratch {
            self.write(l, SCRATCH, &s)?;
        }
        self.read(l, vreg::CONTROL, 1)?;
        self.write(l, vreg::CONTROL, &[0])?;
        self.write(l, ocds::MMCR, &[mmcr])?;
        self.halted = false;
        self.scratch = None;
        Ok(())
    }

    /// One instruction; returns with the SCR halted again.
    pub fn step(&mut self, l: &mut dyn MonitorLink) -> Result<()> {
        self.last = Last::Step;
        self.go(l, 0xF0)?;
        for _ in 0..50 {
            if self.poll(l)? {
                return Ok(());
            }
        }
        Err(Error::Timeout("SCR step: no monitor entry".into()))
    }

    /// Continues the program; from a breakpoint address it steps over it first (winIDEA does).
    pub fn resume(&mut self, l: &mut dyn MonitorLink) -> Result<()> {
        self.need_halted()?;
        if self.breakpoints.contains(&Some(self.pc)) {
            self.step(l)?;
            if self.reason == ScrStop::Breakpoint {
                return Ok(());
            }
        }
        self.last = Last::Run;
        self.go(l, 0xE0)
    }

    fn need_halted(&self) -> Result<()> {
        if self.halted {
            Ok(())
        } else {
            Err(Error::NotHalted("the SCR is running".into()))
        }
    }

    pub fn set_breakpoint(&mut self, l: &mut dyn MonitorLink, slot: usize, addr: Option<u16>) -> Result<()> {
        self.need_halted()?;
        if slot >= BREAKPOINT_SLOTS {
            return Err(Error::Resource(format!("SCR breakpoint slot {slot} (0..{})", BREAKPOINT_SLOTS - 1)));
        }
        self.write(l, vreg::BREAKPOINTS + 2 * slot as u32, &addr.unwrap_or(0).to_le_bytes())?;
        self.breakpoints[slot] = addr;
        let mask = self.bp_mask();
        self.write(l, ocds::BP_ENABLE, &[mask])
    }

    pub fn read(&mut self, l: &mut dyn MonitorLink, vaddr: u32, len: usize) -> Result<Vec<u8>> {
        self.need_halted()?;
        let mut out = Vec::with_capacity(len);
        while out.len() < len {
            let a = vaddr + out.len() as u32;
            let n = (len - out.len()).min(MAX_BLOCK);
            let n = n.min((0x1_0000 - (a & 0xFFFF)) as usize).max(1);
            out.extend(self.frame(l, if n == 1 { OP_READ_BYTE } else { OP_READ_BLOCK }, a, &vec![1u8; n])?);
        }
        Ok(out)
    }

    pub fn write(&mut self, l: &mut dyn MonitorLink, vaddr: u32, data: &[u8]) -> Result<()> {
        self.need_halted()?;
        let mut done = 0;
        while done < data.len() {
            let a = vaddr + done as u32;
            let n = (data.len() - done).min(MAX_BLOCK).min((0x1_0000 - (a & 0xFFFF)) as usize).max(1);
            self.frame(l, if n == 1 { OP_WRITE_BYTE } else { OP_WRITE_BLOCK }, a, &data[done..done + n])?;
            done += n;
        }
        Ok(())
    }

    /// One monitor frame; returns the data bytes of the answer. Retries with more polls per byte
    /// (and HCOMRST) if the monitor did not take a byte in time.
    fn frame(&mut self, l: &mut dyn MonitorLink, op: u8, vaddr: u32, data: &[u8]) -> Result<Vec<u8>> {
        if vaddr >= 0x10_0000 {
            return Err(Error::Usage(format!("SCR address 0x{vaddr:x} outside the 20-bit map")));
        }
        let mut last = err("no answer");
        for _ in 0..4 {
            match self.try_frame(l, op, vaddr, data) {
                Ok(d) => return Ok(d),
                Err(Retry::Again(e)) => {
                    last = e;
                    self.pace = (self.pace * 2 + 1).min(8);
                    self.hw(l, HCOMRST)?;
                    self.frame = 0;
                }
                Err(Retry::Lost(e)) => {
                    // A failed transaction, or the 0xC0 setting lost with a new DAP connect.
                    last = e;
                    l.monitor_select()?;
                    self.hw(l, HCOMRST)?;
                    self.frame = 0;
                }
                Err(Retry::Fatal(e)) => return Err(e),
            }
        }
        Err(last)
    }

    fn try_frame(&mut self, l: &mut dyn MonitorLink, op: u8, vaddr: u32, data: &[u8]) -> std::result::Result<Vec<u8>, Retry> {
        let block = op == OP_READ_BLOCK || op == OP_WRITE_BLOCK;
        let mut host = vec![op | (vaddr >> 16) as u8, (vaddr >> 8) as u8, vaddr as u8];
        if block {
            host.push(data.len() as u8);
        }
        host.extend_from_slice(data);
        host.push(self.frame);
        let chk = host.iter().fold(0u8, |c, b| c ^ b);
        host.push(chk);
        host.push(0x02);
        let want = host.len() + 1;

        let mut tdi = Vec::with_capacity(host.len() * (1 + self.pace) + 4);
        for &b in &host {
            tdi.push(RRF | (b as u16) << 4);
            tdi.extend(std::iter::repeat_n(HSTATE, self.pace));
        }
        tdi.extend(std::iter::repeat_n(HSTATE, 4));
        let tdo = l.monitor_scan(&tdi).map_err(Retry::Lost)?;
        let first = 0;
        let mut got = Vec::new();
        for (i, (&t, &d)) in tdi.iter().zip(&tdo).enumerate() {
            if d == 0xFFFF {
                return Err(Retry::Lost(err("no answer on JTAG instruction 0xC1")));
            }
            if t & RRF != 0 && d & TDO_PENDING != 0 {
                return Err(Retry::Again(err(format!("monitor busy at byte {i}"))));
            }
            if i > first && d & TDO_NEW != 0 {
                got.push((d >> 4) as u8);
            }
        }
        let mut polls = 0;
        while got.len() < want {
            polls += 1;
            if polls > 20 {
                return Err(Retry::Again(err(format!("answer incomplete ({} of {want} bytes)", got.len()))));
            }
            for d in l.monitor_scan(&[HSTATE; 8]).map_err(Retry::Lost)? {
                if d & TDO_NEW != 0 {
                    got.push((d >> 4) as u8);
                }
            }
        }
        if got[0] != 0x01 || got[1..4] != host[..3] {
            return Err(Retry::Again(err(format!("unexpected answer {:02x?}", &got[..want]))));
        }
        let chk = got[..want - 1].iter().fold(0u8, |c, b| c ^ b);
        if chk != got[want - 1] {
            return Err(Retry::Again(err(format!("answer checksum {:02x} != {chk:02x}", got[want - 1]))));
        }
        let status = got[want - 2];
        self.frame = self.frame.wrapping_add(1);
        if status & 0x08 != 0 {
            return Err(Retry::Fatal(Error::Memory { addr: vaddr as u64, write: op >= OP_WRITE_BYTE, detail: "SCR monitor: invalid address range".into() }));
        }
        if status != 0 {
            return Err(Retry::Again(err(format!("frame status {status:02x}"))));
        }
        let start = 4 + block as usize;
        Ok(got[start..start + data.len()].to_vec())
    }

    pub fn read_regs(&mut self, l: &mut dyn MonitorLink) -> Result<crate::adapter::ScrRegs> {
        let d = self.read(l, vreg::DPL, 4)?; // DPL DPH B ACC
        let psw = self.read(l, vreg::PSW, 1)?[0];
        let sp = self.read(l, vreg::SP, 1)?[0];
        let bank = ((psw >> 3) & 3) as u32;
        let r = self.read(l, crate::adapter::scr_space::IRAM + bank * 8, 8)?;
        let mut regs = crate::adapter::ScrRegs { acc: d[3], b: d[2], sp, psw, dpl: d[0], dph: d[1], pc: self.pc, r: [0; 8] };
        regs.r.copy_from_slice(&r);
        Ok(regs)
    }

    pub fn write_reg(&mut self, l: &mut dyn MonitorLink, reg: crate::adapter::ScrReg, value: u16) -> Result<()> {
        use crate::adapter::ScrReg;
        let v = value as u8;
        match reg {
            ScrReg::Acc => self.write(l, vreg::ACC, &[v]),
            ScrReg::B => self.write(l, vreg::B, &[v]),
            ScrReg::Sp => self.write(l, vreg::SP, &[v]),
            ScrReg::Psw => self.write(l, vreg::PSW, &[v]),
            ScrReg::Dpl => self.write(l, vreg::DPL, &[v]),
            ScrReg::Dph => self.write(l, vreg::DPH, &[v]),
            ScrReg::Pc => {
                self.write_pc(l, value)?;
                self.pc = value;
                Ok(())
            }
            ScrReg::R(n) => {
                let psw = self.read(l, vreg::PSW, 1)?[0];
                let a = crate::adapter::scr_space::IRAM + ((psw >> 3) & 3) as u32 * 8 + (n & 7) as u32;
                self.write(l, a, &[v])
            }
        }
    }
}

enum Retry {
    /// The monitor missed or garbled a byte: HCOMRST and again, with more polls.
    Again(Error),
    /// The JTAG path failed: select it again, HCOMRST and again.
    Lost(Error),
    Fatal(Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A monitor model at the byte level: takes a host byte per RRF scan once the previous one
    /// was consumed (one scan later), answers like the boot ROM, memory as a flat map.
    struct Fake {
        mem: std::collections::HashMap<u32, u8>,
        monitor: bool,
        pending: Option<u8>,
        out: std::collections::VecDeque<u8>,
        rx: Vec<u8>,
        ans: Vec<u8>,
        latch: Option<u8>,
        selected: bool,
        exits: Vec<u8>,
        scans: usize,
    }

    impl Fake {
        fn new() -> Self {
            Fake {
                mem: Default::default(),
                monitor: false,
                pending: None,
                out: Default::default(),
                rx: Vec::new(),
                ans: Vec::new(),
                latch: None,
                selected: false,
                exits: Vec::new(),
                scans: 0,
            }
        }
        fn byte(&mut self, b: u8) {
            if self.rx.is_empty() {
                self.ans = vec![0x01];
                self.out.push_back(0x01);
            }
            self.rx.push(b);
            let r = self.rx.clone();
            let op = r[0] & 0xF0;
            let block = op == OP_READ_BLOCK || op == OP_WRITE_BLOCK;
            let hdr = 3 + block as usize;
            let n = if block && r.len() > 3 { r[3] as usize } else { 1 };
            let reply = if r.len() <= hdr {
                b
            } else {
                let a = ((r[0] as u32 & 0xF) << 16) | (r[1] as u32) << 8 | r[2] as u32;
                let i = r.len() - hdr - 1;
                if i < n {
                    let addr = a + i as u32;
                    if op == OP_READ_BYTE || op == OP_READ_BLOCK {
                        *self.mem.get(&addr).unwrap_or(&0)
                    } else {
                        self.mem.insert(addr, b);
                        b
                    }
                } else if i == n {
                    b // frame#
                } else if i == n + 1 {
                    0 // status, after the host checksum
                } else {
                    // C_END: the monitor's checksum, then the command takes effect
                    let chk = self.ans.iter().fold(0, |c, x| c ^ x);
                    self.out.push_back(chk);
                    if a == ocds::MMCR && op == OP_WRITE_BYTE {
                        self.exits.push(r[hdr]);
                        self.monitor = r[hdr] == 0xF0; // a step comes straight back
                        if self.monitor {
                            let pc = u16::from_be_bytes([self.mem[&vreg::PC], self.mem[&(vreg::PC + 1)]]) + 2;
                            self.mem.insert(vreg::PC, (pc >> 8) as u8);
                            self.mem.insert(vreg::PC + 1, pc as u8);
                        }
                    }
                    self.rx.clear();
                    return;
                }
            };
            self.ans.push(reply);
            self.out.push_back(reply);
        }
    }

    impl MonitorLink for Fake {
        fn monitor_select(&mut self) -> Result<()> {
            self.selected = true;
            Ok(())
        }
        fn monitor_scan(&mut self, tdi: &[u16]) -> Result<Vec<u16>> {
            let mut tdo = Vec::new();
            for &t in tdi {
                self.scans += 1;
                if !self.selected {
                    tdo.push(0xFFFF);
                    continue;
                }
                // the previous scan's host byte is consumed now
                if let Some(b) = self.pending.take() {
                    self.byte(b);
                }
                let mut d = if self.monitor { TDO_MONITOR } else { 0 };
                if let Some(b) = self.out.pop_front() {
                    d |= TDO_NEW | (b as u16) << 4;
                    self.latch = Some(b);
                } else if let Some(b) = self.latch {
                    d |= (b as u16) << 4;
                }
                tdo.push(d);
                match t {
                    HMONITOR | HRESET => {
                        self.monitor = true;
                        self.rx.clear();
                        self.mem.insert(vreg::PC, 0);
                        self.mem.insert(vreg::PC + 1, 0x11);
                    }
                    HCOMRST => self.rx.clear(),
                    t if t & RRF != 0 && self.monitor => self.pending = Some((t >> 4) as u8),
                    _ => {}
                }
            }
            Ok(tdo)
        }
    }

    #[test]
    fn attach_reads_pc_and_memory_and_steps() {
        let mut f = Fake::new();
        f.mem.insert(0x0000, 0x02);
        f.mem.insert(0x0001, 0x00);
        f.mem.insert(0x0002, 0x79);
        let mut m = Monitor::new();
        m.attach(&mut f).unwrap();
        assert!(!m.halted(), "attach leaves a running SCR running");
        assert!(!m.poll(&mut f).unwrap());
        m.halt(&mut f).unwrap();
        assert!(m.poll(&mut f).unwrap());
        assert_eq!(m.pc(), 0x10, "halt request: break address - 1");
        assert_eq!(m.reason(), ScrStop::Request);
        assert_eq!(m.read(&mut f, 0, 3).unwrap(), vec![0x02, 0x00, 0x79]);
        m.write(&mut f, 0x2_0030, &[0xAB, 0xCD]).unwrap();
        assert_eq!(m.read(&mut f, 0x2_0030, 2).unwrap(), vec![0xAB, 0xCD]);
        m.write_reg(&mut f, crate::adapter::ScrReg::Pc, 0x79).unwrap();
        m.step(&mut f).unwrap();
        assert_eq!(f.exits, vec![0xF0]);
        assert_eq!(m.reason(), ScrStop::Step);
        assert_eq!(m.pc(), 0x7A, "fake steps two bytes: break address - 1");
        m.resume(&mut f).unwrap();
        assert_eq!(f.exits, vec![0xF0, 0xE0]);
        assert!(!m.halted());
        assert!(!m.poll(&mut f).unwrap());
    }

    #[test]
    fn breakpoints_set_the_enable_mask() {
        let mut f = Fake::new();
        let mut m = Monitor::new();
        m.attach(&mut f).unwrap();
        m.halt(&mut f).unwrap();
        assert!(m.poll(&mut f).unwrap());
        m.set_breakpoint(&mut f, 1, Some(0x00EE)).unwrap();
        assert_eq!(f.mem[&(vreg::BREAKPOINTS + 2)], 0xEE);
        assert_eq!(f.mem[&ocds::BP_ENABLE], 0x82);
        m.set_breakpoint(&mut f, 1, None).unwrap();
        assert_eq!(f.mem[&ocds::BP_ENABLE], 0x80);
    }

    #[test]
    fn unselected_interface_is_reported() {
        struct Dead;
        impl MonitorLink for Dead {
            fn monitor_select(&mut self) -> Result<()> {
                Ok(())
            }
            fn monitor_scan(&mut self, tdi: &[u16]) -> Result<Vec<u16>> {
                Ok(vec![0xFFFF; tdi.len()])
            }
        }
        let e = Monitor::new().attach(&mut Dead).unwrap_err();
        assert!(e.to_string().contains("0xC1"), "{e}");
    }
}
