//! Program trace events -> executed instructions.
//!
//! Rules (verified against winIDEA's decoded exports, 33,320 of 33,321 samples identical):
//! - A count ending in a **direct** branch/call/loop continues at the target from the code.
//! - A count ending in anything else (indirect jump/call, return, interrupt) leaves the pc unknown;
//!   the next address message is the next instruction.
//! - An address message while the pc is known is a **sync point**: the flow ran sequentially up
//!   to it (TT0 about once per paragraph, also inside sequential code).
//! - A count of 0: the instruction at the pc (a branch target) is itself the next discontinuity.
//! - Trace off: `count` more bytes, then nothing is known until the next address message (a gap).

use crate::decode::PtuEvent;

/// What the flow needs to know about the instruction at an address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Insn {
    pub size: u8,
    /// Target of a direct branch, call or loop.
    pub direct: Option<u32>,
    /// Calls and returns, for the return addresses the flow keeps.
    pub kind: InsnKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InsnKind {
    #[default]
    Other,
    Call,
    Ret,
}

/// Return addresses kept for an unresolved final return (see [`Flow::run_to`]).
const CALL_DEPTH: usize = 64;

/// Instruction lookup (code image + decoder).
pub trait Code {
    fn insn(&mut self, addr: u32) -> Option<Insn>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// An executed instruction.
    Insn { addr: u32, time: Option<u64> },
    /// Execution not recorded between the neighbours (trace off, lost messages, no code).
    Gap { time: Option<u64>, reason: GapReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapReason {
    TraceOff,
    Lost,
    NoCode,
    Inconsistent,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FlowStats {
    pub instructions: u64,
    pub direct: u64,
    pub sync_fills: u64,
    pub zero_counts: u64,
    pub trace_off: u64,
    pub inconsistent: u64,
    pub no_code: u64,
}

/// Longest stretch a sync point may fill sequentially.
const SYNC_MAX: u32 = 0x2000;

#[derive(Default)]
pub struct Flow {
    pc: Option<u32>,
    /// Return addresses of the calls seen (bounded; cleared on a gap).
    calls: Vec<u32>,
    /// The pc is unknown after a return: where its call said it returns to.
    ret_to: Option<u32>,
    pub stats: FlowStats,
}

impl Flow {
    pub fn new() -> Self {
        Self::default()
    }

    /// Starts the flow at a known PC (where the core stood when the recording started).
    pub fn start_at(&mut self, pc: u32) {
        self.pc = Some(pc);
    }

    /// Where the flow continues after the last event (`None`: unknown, e.g. after an indirect
    /// branch whose target message was not recorded).
    pub fn pc(&self) -> Option<u32> {
        self.pc
    }

    /// Runs on sequentially from the pc to `target` if no branch is needed to get there: with
    /// every message flushed, the code after the last message up to a halt is straight (any
    /// discontinuity in between would have produced a message). Returns whether it got there.
    ///
    /// When the trace ends with a return whose target was not reported (the PTU sends it with the
    /// next stretch, which a halt right after the return never produces), the return address of
    /// its call is used, if the straight code from there reaches `target`.
    pub fn run_to(&mut self, target: u32, time: Option<u64>, code: &mut dyn Code, out: &mut Vec<Step>) -> bool {
        let Some(mut a) = self.pc.or(self.ret_to) else { return false };
        if target.wrapping_sub(a) > SYNC_MAX {
            return a == target;
        }
        let mut steps = Vec::new();
        while a != target {
            let Some(i) = code.insn(a) else { return false };
            steps.push(Step::Insn { addr: a, time });
            a = a.wrapping_add(i.size as u32);
            if target.wrapping_sub(a) > SYNC_MAX {
                return false;
            }
        }
        self.stats.instructions += steps.len() as u64;
        out.extend(steps);
        self.pc = Some(target);
        true
    }

    /// Data was lost: the pc is unknown until the next address message.
    pub fn gap(&mut self, out: &mut Vec<Step>, time: Option<u64>) {
        self.calls.clear();
        self.ret_to = None;
        if self.pc.take().is_some() {
            out.push(Step::Gap { time, reason: GapReason::Lost });
        }
    }

    /// Executes `n` bytes from the pc; returns the last instruction.
    fn run(&mut self, code: &mut dyn Code, n: u32, time: Option<u64>, out: &mut Vec<Step>) -> Result<Option<Insn>, GapReason> {
        let Some(mut a) = self.pc else { return Ok(None) };
        let mut left = n as i64;
        let mut last = None;
        while left > 0 {
            let Some(i) = code.insn(a) else {
                self.stats.no_code += 1;
                return Err(GapReason::NoCode);
            };
            out.push(Step::Insn { addr: a, time });
            self.stats.instructions += 1;
            last = Some(i);
            match i.kind {
                InsnKind::Call => {
                    if self.calls.len() == CALL_DEPTH {
                        self.calls.remove(0);
                    }
                    self.calls.push(a.wrapping_add(i.size as u32));
                }
                InsnKind::Ret => self.ret_to = self.calls.pop(),
                InsnKind::Other => {}
            }
            a = a.wrapping_add(i.size as u32);
            left -= i.size as i64;
        }
        if left != 0 {
            self.stats.inconsistent += 1;
            return Err(GapReason::Inconsistent);
        }
        self.pc = Some(a);
        Ok(last)
    }

    pub fn feed(&mut self, ev: PtuEvent, time: Option<u64>, code: &mut dyn Code, out: &mut Vec<Step>) {
        match ev {
            PtuEvent::Addr(v) => {
                if let Some(pc) = self.pc {
                    let d = v.wrapping_sub(pc);
                    if d > 0 && d <= SYNC_MAX {
                        let mut a = pc;
                        while a != v {
                            let Some(i) = code.insn(a) else { break };
                            out.push(Step::Insn { addr: a, time });
                            self.stats.instructions += 1;
                            a = a.wrapping_add(i.size as u32);
                            if v.wrapping_sub(a) > SYNC_MAX {
                                break;
                            }
                        }
                        self.stats.sync_fills += 1;
                        if a != v {
                            self.stats.inconsistent += 1;
                            out.push(Step::Gap { time, reason: GapReason::Inconsistent });
                        }
                    }
                }
                self.pc = Some(v);
                self.ret_to = None;
            }
            PtuEvent::Count(n) => {
                if self.pc.is_none() {
                    return;
                }
                let n = if n == 0 {
                    self.stats.zero_counts += 1;
                    match self.pc.and_then(|a| code.insn(a)) {
                        Some(i) => i.size as u32,
                        None => 0,
                    }
                } else {
                    n
                };
                match self.run(code, n, time, out) {
                    Ok(Some(last)) => match last.direct {
                        Some(t) => {
                            self.pc = Some(t);
                            self.stats.direct += 1;
                        }
                        None => self.pc = None,
                    },
                    Ok(None) => {}
                    Err(reason) => {
                        self.pc = None;
                        out.push(Step::Gap { time, reason });
                    }
                }
            }
            PtuEvent::Off { count, .. } => {
                self.stats.trace_off += 1;
                if self.pc.is_some() && count > 0 {
                    let _ = self.run(code, count, time, out);
                }
                self.pc = None;
                out.push(Step::Gap { time, reason: GapReason::TraceOff });
            }
        }
    }
}

/// [`Code`] over a memory image with Capstone's TriCore decoder.
#[cfg(feature = "capstone")]
pub struct CapstoneCode<R: FnMut(u32, usize) -> Option<Vec<u8>>> {
    cs: capstone::Capstone,
    read: R,
    cache: std::collections::HashMap<u32, Option<Insn>>,
    text: std::collections::HashMap<u32, Option<crate::replay::Decoded>>,
}

#[cfg(feature = "capstone")]
impl<R: FnMut(u32, usize) -> Option<Vec<u8>>> CapstoneCode<R> {
    /// `read(addr, len)` returns code bytes (from the ELF; the uncached 0xA segment is mapped to
    /// 0x8 by the caller or here).
    pub fn new(read: R) -> Result<Self, String> {
        use capstone::arch::tricore::ArchMode;
        use capstone::prelude::*;
        let cs = Capstone::new().tricore().mode(ArchMode::TriCore162).detail(true).build().map_err(|e| e.to_string())?;
        Ok(Self { cs, read, cache: Default::default(), text: Default::default() })
    }
}

#[cfg(feature = "capstone")]
impl<R: FnMut(u32, usize) -> Option<Vec<u8>>> crate::replay::Disasm for CapstoneCode<R> {
    fn disasm(&mut self, addr: u32) -> Option<crate::replay::Decoded> {
        if let Some(t) = self.text.get(&addr) {
            return t.clone();
        }
        let a = if addr >> 28 == 0xA { (addr & 0x0FFF_FFFF) | 0x8000_0000 } else { addr };
        let r = (|| {
            let bytes = (self.read)(a, 4)?;
            let insns = self.cs.disasm_count(&bytes, addr as u64, 1).ok()?;
            let i = insns.iter().next()?;
            Some(crate::replay::Decoded { size: i.bytes().len() as u8, mnemonic: i.mnemonic()?.to_string(), ops: i.op_str().unwrap_or("").to_string() })
        })();
        self.text.insert(addr, r.clone());
        r
    }
}

#[cfg(feature = "capstone")]
fn is_direct(mnemonic: &str) -> bool {
    let m = mnemonic;
    (m.starts_with('j') || m.starts_with("call") || m.starts_with("loop") || m.starts_with("fcall"))
        && !matches!(m, "ji" | "jli" | "calli" | "fcalli")
}

#[cfg(feature = "capstone")]
impl<R: FnMut(u32, usize) -> Option<Vec<u8>>> Code for CapstoneCode<R> {
    fn insn(&mut self, addr: u32) -> Option<Insn> {
        if let Some(c) = self.cache.get(&addr) {
            return *c;
        }
        let a = if addr >> 28 == 0xA { (addr & 0x0FFF_FFFF) | 0x8000_0000 } else { addr };
        let r = (|| {
            let bytes = (self.read)(a, 4)?;
            let insns = self.cs.disasm_count(&bytes, addr as u64, 1).ok()?;
            let i = insns.iter().next()?;
            let mnemonic = i.mnemonic().unwrap_or("");
            let mut direct = None;
            if is_direct(mnemonic) {
                if let Ok(d) = self.cs.insn_detail(i) {
                    use capstone::arch::ArchOperand;
                    for op in d.arch_detail().operands() {
                        if let ArchOperand::TriCoreOperand(capstone::arch::tricore::TriCoreOperand::Imm(v)) = op {
                            direct = Some(v as u32);
                        }
                    }
                }
            }
            let kind = match mnemonic {
                "call" | "calla" | "calli" | "fcall" | "fcalla" | "fcalli" => InsnKind::Call,
                "ret" | "fret" => InsnKind::Ret,
                _ => InsnKind::Other,
            };
            Some(Insn { size: i.bytes().len() as u8, direct, kind })
        })();
        self.cache.insert(addr, r);
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn unresolved_final_return_uses_the_call() {
        // 0x100 call 0x200 (direct); 0x200..0x204 then ret; the halt is at 0x104, the return
        // address, but the PTU never reported the return's target.
        let mut m = HashMap::new();
        m.insert(0x100, Insn { size: 4, direct: Some(0x200), kind: InsnKind::Call });
        m.insert(0x104, Insn { size: 2, direct: None, kind: InsnKind::Other });
        m.insert(0x200, Insn { size: 4, direct: None, kind: InsnKind::Other });
        m.insert(0x204, Insn { size: 2, direct: None, kind: InsnKind::Ret });
        let mut c = Map(m);
        let mut f = Flow::new();
        let mut out = Vec::new();
        f.feed(PtuEvent::Addr(0x100), None, &mut c, &mut out);
        f.feed(PtuEvent::Count(4), None, &mut c, &mut out); // the call
        f.feed(PtuEvent::Count(6), None, &mut c, &mut out); // 0x200, ret
        assert_eq!(f.pc(), None);
        assert!(f.run_to(0x104, None, &mut c, &mut out));
        assert_eq!(addrs(&out), vec![0x100, 0x200, 0x204]);
        // A halt elsewhere is not explained by it.
        let mut f2 = Flow::new();
        let mut out2 = Vec::new();
        f2.feed(PtuEvent::Addr(0x100), None, &mut c, &mut out2);
        f2.feed(PtuEvent::Count(4), None, &mut c, &mut out2);
        f2.feed(PtuEvent::Count(6), None, &mut c, &mut out2);
        assert!(!f2.run_to(0x300, None, &mut c, &mut out2));
    }

    /// A tiny code image: (addr -> size, direct target).
    struct Map(HashMap<u32, Insn>);
    impl Code for Map {
        fn insn(&mut self, a: u32) -> Option<Insn> {
            self.0.get(&a).copied()
        }
    }
    fn code(list: &[(u32, u8, Option<u32>)]) -> Map {
        Map(list.iter().map(|&(a, s, d)| (a, Insn { size: s, direct: d, kind: InsnKind::Other })).collect())
    }
    fn addrs(steps: &[Step]) -> Vec<u32> {
        steps.iter().filter_map(|s| if let Step::Insn { addr, .. } = s { Some(*addr) } else { None }).collect()
    }

    #[test]
    fn direct_indirect_sync_and_zero_count() {
        // 0x100: 4-byte jlt -> 0x110 ; 0x110: ld 4, j16 -> 0x120 ; 0x120: ret (indirect)
        // 0x200 (return target): mov 2, mov 2 ; ... sync at 0x204
        let mut c = code(&[
            (0x100, 4, Some(0x110)),
            (0x110, 4, None),
            (0x114, 2, Some(0x120)),
            (0x120, 2, None),
            (0x200, 2, None),
            (0x202, 2, None),
            (0x204, 4, Some(0x300)),
            (0x300, 4, Some(0x400)),
            (0x400, 2, None),
        ]);
        let mut f = Flow::new();
        let mut out = Vec::new();
        for ev in [
            PtuEvent::Addr(0x100),
            PtuEvent::Count(4),     // jlt taken -> 0x110
            PtuEvent::Count(6),     // ld, j -> 0x120
            PtuEvent::Count(2),     // ret
            PtuEvent::Addr(0x200),  // return target
            PtuEvent::Addr(0x204),  // sync: 0x200, 0x202 ran sequentially
            PtuEvent::Count(4),     // jump at 0x204 -> 0x300
            PtuEvent::Count(0),     // the call at the target itself -> 0x400
            PtuEvent::Off { count: 2, flags: 0 },
        ] {
            f.feed(ev, None, &mut c, &mut out);
        }
        assert_eq!(addrs(&out), vec![0x100, 0x110, 0x114, 0x120, 0x200, 0x202, 0x204, 0x300, 0x400]);
        assert!(matches!(out.last(), Some(Step::Gap { reason: GapReason::TraceOff, .. })));
        assert_eq!(f.stats.inconsistent, 0);
        assert_eq!(f.stats.zero_counts, 1);
        assert_eq!(f.stats.sync_fills, 1);
    }

    #[test]
    fn started_at_a_known_pc() {
        // The first stretch (0x100..0x104, jump to 0x110) comes before any address message.
        let mut c = code(&[(0x100, 2, None), (0x102, 2, Some(0x110)), (0x110, 2, None), (0x112, 2, None)]);
        let mut f = Flow::new();
        let mut out = Vec::new();
        f.feed(PtuEvent::Count(4), None, &mut c, &mut out);
        assert!(out.is_empty(), "unknown pc: the count is dropped");
        let mut f = Flow::new();
        f.start_at(0x100);
        f.feed(PtuEvent::Count(4), None, &mut c, &mut out);
        assert!(f.run_to(0x112, None, &mut c, &mut out));
        assert_eq!(addrs(&out), vec![0x100, 0x102, 0x110]);
    }
}
