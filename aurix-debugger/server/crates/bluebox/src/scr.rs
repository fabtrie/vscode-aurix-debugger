//! The TC3xx standby controller (SCR, an XC800/8051, core 3) through the probe, as winIDEA 9.21.417
//! debugs it (`scr2-417.jsonl`, `scr3-417.jsonl`).
//!
//! Preconditions: the application started the SCR in its OCDS SoC DAP boot mode
//! (PMSWCR4.SCRCFG = 0x0A), and the attach set debug mode DAP Standard
//! ([`crate::attach::DebugMode::Standard`]); in DAP Wide the probe reports the SCR inaccessible.
//!
//! The services, with the request bodies winIDEA sends:
//!
//! | Path | Body | Use |
//! |---|---|---|
//! | `//2///12/` | mask, count 1, core 3, mask, flag, ... (34 bytes) | status; with mask `3f` the reply holds the run state, the stop reason and the [`Regs`] block |
//! | `//2///17/` | op, count 1, core 3 | op `00`: halt; op `01`: before the first run after the attach |
//! | `//2///20/` | mode (`01` run, `02` step), core 3 | first message of a step or run |
//! | `//3//3/9/` | the 16-byte [`Regs`] block | WriteRegisters: after `//2///20/`, when registers changed |
//! | `//3//3/11/` | `01 00` step, `01 01` run | TargetRun: executes one instruction either way |
//! | `//3//3/7/` | `00` | ReadCoreStatus, after each TargetRun |
//! | `//3//3/3/`, `//2///15/`, `//2///16/`, `//2///18/` | see [`run`](Probe::scr_run) | the rest of a run; which one lets the core go is not known, so the sequence is replayed |
//! | `//3//3/12/` | slot, enable, address u32 | SetHWExecutionBPIndex32, after `//2////` and `//2///12/` |
//! | `//3//3/13/` | `01`/`00`, 12 zero bytes | DoConfigOnchipBreakpointLogic (attach, first run) |
//! | `//3//3/17/`, `18`, `16` | `00` | IsDebugAccess, OCDSInitialize, DebugEstablished (attach) |
//!
//! Memory goes through `//2///14/` in its core form ([`crate::memory::core_request`]), space `00`
//! code (XRAM), `01` data (IRAM 0x00-0x7F, SFRs 0x80-0xFF). While the SCR is halted the debug monitor
//! uses ACC, B, SP, PSW, DPTR and register bank 0 itself: reading them as data shows its values, so
//! registers come from the [`Regs`] block only.

use crate::link::Link;
use crate::memory::{self, MemReply};
use crate::probe::Probe;
use crate::reset::{exchange, service_reply};
use crate::{hex, message, Error, Result};

/// The SCR's core index in the probe's services.
pub const CORE: u8 = 3;
/// Hardware breakpoints winIDEA was seen to use (slots 0 and 1).
pub const BREAKPOINTS: usize = 2;
/// Memory spaces of [`crate::memory::core_request`].
pub const SPACE_CODE: u8 = 0;
pub const SPACE_DATA: u8 = 1;
/// The SCR's XRAM, which holds its program (TC3xx UM 49: 8 KB at code 0x0000).
pub const XRAM_SIZE: usize = 0x2000;
/// Largest block per memory request (winIDEA reads 128 bytes at a time).
pub const MAX_BLOCK: usize = 128;

pub const STATUS: &str = "//2///12/";
pub const HALT: &str = "//2///17/";
pub const MODE: &str = "//2///20/";
pub const RUN_15: &str = "//2///15/";
pub const RUN_16: &str = "//2///16/";
pub const RUN_18: &str = "//2///18/";
pub const MASKED_WRITE: &str = "//2////";
pub const WRITE_REGS: &str = "//3//3/9/";
pub const TARGET_RUN: &str = "//3//3/11/";
pub const CORE_STATUS: &str = "//3//3/7/";
pub const GO: &str = "//3//3/3/";
pub const BREAKPOINT: &str = "//3//3/12/";
pub const BP_LOGIC: &str = "//3//3/13/";
pub const IS_DEBUG_ACCESS: &str = "//3//3/17/";
pub const OCDS_INITIALIZE: &str = "//3//3/18/";
pub const DEBUG_ESTABLISHED: &str = "//3//3/16/";

/// `PMSWCR4`, which winIDEA rewrites through [`MASKED_WRITE`] before each breakpoint change.
pub const PMSWCR4: u32 = 0xF024_80C4;

/// `//2///15/` of a run, as winIDEA 9.21.417 sends it (constant in all captured runs but for a u64
/// at 0x77 that looks like a host heap address, and one byte at 0x4a in the first run after an
/// attach).
const RUN_15_BODY: &str = "030000000100000200000002000000010000000000000000000000000001000003000000000100000000000000000000000000000001010000000300000001010000000200000001010101000000000000000000000000000000000000000000000000000000000000000000000007000000000000000070d0b78dfc010000010000000000";

/// The registers as the debug monitor saved them: the body of `//3//3/9/` and part of the
/// `//2///12/` reply. `PC u16 LE, ACC, B, PSW, DPTR u16 LE, R0..R7, SP`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Regs {
    pub pc: u16,
    pub acc: u8,
    pub b: u8,
    pub psw: u8,
    pub dptr: u16,
    pub r: [u8; 8],
    pub sp: u8,
}

impl Regs {
    pub const LEN: usize = 16;

    pub fn decode(b: &[u8]) -> Option<Regs> {
        let b = b.get(..Self::LEN)?;
        let mut r = [0u8; 8];
        r.copy_from_slice(&b[7..15]);
        Some(Regs {
            pc: u16::from_le_bytes([b[0], b[1]]),
            acc: b[2],
            b: b[3],
            psw: b[4],
            dptr: u16::from_le_bytes([b[5], b[6]]),
            r,
            sp: b[15],
        })
    }

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0..2].copy_from_slice(&self.pc.to_le_bytes());
        b[2] = self.acc;
        b[3] = self.b;
        b[4] = self.psw;
        b[5..7].copy_from_slice(&self.dptr.to_le_bytes());
        b[7..15].copy_from_slice(&self.r);
        b[15] = self.sp;
        b
    }
}

/// Why the SCR stopped (`//2///12/` reply, 0xe4): a bit field. Measured: `01` at a breakpoint
/// in slot 0 (TC377 board, 2026-11-09), `02` in slot 1 (winIDEA capture), `20` after winIDEA's halt,
/// `00` after a step and after a halt sent without winIDEA's attach messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// No breakpoint bit: halted by the debugger, or after a step (which the caller knows).
    Request,
    /// Hardware breakpoint(s) hit: bit n = slot n.
    Breakpoint(u8),
    /// Bits not seen so far.
    Other(u8),
}

/// The breakpoint bits of the stop reason, one per slot.
const STOP_SLOTS: u8 = (1 << BREAKPOINTS) - 1;

/// The SCR's state from a `//2///12/` reply with mask `3f`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub halted: bool,
    pub reason: Stop,
    /// Zero while the SCR runs.
    pub regs: Regs,
}

const STATUS_HALTED: usize = 0xe0;
const STATUS_REASON: usize = 0xe4;
const STATUS_REGS: usize = 0x101;

impl Status {
    /// The probe's view of an SCR its boot ROM stopped at 0x0000 after a reset (in OCDS mode, with a
    /// debugger present): halted, PC 0, SP 0. The probe's link to the debug monitor is stale then: a
    /// step leaves the PC at 0, a run does not start, until [`Probe::scr_initialize`] has been sent.
    /// A real stop at 0x0000 shows SP 0x07 (its reset value) or what the program set.
    pub fn is_boot_entry(&self) -> bool {
        self.halted && self.regs.pc == 0 && self.regs.sp == 0
    }

    /// The SCR waits in its boot ROM after a reset of its debug interface: halted, SP 0xFF, PC outside
    /// the 8 KB of XRAM its program runs from (0x33xx measured, with R1 0x32 and DPTR 4). Seen after
    /// a chip reset, after `PMSWCR4.SCRSTREQ`, and right after [`Probe::scr_initialize`] (which
    /// resets it). Its program runs again only from 0x0000: winIDEA writes PC 0 after its attach.
    pub fn in_boot_rom(&self) -> bool {
        self.halted && self.regs.sp == 0xFF && self.regs.pc as usize >= XRAM_SIZE
    }

    pub fn parse(r: &[u8]) -> Result<Status> {
        service_reply(STATUS, r)?;
        let regs = r.get(STATUS_REGS..).and_then(Regs::decode).ok_or_else(|| Error::Protocol(format!("{STATUS}: reply too short: {}", hex(r, 48))))?;
        let halted = r[STATUS_HALTED] == 1;
        let reason = match r[STATUS_REASON] {
            0x00 | 0x20 => Stop::Request,
            n if n & STOP_SLOTS != 0 && n & !STOP_SLOTS == 0 => Stop::Breakpoint(n),
            n => Stop::Other(n),
        };
        Ok(Status { halted, reason, regs })
    }
}

/// `//2///12/` for the SCR: `mask` `3f` asks for the full state; `flag` is `00` around a halt,
/// `01` otherwise.
pub fn status_body(mask: u32, flag: u8) -> Vec<u8> {
    let mut b = Vec::with_capacity(34);
    b.extend_from_slice(&mask.to_le_bytes());
    b.extend_from_slice(&1u32.to_le_bytes());
    b.extend_from_slice(&(CORE as u32).to_le_bytes());
    b.extend_from_slice(&mask.to_le_bytes());
    b.push(flag);
    b.extend_from_slice(&[0; 4 + 8]);
    b.push(0x01);
    b.extend_from_slice(&[0; 4]);
    b
}

/// `//2///17/`: op `00` halts, op `01` comes before the first run after the attach.
pub fn halt_body(op: u8) -> Vec<u8> {
    let mut b = vec![op];
    b.extend_from_slice(&1u32.to_le_bytes());
    b.extend_from_slice(&(CORE as u32).to_le_bytes());
    b
}

/// `//2///20/`: mode `1` run, `2` step.
pub fn mode_body(mode: u32) -> Vec<u8> {
    let mut b = mode.to_le_bytes().to_vec();
    b.extend_from_slice(&(CORE as u32).to_le_bytes());
    b
}

/// `//3//3/12/`: hardware breakpoint `slot` at `addr`, set or cleared.
pub fn breakpoint_body(slot: u8, enable: bool, addr: u16) -> Vec<u8> {
    let mut b = vec![slot, enable as u8];
    b.extend_from_slice(&(addr as u32).to_le_bytes());
    b
}

/// `//3//3/13/`: `01` or `00`, then 12 zero bytes.
pub fn bp_logic_body(on: bool) -> Vec<u8> {
    let mut b = vec![on as u8];
    b.extend_from_slice(&[0; 12]);
    b
}

/// `//2///18/` of a run: ids 2 and 3, each followed by `01 00 00`.
pub fn run_18_body() -> Vec<u8> {
    let mut b = 2u32.to_le_bytes().to_vec();
    for id in [2u32, 3] {
        b.extend_from_slice(&id.to_le_bytes());
        b.extend_from_slice(&[1, 0, 0]);
    }
    b
}

pub fn run_15_body() -> Vec<u8> {
    (0..RUN_15_BODY.len()).step_by(2).map(|i| u8::from_str_radix(&RUN_15_BODY[i..i + 2], 16).unwrap()).collect()
}

/// `//2////` as winIDEA sends it before each `//3//3/12/`: one access to `PMSWCR4` (4 bytes,
/// `0x0002FE00` at 0x12) carrying `value`, the register's content (`0x020A0000` with the SCR in
/// OCDS SoC DAP mode). Which of the two words is the mask was not tested; the driver sends the
/// register's current value, so the write changes nothing either way.
pub fn pmswcr4_body(value: u32) -> Vec<u8> {
    let mut b = vec![0u8; 38];
    b[0x00..0x04].copy_from_slice(&1u32.to_le_bytes());
    b[0x06..0x0e].copy_from_slice(&(PMSWCR4 as u64).to_le_bytes());
    b[0x0e..0x12].copy_from_slice(&4u32.to_le_bytes());
    b[0x12..0x16].copy_from_slice(&0x0002_FE00u32.to_le_bytes());
    b[0x19] = 0x01;
    b[0x1a] = 0x01;
    b[0x1e..0x22].copy_from_slice(&value.to_le_bytes());
    b
}

/// The memory space and 8051 address of an SCR virtual address (TC3xx UM Table 558): code 0x0000-
/// 0xFFFF and external data 0x10000-0x1FFFF are XRAM (space 0); IRAM 0x20000-0x2007F and standard
/// SFRs (0x40080-0x400FF: page 0, RMAP 0) are data space (space 1). Upper IRAM (indirect 0x80-0xFF),
/// paged and mapped SFRs have no address in the probe's spaces.
pub fn space_of(vaddr: u32) -> Result<(u8, u16)> {
    match vaddr {
        0x0_0000..=0x0_FFFF => Ok((SPACE_CODE, vaddr as u16)),
        0x1_0000..=0x1_FFFF => Ok((SPACE_CODE, (vaddr - 0x1_0000) as u16)),
        0x2_0000..=0x2_007F => Ok((SPACE_DATA, (vaddr - 0x2_0000) as u16)),
        0x4_0000..=0x4_007F => Ok((SPACE_DATA, 0x80 | (vaddr & 0x7F) as u16)),
        _ => Err(Error::Protocol(format!("SCR virtual address 0x{vaddr:05x} has no address in the probe's memory spaces"))),
    }
}

/// A probe session's view of the SCR: the work of [`Probe::scr_run`] and friends that depends on
/// what came before.
#[derive(Debug, Clone, Default)]
pub struct Session {
    /// The first run after the attach sends `//2///17/` op 1 and `//3//3/13/` first.
    pub ran: bool,
    /// Registers changed while halted, sent with the next step or run (as winIDEA does).
    pub pending: Option<Regs>,
    /// Breakpoint address per slot.
    pub breakpoints: [Option<u16>; BREAKPOINTS],
    /// The SCR waits in its boot ROM ([`Status::in_boot_rom`]) and the restart at 0x0000 is
    /// queued in [`Session::pending`].
    pub restart: bool,
}

impl<L: Link> Probe<L> {
    fn scr_call(&mut self, path: &str, body: &[u8]) -> Result<Vec<u8>> {
        let r = self.exchange(&exchange(path, body))?.remove(0);
        service_reply(path, &r)?;
        Ok(r)
    }

    /// Starts an SCR session (see [`Probe::scr_status_synced`]).
    ///
    /// winIDEA's attach always sends IsDebugAccess, OCDSInitialize, DebugEstablished,
    /// ReadCoreStatus and DoConfigOnchipBreakpointLogic ([`Probe::scr_initialize`]). Measured on
    /// the TC377 board: sent to an SCR that runs its program, they leave it halted at an address
    /// outside the program (in unprogrammed XRAM, executing `FF` bytes once resumed), and winIDEA
    /// then restarts the program at 0x0000 by writing the registers ("re-init SCR" in its log).
    /// Sent after the SCR was reset ([`Status::is_boot_entry`]), they are what makes it debuggable.
    /// So they are sent only then.
    pub fn scr_attach(&mut self) -> Result<Session> {
        let mut s = Session::default();
        self.scr_status_synced(&mut s)?;
        Ok(s)
    }

    /// [`Probe::scr_status`], with the probe's link to the debug monitor set up again first when the
    /// SCR was reset since ([`Status::is_boot_entry`]): the first run afterwards is a first run
    /// after an attach again.
    ///
    /// An SCR waiting in its boot ROM ([`Status::in_boot_rom`]) gets its program restarted: the
    /// monitor link is set up again (once), PC 0 and zeroed registers are queued for the next step
    /// or run, and the status shows the SCR halted at 0x0000, as after a halting reset.
    pub fn scr_status_synced(&mut self, s: &mut Session) -> Result<Status> {
        let mut st = self.scr_status()?;
        let mut initialized = false;
        if st.is_boot_entry() {
            self.scr_initialize()?;
            initialized = true;
            s.ran = false;
            s.pending = None;
            s.restart = false;
            st = self.scr_status()?;
        }
        if st.in_boot_rom() {
            if !s.restart {
                if !initialized {
                    self.scr_initialize()?;
                }
                s.ran = false;
                s.pending = Some(Regs::default());
                s.restart = true;
            }
            st.regs = s.pending.unwrap_or_default();
            st.reason = Stop::Request;
        }
        Ok(st)
    }

    /// winIDEA's SCR initialisation (see [`Probe::scr_attach`]).
    pub fn scr_initialize(&mut self) -> Result<()> {
        for (path, body) in [(IS_DEBUG_ACCESS, &[0u8][..]), (OCDS_INITIALIZE, &[0]), (DEBUG_ESTABLISHED, &[0]), (CORE_STATUS, &[0])] {
            self.scr_call(path, body)?;
        }
        self.scr_call(BP_LOGIC, &bp_logic_body(true))?;
        self.scr_call(BP_LOGIC, &bp_logic_body(false))?;
        Ok(())
    }

    /// Run state, stop reason and registers.
    pub fn scr_status(&mut self) -> Result<Status> {
        let r = self.scr_call(STATUS, &status_body(0x3f, 1))?;
        Status::parse(&r)
    }

    pub fn scr_halt(&mut self) -> Result<()> {
        self.scr_call(STATUS, &status_body(0x3f, 0))?;
        self.scr_call(HALT, &halt_body(0))?;
        self.scr_call(STATUS, &status_body(0x3f, 0))?;
        Ok(())
    }

    /// `//2///20/` and the registers changed since the last stop.
    fn scr_prepare(&mut self, s: &mut Session, mode: u32) -> Result<()> {
        s.restart = false;
        self.scr_call(MODE, &mode_body(mode))?;
        if let Some(r) = s.pending.take() {
            self.scr_call(WRITE_REGS, &r.encode())?;
        }
        Ok(())
    }

    /// One instruction.
    pub fn scr_step(&mut self, s: &mut Session) -> Result<()> {
        self.scr_prepare(s, 2)?;
        self.scr_call(STATUS, &status_body(0x20, 1))?;
        self.scr_call(TARGET_RUN, &[1, 0])?;
        self.scr_call(CORE_STATUS, &[0])?;
        self.scr_call(STATUS, &status_body(0x20, 1))?;
        Ok(())
    }

    /// Lets the SCR run. Off a breakpoint at `pc` it first steps with that breakpoint cleared, as
    /// winIDEA does.
    pub fn scr_run(&mut self, s: &mut Session, pc: u16) -> Result<()> {
        if !s.ran {
            self.scr_call(HALT, &halt_body(1))?;
            self.scr_call(BP_LOGIC, &bp_logic_body(true))?;
            self.scr_call(BP_LOGIC, &bp_logic_body(false))?;
            s.ran = true;
        }
        self.scr_prepare(s, 1)?;
        self.scr_call(STATUS, &status_body(0x3f, 1))?;
        let under = s.breakpoints.iter().position(|b| *b == Some(pc));
        if let Some(slot) = under {
            self.scr_breakpoint(slot, false, pc)?;
        }
        self.scr_call(TARGET_RUN, &[1, 1])?;
        self.scr_call(CORE_STATUS, &[0])?;
        if let Some(slot) = under {
            self.scr_breakpoint(slot, true, pc)?;
        }
        self.scr_call(STATUS, &status_body(0x20, 1))?;
        self.scr_call(GO, &1u32.to_le_bytes())?;
        self.scr_call(GO, &0u32.to_le_bytes())?;
        self.scr_call(RUN_15, &run_15_body())?;
        self.scr_call(RUN_16, &[])?;
        self.scr_call(RUN_18, &run_18_body())?;
        Ok(())
    }

    /// Sets or clears hardware breakpoint `slot`.
    fn scr_breakpoint(&mut self, slot: usize, enable: bool, addr: u16) -> Result<()> {
        let pmswcr4 = self.read_memory(PMSWCR4, 4)?;
        let value = u32::from_le_bytes(pmswcr4.data[..4].try_into().unwrap());
        self.scr_call(MASKED_WRITE, &pmswcr4_body(value))?;
        self.scr_call(STATUS, &status_body(0x3f, 1))?;
        self.scr_call(BREAKPOINT, &breakpoint_body(slot as u8, enable, addr))?;
        Ok(())
    }

    pub fn scr_set_breakpoint(&mut self, s: &mut Session, slot: usize, addr: Option<u16>) -> Result<()> {
        if slot >= BREAKPOINTS {
            return Err(Error::Protocol(format!("SCR breakpoint {slot}: there are {BREAKPOINTS}")));
        }
        match (addr, s.breakpoints[slot]) {
            (Some(a), _) => self.scr_breakpoint(slot, true, a)?,
            (None, Some(old)) => self.scr_breakpoint(slot, false, old)?,
            (None, None) => {}
        }
        s.breakpoints[slot] = addr;
        Ok(())
    }

    fn scr_access(&mut self, vaddr: u32, len: usize, data: Option<&[u8]>) -> Result<MemReply> {
        let (space, addr) = space_of(vaddr)?;
        let last = vaddr.checked_add(len.saturating_sub(1) as u32).ok_or_else(|| Error::Protocol("SCR address overflow".into()))?;
        if len > 0 && space_of(last)?.0 != space {
            return Err(Error::Protocol(format!("SCR access 0x{vaddr:05x}+{len} crosses a memory space")));
        }
        let r = self.exchange(&crate::probe::Exchange {
            payload: memory::core_request(CORE, space, addr as u32, len, data),
            ..exchange(memory::SERVICE, &[])
        })?;
        memory::parse_reply(&r[0], if data.is_some() { 0 } else { len })
    }

    /// Reads SCR memory at a virtual address (see [`space_of`]).
    pub fn scr_read(&mut self, vaddr: u32, len: usize) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(len);
        let mut off = 0;
        while off < len {
            let n = (len - off).min(MAX_BLOCK);
            let a = vaddr + off as u32;
            let m = self.scr_access(a, n, None)?;
            if m.data.len() != n || m.failed.iter().any(|&f| f) {
                return Err(Error::Protocol(format!("SCR read 0x{a:05x}+{n} failed")));
            }
            out.extend_from_slice(&m.data);
            off += n;
        }
        Ok(out)
    }

    /// Writes SCR memory at a virtual address (see [`space_of`]).
    pub fn scr_write(&mut self, vaddr: u32, data: &[u8]) -> Result<()> {
        for (i, chunk) in data.chunks(MAX_BLOCK).enumerate() {
            let a = vaddr + (i * MAX_BLOCK) as u32;
            let m = self.scr_access(a, chunk.len(), Some(chunk))?;
            if m.failed.iter().any(|&f| f) {
                return Err(Error::Protocol(format!("SCR write 0x{a:05x}+{} failed", chunk.len())));
            }
        }
        Ok(())
    }
}

/// Whether `r` is a plain acknowledgement: its payload is the path's bytes 7 and 8 (`0/` for
/// `//2///20/`, `11` for `//3//3/11/`, `9/` for `//3//3/9/`).
pub fn is_ack(path: &str, r: &[u8]) -> bool {
    message::is_reply(r) && path.len() >= 9 && r.get(0x1e..0x20) == Some(&path.as_bytes()[7..9])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The captured exchanges (`scr_fixtures.txt`): label, path, source,
    /// `req=`, `rsp=`.
    const FIXTURES: &str = include_str!("scr_fixtures.txt");

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// (request body, reply) of the fixture whose label starts with `label`.
    fn fixture(label: &str) -> (String, Vec<u8>, Vec<u8>) {
        let line = FIXTURES.lines().find(|l| l.starts_with(label)).unwrap_or_else(|| panic!("no fixture {label}"));
        let f: Vec<&str> = line.split('\t').collect();
        let req = unhex(f[3].strip_prefix("req=").unwrap());
        let (_, path, body) = message::decode(&req).unwrap();
        (path.to_string(), body.to_vec(), unhex(f[4].strip_prefix("rsp=").unwrap()))
    }

    fn body(label: &str) -> Vec<u8> {
        fixture(label).1
    }

    #[test]
    fn requests_are_what_winidea_sends() {
        assert_eq!(status_body(0x3f, 0), body("status mask 3f flag 0, SCR running"));
        assert_eq!(status_body(0x3f, 1), body("status mask 3f flag 1, halted after step"));
        assert_eq!(status_body(0x20, 1), body("status mask 20, halted"));
        assert_eq!(status_body(0x60, 1), body("status mask 60, halted"));
        assert_eq!(halt_body(0), body("halt request (17 op 00)"));
        assert_eq!(halt_body(1), body("17 op 01"));
        assert_eq!(mode_body(2), body("20 step"));
        assert_eq!(mode_body(1), body("20 run"));
        assert_eq!(breakpoint_body(0, true, 0xee), body("HW breakpoint slot 0 set 0xee"));
        assert_eq!(breakpoint_body(1, true, 0x1bd), body("HW breakpoint slot 1 set 0x1bd"));
        assert_eq!(breakpoint_body(1, false, 0x1bd), body("resume from 0x1bd: slot 1 off"));
        assert_eq!(breakpoint_body(0, false, 0xee), body("HW breakpoint slot 0 clear"));
        assert_eq!(bp_logic_body(true), body("DoConfigOnchipBreakpointLogic 01"));
        assert_eq!(bp_logic_body(false), body("DoConfigOnchipBreakpointLogic 00"));
        assert_eq!(run_15_body(), body("15 (run)"));
        assert_eq!(body("16 (run, empty body)"), Vec::<u8>::new());
        assert_eq!(run_18_body(), body("18 (run, cores 2 and 3)"));
        assert_eq!(pmswcr4_body(0x020A_0000), body("masked write PMSWCR4"));
        assert_eq!(body("TargetRun step (11 01 00)"), vec![1, 0]);
        assert_eq!(body("TargetRun run (11 01 01)"), vec![1, 1]);
        assert_eq!(body("ReadCoreStatus"), vec![0]);
        assert_eq!(body("3/ 01 (run)"), 1u32.to_le_bytes());
        assert_eq!(body("3/ 00 (run)"), 0u32.to_le_bytes());
        let (path, _, _) = fixture("HW breakpoint slot 0 set 0xee");
        assert_eq!(path, BREAKPOINT);
        assert_eq!(fixture("masked write PMSWCR4").0, MASKED_WRITE);
    }

    #[test]
    fn register_block() {
        let b = body("write registers PC=13b ACC=b4 R7=ee SP=24");
        let r = Regs::decode(&b).unwrap();
        assert_eq!((r.pc, r.acc, r.b, r.psw, r.dptr, r.r[7], r.sp), (0x13b, 0xb4, 0, 0, 0x15f, 0xee, 0x24));
        assert_eq!(r.encode().to_vec(), b);
        assert_eq!(Regs::decode(&body("write registers B=5a")).unwrap().b, 0x5a);
    }

    #[test]
    fn status_replies() {
        let s = Status::parse(&fixture("status mask 3f flag 0, SCR running").2).unwrap();
        assert!(!s.halted);
        assert_eq!(s.regs, Regs::default());
        let s = Status::parse(&fixture("status mask 3f flag 0, SCR halted by stop").2).unwrap();
        assert!(s.halted);
        assert_eq!(s.reason, Stop::Request);
        // A step leaves no reason bits: the caller knows it stepped.
        let s = Status::parse(&fixture("status mask 3f flag 1, halted after step").2).unwrap();
        assert_eq!((s.halted, s.reason), (true, Stop::Request));
        // The breakpoint in slot 1.
        let s = Status::parse(&fixture("status mask 3f flag 1, stopped at breakpoint 0x1bd").2).unwrap();
        assert_eq!((s.halted, s.reason, s.regs.pc), (true, Stop::Breakpoint(0b10), 0x1bd));
        assert!(!s.is_boot_entry());
        // After an SCR reset (TC377 board): halted, PC 0, SP 0, R7 1.
        let boot = Status { halted: true, reason: Stop::Request, regs: Regs { r: [0, 0, 0, 0, 0, 0, 0, 1], ..Regs::default() } };
        assert!(boot.is_boot_entry());
        assert!(!Status { halted: true, reason: Stop::Request, regs: Regs { sp: 7, ..Regs::default() } }.is_boot_entry());
        assert!(!Status { halted: false, ..boot }.is_boot_entry());
        // Waiting in the boot ROM (TC377 board): PC 0x33fa, R1 0x32, DPTR 4, SP 0xff.
        let rom = Regs::decode(&[0xfa, 0x33, 0, 0, 0, 0x04, 0, 0, 0x32, 0, 0, 0, 0, 0, 0, 0xff]).unwrap();
        assert!(Status { halted: true, reason: Stop::Request, regs: rom }.in_boot_rom());
        assert!(!Status { halted: false, reason: Stop::Request, regs: rom }.in_boot_rom());
        let prog = Regs::decode(&[0xbd, 0x01, 0x0c, 0, 0, 0x5f, 0x01, 0, 0, 0, 0, 0, 0, 0, 0xb4, 0x24]).unwrap();
        assert!(!Status { halted: true, reason: Stop::Request, regs: prog }.in_boot_rom());
        assert!(is_ack(BREAKPOINT, &fixture("HW breakpoint slot 0 set 0xee").2));
        assert!(is_ack(TARGET_RUN, &fixture("TargetRun step").2));
    }

    #[test]
    fn memory_requests_are_what_winidea_sends() {
        let cases = [
            ("mem read code 0x1fff 1", SPACE_CODE, 0x1fff, 1, None),
            ("mem write code 0x1fff 1 = 55", SPACE_CODE, 0x1fff, 1, Some(vec![0x55])),
            ("mem write code 0x1ffc 4 = aa55aa55", SPACE_CODE, 0x1ffc, 4, Some(vec![0xaa, 0x55, 0xaa, 0x55])),
            ("mem read data 0x7f 1", SPACE_DATA, 0x7f, 1, None),
            ("mem write data 0x7f 1 = ff", SPACE_DATA, 0x7f, 1, Some(vec![0xff])),
            ("mem write SFR 0xf0 1 = d2", SPACE_DATA, 0xf0, 1, Some(vec![0xd2])),
        ];
        for (label, space, addr, len, data) in cases {
            let (_, want, rsp) = fixture(label);
            let got = memory::core_request(CORE, space, addr, len, data.as_deref());
            assert_eq!(message::decode(&got).unwrap().2, &want[..], "{label}");
            let m = memory::parse_reply(&rsp, if data.is_some() { 0 } else { len }).unwrap();
            assert!(m.failed.iter().all(|f| !f), "{label}");
        }
        let (_, _, rsp) = fixture("mem read code 0x1ffc 4");
        assert_eq!(memory::parse_reply(&rsp, 4).unwrap().data, vec![0xaa, 0x55, 0xaa, 0x55]);
    }

    #[test]
    fn virtual_addresses() {
        assert_eq!(space_of(0x0079).unwrap(), (SPACE_CODE, 0x79));
        assert_eq!(space_of(0x1_1FFF).unwrap(), (SPACE_CODE, 0x1FFF));
        assert_eq!(space_of(0x2_0007).unwrap(), (SPACE_DATA, 0x07));
        assert_eq!(space_of(0x4_0060).unwrap(), (SPACE_DATA, 0xE0), "ACC");
        assert!(space_of(0x2_0080).is_err() && space_of(0x4_0100).is_err() && space_of(0x8_0000).is_err());
    }
}
