//! Register replay: the registers at every instruction of a program trace, rebuilt from the
//! state at the halt.
//!
//! Flow trace records which instructions ran, not what they computed. A trace that ends at a
//! halt, however, ends where every register and memory is known; from there values spread
//! backwards (operations that can be undone, branch outcomes, stores whose memory is known) and
//! forwards (constants, operations with known inputs). Calls and returns go through the context
//! save areas, whose contents are still in memory at the halt unless the area was used again.
//! Each value is exact, taken from memory at the halt ([`Mark::Mem`]: another bus master could
//! have changed it), or unknown; nothing is guessed.

pub mod model;
pub mod operand;
pub mod solve;

use std::collections::HashMap;

use crate::flow::Step;
pub use model::{NREG, PCXH, PCXL, PSWC, PSWF};
use model::{Builder, Next, FCX, ICR};
pub use solve::{How, Mark, MemFrom};
use solve::{Class, MemSource, Solver, Val, Ver, PCXI_LINK, PSW_FLAGS};

/// One decoded instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    pub size: u8,
    pub mnemonic: String,
    pub ops: String,
}

pub trait Disasm {
    fn disasm(&mut self, addr: u32) -> Option<Decoded>;
}

/// Registers at the halt, in the debugger's numbering (d0-d15, a0-a15, lcx, fcx, pcxi, psw, pc,
/// icr, isp, btv, biv, syscon).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HaltRegs(pub Vec<u32>);

impl HaltRegs {
    fn get(&self, i: usize) -> Option<u32> {
        self.0.get(i).copied()
    }
    pub fn pc(&self) -> Option<u32> {
        self.get(36)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// An executed instruction (`step`: index into the decoder's steps).
    Insn { step: usize, addr: u32, d: Decoded },
    /// An interrupt or trap entered before the next instruction; `resume`: where it returns to.
    Entry { step: usize, resume: Option<u32>, vector: u32 },
    /// Code ran that the trace does not show.
    Gap { step: usize },
}

/// A register as shown: value, which bits are known, how sure and where it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegOut {
    pub name: &'static str,
    pub value: u32,
    /// Known bits (all, none, or the PSW without its status flags).
    pub known: u32,
    pub mark: Option<Mark>,
    pub row: Option<u32>,
    pub how: Option<How>,
}

impl RegOut {
    pub fn is_known(&self) -> bool {
        self.known == u32::MAX
    }
}

/// A memory access of a row, for the effects column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessOut {
    pub store: bool,
    pub addr: Option<u32>,
    pub len: u8,
    /// The data, when every word of it is known.
    pub value: Option<Vec<u32>>,
    pub context: bool,
}

pub const VISIBLE: [&str; 36] = [
    "d0", "d1", "d2", "d3", "d4", "d5", "d6", "d7", "d8", "d9", "d10", "d11", "d12", "d13", "d14", "d15", "a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7",
    "a8", "a9", "a10", "a11", "a12", "a13", "a14", "a15", "psw", "pcxi", "fcx", "icr",
];

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stats {
    pub rows: usize,
    pub entries: usize,
    /// Instructions the model does not know (mnemonic, count), most frequent first.
    pub unmodelled: Vec<(String, u32)>,
    pub conflicts: u32,
    /// Whether the replay starts from the halt state (the flow reaches the halt).
    pub anchored: bool,
}

pub struct Replay {
    pub rows: Vec<Row>,
    solver: Solver,
    initial: [Ver; NREG],
    after: Vec<[Ver; NREG]>,
    /// Per row: the first and last access index.
    acc_of: Vec<(usize, usize)>,
    pub stats: Stats,
    /// The halt PC, when anchored.
    pub halt_pc: Option<u32>,
}

/// Memory for the solver: aliases folded, blocks of 64 bytes read once.
struct Mem<'a> {
    core: u8,
    read: &'a mut dyn FnMut(u32, usize) -> Option<Vec<u8>>,
    cache: HashMap<u32, Option<Vec<u8>>>,
}

/// Global address and class of an address seen by core `core` (TC3xx address map).
pub fn place(core: u8, a: u32) -> (u32, Class) {
    let dspr = 0x7000_0000u32.wrapping_sub(core as u32 * 0x1000_0000);
    let a = match a >> 20 {
        0xD00..=0xD0F => dspr + (a & 0xF_FFFF),
        0xC00..=0xC0F => dspr + 0x10_0000 + (a & 0xF_FFFF),
        0xA00..=0xAEF => 0x8000_0000 | (a & 0x0FFF_FFFF),
        0xB00..=0xB0F => 0x9000_0000 | (a & 0x0FFF_FFFF),
        _ => a,
    };
    let class = match a >> 28 {
        0x8 => Class::Flash,
        0x5..=0x7 | 0x9 => Class::Ram,
        _ => Class::Volatile,
    };
    (a, class)
}

impl MemSource for Mem<'_> {
    fn place(&self, addr: u32) -> (u32, Class) {
        place(self.core, addr)
    }
    fn halt_byte(&mut self, addr: u32) -> Option<u8> {
        let block = addr & !63;
        let read = &mut self.read;
        let b = self.cache.entry(block).or_insert_with(|| read(block, 64).filter(|v| v.len() == 64));
        b.as_ref().map(|b| b[(addr - block) as usize])
    }
}

impl Replay {
    /// Rebuilds the registers for `steps`. `halt`: the registers at the halt when the flow
    /// reaches it (then also memory at the halt is used, through `read`).
    pub fn build(steps: &[Step], dis: &mut dyn Disasm, halt: Option<&HaltRegs>, read: &mut dyn FnMut(u32, usize) -> Option<Vec<u8>>, core: u8) -> Replay {
        Self::build_between(steps, dis, None, halt, read, core)
    }

    /// [`Replay::build`] with the registers before the first row as well (`entry`: the state the
    /// core was started from, when `steps` begin with the first instruction it ran). Values then
    /// also spread forwards from an exact start, so far fewer stay unknown.
    pub fn build_between(
        steps: &[Step],
        dis: &mut dyn Disasm,
        entry: Option<&HaltRegs>,
        halt: Option<&HaltRegs>,
        read: &mut dyn FnMut(u32, usize) -> Option<Vec<u8>>,
        core: u8,
    ) -> Replay {
        // Only an entry state whose PC is the first instruction belongs to these rows.
        let entry = entry.filter(|e| matches!(steps.first(), Some(Step::Insn { addr, .. }) if e.pc() == Some(*addr)));
        let mut solver = Solver::default();
        let mut rows = Vec::new();
        let mut after = Vec::new();
        let mut acc_of = Vec::new();
        let mut unmodelled: HashMap<String, u32> = HashMap::new();
        let mut entries = 0;
        let (initial, last_epoch, cur) = {
            let mut b = Builder::new(&mut solver);
            b.isp = halt.and_then(|h| h.get(38)).or_else(|| entry.and_then(|h| h.get(38)));
            let initial = b.cur;
            let next_of = |i: usize| -> Option<u32> {
                match steps.get(i + 1) {
                    Some(Step::Insn { addr, .. }) => Some(*addr),
                    Some(Step::Gap { .. }) => None,
                    None => halt.and_then(HaltRegs::pc),
                }
            };
            for (i, s) in steps.iter().enumerate() {
                match *s {
                    Step::Gap { .. } => {
                        let a0 = b.s.acc.len();
                        b.row = rows.len() as u32;
                        b.gap();
                        rows.push(Row::Gap { step: i });
                        after.push(b.cur);
                        acc_of.push((a0, b.s.acc.len()));
                    }
                    Step::Insn { addr, .. } => {
                        let a0 = b.s.acc.len();
                        b.row = rows.len() as u32;
                        let Some(d) = dis.disasm(addr) else {
                            b.gap();
                            rows.push(Row::Gap { step: i });
                            after.push(b.cur);
                            acc_of.push((a0, b.s.acc.len()));
                            continue;
                        };
                        let fall = addr.wrapping_add(d.size as u32);
                        let next = next_of(i);
                        let cls = operand::parse(&d.ops).map(|o| model::classify(&d.mnemonic, &o)).unwrap_or(Next::Seq);
                        let expected = match (cls, next) {
                            (_, None) => true,
                            (Next::Seq, Some(n)) => n == fall,
                            (Next::Direct(t), Some(n)) => n == t,
                            (Next::Cond(t), Some(n)) => n == t || n == fall,
                            (Next::Indirect, Some(_)) => true,
                        };
                        let before = b.s.ops.len();
                        b.insn(addr, d.size, &d.mnemonic, &d.ops, if expected { next } else { None });
                        if is_unmodelled(&d.mnemonic) {
                            *unmodelled.entry(d.mnemonic.clone()).or_default() += 1;
                        }
                        let _ = before;
                        rows.push(Row::Insn { step: i, addr, d: d.clone() });
                        after.push(b.cur);
                        acc_of.push((a0, b.s.acc.len()));
                        if !expected {
                            let a1 = b.s.acc.len();
                            b.row = rows.len() as u32;
                            let resume = match cls {
                                Next::Seq => Some(fall),
                                Next::Direct(t) => Some(t),
                                _ => None,
                            };
                            b.entry(resume);
                            entries += 1;
                            rows.push(Row::Entry { step: i, resume, vector: next.unwrap_or(0) });
                            after.push(b.cur);
                            acc_of.push((a1, b.s.acc.len()));
                        }
                    }
                }
            }
            (initial, b.epoch, b.cur)
        };
        solver.last_epoch = last_epoch;
        solver.anchored = halt.is_some();
        let anchor_all = |solver: &mut Solver, at: &[Ver; NREG], h: &HaltRegs, how: How, row: u32| {
            let mut anchor = |r: usize, v: Option<u32>| {
                if let Some(v) = v {
                    solver.set(at[r], Val { v, mark: Mark::Exact, row, how });
                }
            };
            for r in 0..32 {
                anchor(r, h.get(r));
            }
            anchor(PSWC as usize, h.get(35).map(|v| v & !PSW_FLAGS));
            anchor(PSWF as usize, h.get(35).map(|v| v & PSW_FLAGS));
            anchor(PCXL as usize, h.get(34).map(|v| v & PCXI_LINK));
            anchor(PCXH as usize, h.get(34).map(|v| v & !PCXI_LINK));
            anchor(FCX as usize, h.get(33).map(|v| v & PCXI_LINK));
            // Only IE and CCPN: PIPN (pending priority) changes with the interrupts that come in.
            anchor(ICR as usize, h.get(37).map(|v| v & model::ICR_TRACKED));
        };
        if let Some(h) = halt {
            anchor_all(&mut solver, &cur, h, How::Halt, u32::MAX);
        }
        if let Some(e) = entry {
            anchor_all(&mut solver, &initial, e, How::Entry, u32::MAX);
        }
        let mut mem = Mem { core, read, cache: HashMap::new() };
        // Solve; where a value from memory contradicts a branch outcome (memory changed after the
        // load), reject that memory and solve again from the compiled relations.
        let compiled = solver.clone();
        solver.solve(&mut mem);
        for _ in 0..8 {
            let bad = solver.contradictions();
            if bad.is_empty() {
                break;
            }
            let (mut blocked, mut poisoned) = (solver.blocked.clone(), solver.poisoned.clone());
            for (v, addr) in bad {
                match addr {
                    Some(a) => blocked.extend(a..a + 4),
                    None => {
                        poisoned.insert(v);
                    }
                }
            }
            solver = compiled.clone();
            solver.blocked = blocked;
            solver.poisoned = poisoned;
            solver.solve(&mut mem);
        }
        let mut unmodelled: Vec<(String, u32)> = unmodelled.into_iter().collect();
        unmodelled.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let stats = Stats { rows: rows.len(), entries, unmodelled, conflicts: solver.conflicts, anchored: halt.is_some() };
        Replay { rows, solver, initial, after, acc_of, stats, halt_pc: halt.and_then(HaltRegs::pc) }
    }

    fn state(&self, row: Option<usize>) -> &[Ver; NREG] {
        match row {
            None => &self.initial,
            Some(r) => &self.after[r.min(self.after.len().saturating_sub(1))],
        }
    }

    fn one(&self, name: &'static str, v: Ver) -> RegOut {
        match self.solver.vals[v as usize] {
            Some(x) => RegOut { name, value: x.v, known: u32::MAX, mark: Some(x.mark), row: Some(x.row), how: Some(x.how) },
            None => RegOut { name, value: 0, known: 0, mark: None, row: None, how: None },
        }
    }

    fn joined(&self, name: &'static str, lo: Ver, hi: Ver, hi_mask: u32) -> RegOut {
        let (a, b) = (self.solver.vals[lo as usize], self.solver.vals[hi as usize]);
        let known = a.map_or(0, |_| !hi_mask) | b.map_or(0, |_| hi_mask);
        let value = a.map_or(0, |x| x.v & !hi_mask) | b.map_or(0, |x| x.v & hi_mask);
        let mark = match (a, b) {
            (Some(x), Some(y)) => Some(x.mark.min(y.mark)),
            (Some(x), None) | (None, Some(x)) => Some(x.mark),
            _ => None,
        };
        let src = a.or(b);
        RegOut { name, value, known, mark, row: src.map(|x| x.row), how: src.map(|x| x.how) }
    }

    /// The visible registers after `row` (`None`: before the first row).
    pub fn regs(&self, row: Option<usize>) -> Vec<RegOut> {
        let s = self.state(row);
        let mut out: Vec<RegOut> = (0..32).map(|r| self.one(VISIBLE[r], s[r])).collect();
        out.push(self.joined("psw", s[PSWC as usize], s[PSWF as usize], PSW_FLAGS));
        out.push(self.joined("pcxi", s[PCXL as usize], s[PCXH as usize], !PCXI_LINK));
        out.push(self.one("fcx", s[FCX as usize]));
        let mut icr = self.one("icr", s[ICR as usize]);
        if icr.known != 0 {
            icr.known = model::ICR_TRACKED;
            icr.value &= model::ICR_TRACKED;
        }
        out.push(icr);
        out
    }

    /// The visible registers `row` wrote.
    pub fn written(&self, row: usize) -> Vec<RegOut> {
        let before = self.state(row.checked_sub(1));
        let after = self.state(Some(row));
        let changed = |r: u8| before[r as usize] != after[r as usize];
        let all = self.regs(Some(row));
        let mut out = Vec::new();
        for (i, r) in all.into_iter().enumerate() {
            let c = match i {
                0..=31 => changed(i as u8),
                32 => changed(PSWC) || changed(PSWF),
                33 => changed(PCXL) || changed(PCXH),
                34 => changed(FCX),
                _ => changed(ICR),
            };
            if c {
                out.push(r);
            }
        }
        out
    }

    /// The memory accesses of `row`.
    pub fn accesses(&self, row: usize) -> Vec<AccessOut> {
        let Some(&(a, b)) = self.acc_of.get(row) else { return Vec::new() };
        (a..b)
            .map(|i| {
                let x = &self.solver.acc[i];
                let addr = self.solver.ea.get(i).copied().flatten().or_else(|| self.solver.get(x.addr).map(|(v, _)| v.wrapping_add(x.disp as u32)));
                let value = if x.words.is_empty() { None } else { x.words.iter().map(|&w| self.solver.vals[w as usize].map(|v| v.v)).collect::<Option<Vec<u32>>>() };
                let value = match (value, x.sub) {
                    (Some(v), Some((size, _))) => Some(vec![v[0] & (u32::MAX >> (32 - 8 * size as u32))]),
                    (v, _) => v,
                };
                AccessOut { store: x.store, addr, len: x.len, value, context: x.ctx }
            })
            .collect()
    }

    /// Share of known D and A registers after `row`.
    pub fn known_share(&self, row: Option<usize>) -> f64 {
        let s = self.state(row);
        let n = (0..32).filter(|&r| self.solver.vals[s[r] as usize].is_some()).count();
        n as f64 / 32.0
    }

    /// Where a value came from, in words; `num(row)` names a row as the UI numbers it.
    pub fn explain(&self, r: &RegOut, num: &dyn Fn(u32) -> String) -> String {
        if r.known == 0 {
            return "Unknown: overwritten with no way back, loaded from an unknown address, or before a gap in the trace.".into();
        }
        let insn = |row: u32| match self.rows.get(row as usize) {
            Some(Row::Insn { d, .. }) => format!("{} `{} {}`", num(row), d.mnemonic, d.ops),
            Some(Row::Entry { .. }) => format!("{} (interrupt entry)", num(row)),
            _ => num(row),
        };
        let mut s = match (r.how, r.row) {
            (Some(How::Halt), _) => "The value at the halt.".to_string(),
            (Some(How::Entry), _) => "The value when the core was started (checkpoint).".to_string(),
            (Some(How::Forward), Some(row)) => format!("Computed by {} from known inputs.", insn(row)),
            (Some(How::Inverse), Some(row)) => format!("Undone through {} from the value after it.", insn(row)),
            (Some(How::Fact), Some(row)) => format!("From where the trace went at {}.", insn(row)),
            (Some(How::Mem(from, addr)), Some(row)) => {
                let src = match from {
                    MemFrom::Halt => "memory at the halt (another bus master could have changed it since)".to_string(),
                    MemFrom::Flash => "program flash (constant)".to_string(),
                    MemFrom::Context => "a context save area, still intact at the halt".to_string(),
                    MemFrom::Store(j) => format!("the store at {}", insn(j)),
                    MemFrom::Load(j) => format!("the load at {}", insn(j)),
                };
                format!("Through memory at 0x{addr:08X} ({}): {src}.", insn(row))
            }
            _ => String::new(),
        };
        if r.known != u32::MAX {
            s.push_str(" The status flags are not known.");
        } else if r.mark == Some(Mark::Mem) && !matches!(r.how, Some(How::Mem(MemFrom::Halt, _))) {
            s.push_str(" Depends on memory at the halt.");
        }
        s
    }
}

/// Mnemonics the model handles only by making their results unknown.
fn is_unmodelled(m: &str) -> bool {
    const KNOWN: &[&str] = &[
        "mov", "mov.u", "movh", "movh.a", "mov.a", "mov.aa", "mov.d", "lea", "addih", "addih.a", "add", "addi", "add.a", "addx", "sub", "sub.a", "subx",
        "rsub", "addsc.a", "mul", "mul.u", "madd", "msub", "madd.u", "div", "div.u", "abs", "clz", "popcnt.w", "and", "or", "xor", "andn", "orn", "nand",
        "nor", "xnor", "not", "sh", "sha", "extr", "extr.u", "insert", "dextr", "imask", "eq", "ne", "lt", "ge", "min", "max", "lt.u", "ge.u", "min.u",
        "max.u", "eq.a", "ne.a", "lt.a", "ge.a", "eqz.a", "nez.a", "sel", "seln", "cmov", "cmovn", "ld.b", "ld.bu", "ld.h", "ld.hu", "ld.w", "ld.a", "ld.d",
        "ld.da", "st.b", "st.h", "st.w", "st.a", "st.d", "st.da", "swap.w", "ldmst", "cmpswap.w", "swapmsk.w", "st.q", "call", "calla", "calli", "fcall",
        "fcalla", "fcalli", "fret", "ret", "rfe", "svlcx", "bisr", "rslcx", "ldlcx", "lducx", "stlcx", "stucx", "mfcr", "mtcr", "disable", "enable",
        "restore", "nop", "isync", "dsync", "debug", "wait", "j", "ja", "loopu", "jl", "jla", "ji", "jli", "jeq", "jne", "jeq.a", "jne.a", "jz", "jnz",
        "jz.a", "jnz.a", "jge", "jge.u", "jlt", "jlt.u", "jgez", "jgtz", "jlez", "jltz", "jz.t", "jnz.t", "jned", "jnei", "loop", "syscall", "trapv",
        "trapsv",
    ];
    !KNOWN.contains(&m)
}

#[cfg(test)]
mod tests;
