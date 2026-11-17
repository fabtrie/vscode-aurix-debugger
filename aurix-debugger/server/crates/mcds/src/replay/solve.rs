//! The constraint solver of the register replay.
//!
//! Every register write in the trace makes a new *version*; an instruction becomes relations
//! between versions ([`Op`]) and memory accesses ([`Access`]). Values come from the halt state,
//! constants and memory, and spread forwards (an operation with known inputs) and backwards (an
//! operation that can be undone, a branch outcome, a store whose memory is known later) until
//! nothing changes. Memory is followed per byte: the accesses to a byte, in program order, split
//! into *segments* at each store; all loads of a segment see the bytes its store wrote, and the
//! last segment holds what memory holds at the halt.

use std::collections::HashMap;

pub type Ver = u32;

/// How sure a value is. `Mem`: taken from memory as it is at the halt (another bus master could
/// have changed it in between) or derived from such a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mark {
    Mem = 1,
    Exact = 2,
}

/// Where memory bytes came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemFrom {
    /// Memory at the halt.
    Halt,
    /// Program flash (from the ELF or the device): constant.
    Flash,
    /// A context save area at the halt (only the core itself writes it).
    Context,
    /// Written by the store at this row.
    Store(u32),
    /// Seen by the load at this row.
    Load(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    /// The register's value at the halt.
    Halt,
    /// Computed by the instruction from known inputs.
    Forward,
    /// Undone through the instruction from the value after it.
    Inverse,
    /// From the outcome of a branch or jump (`jz` taken: the register was 0).
    Fact,
    /// From memory at this address.
    Mem(MemFrom, u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Val {
    pub v: u32,
    pub mark: Mark,
    /// The row that determined it (`u32::MAX`: the halt).
    pub row: u32,
    pub how: How,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arg {
    V(Ver),
    K(u32),
}

/// Operations of [`Op::Set`]. Inputs are `args` in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fun {
    Copy,
    Add,
    Sub,
    Mul,
    /// High word of the signed / unsigned 64-bit product.
    MulHiS,
    MulHiU,
    /// `a + b * c` (32 bit).
    Madd,
    /// `a - b * c`.
    Msub,
    /// 64-bit `{hi,lo} + b * c`: args `[lo, hi, b, c]`; `hi` selects the result word.
    Madd64 { signed: bool, hi: bool },
    And,
    Or,
    Xor,
    AndN,
    OrN,
    Nand,
    Nor,
    Xnor,
    Not,
    Neg,
    /// Shift by a signed 6-bit count (positive: left); `arith`: arithmetic right shift.
    Sh { arith: bool },
    Eq,
    Ne,
    Lt,
    LtU,
    Ge,
    GeU,
    Min,
    MinU,
    Max,
    MaxU,
    /// `a + (b << s)`.
    AddSc(u8),
    /// `[a, pos, width]`.
    Extr { signed: bool },
    /// `[a, b, pos, width]`: `a` with `width` bits at `pos` from `b`.
    Insert,
    /// `[hi, lo, pos]`: `({hi,lo} << pos) >> 32`.
    Dextr,
    /// `[c, a, b]`: `c != 0 ? a : b`.
    Sel,
    /// `[c, a, b]`: `c == 0 ? a : b`.
    SelN,
    Abs,
    Clz,
    Popcnt,
    DivQ { signed: bool },
    DivR { signed: bool },
    /// `(a & !m) | (b & m)`: joins two parts of a register (PSW, PCXI).
    Join(u32),
    /// `a & m`.
    Part(u32),
    /// PSW after CALL: call depth counter incremented, CDE set (None on overflow: a trap).
    CallPsw,
    /// PCXI's upper part after a context save: PCPN, PIE from ICR, UL.
    PcxiHi { ul: bool },
    /// ICR after RFE: `[icr, pcxi_hi]` (CCPN and IE from PCXI).
    IcrRfe,
    /// ICR with IE set or cleared.
    IcrIe(bool),
    /// ICR after BISR: IE set, CCPN = n.
    IcrBisr(u8),
    /// ICR after RESTORE: `[icr, d]` (IE = d[0]).
    IcrRestore,
    /// The context save area of a link word.
    CsaAddr,
    /// A10 at an interrupt entry: `[psw_c, a10, isp]` (ISP when PSW.IS was 0).
    IspSel,
    /// `(a >> s) & 1`.
    Bit(u8),
}

fn sext(v: u32, bits: u32) -> u32 {
    if bits >= 32 {
        v
    } else {
        (((v << (32 - bits)) as i32) >> (32 - bits)) as u32
    }
}

pub const PSW_FLAGS: u32 = 0xFC00_0000;
pub const PCXI_LINK: u32 = 0x000F_FFFF;

/// PSW.CDC increment of CALL (`None`: overflow, a trap instead of the call).
pub fn cdc_increment(psw: u32) -> Option<u32> {
    if psw & 0x80 == 0 {
        return Some(psw);
    }
    let cdc = psw & 0x7F;
    if cdc == 0x7F {
        return Some(psw);
    }
    let mut ones = 0;
    while ones < 7 && (cdc >> (6 - ones)) & 1 == 1 {
        ones += 1;
    }
    let width = 6 - ones;
    let mask = (1u32 << width) - 1;
    let n = cdc & mask;
    if n == mask {
        return None;
    }
    Some((psw & !mask) | (n + 1))
}

pub fn csa_addr(link: u32) -> u32 {
    ((link & 0xF_0000) << 12) | ((link & 0xFFFF) << 6)
}

impl Fun {
    pub fn eval(self, a: &[u32]) -> Option<u32> {
        let x = |i: usize| a[i];
        let s = |i: usize| a[i] as i32;
        Some(match self {
            Fun::Copy => x(0),
            Fun::Add => x(0).wrapping_add(x(1)),
            Fun::Sub => x(0).wrapping_sub(x(1)),
            Fun::Mul => x(0).wrapping_mul(x(1)),
            Fun::MulHiS => ((s(0) as i64 * s(1) as i64) >> 32) as u32,
            Fun::MulHiU => ((x(0) as u64 * x(1) as u64) >> 32) as u32,
            Fun::Madd => x(0).wrapping_add(x(1).wrapping_mul(x(2))),
            Fun::Msub => x(0).wrapping_sub(x(1).wrapping_mul(x(2))),
            Fun::Madd64 { signed, hi } => {
                let acc = ((x(1) as u64) << 32) | x(0) as u64;
                let p = if signed { (s(2) as i64).wrapping_mul(s(3) as i64) as u64 } else { (x(2) as u64).wrapping_mul(x(3) as u64) };
                let r = acc.wrapping_add(p);
                if hi {
                    (r >> 32) as u32
                } else {
                    r as u32
                }
            }
            Fun::And => x(0) & x(1),
            Fun::Or => x(0) | x(1),
            Fun::Xor => x(0) ^ x(1),
            Fun::AndN => x(0) & !x(1),
            Fun::OrN => x(0) | !x(1),
            Fun::Nand => !(x(0) & x(1)),
            Fun::Nor => !(x(0) | x(1)),
            Fun::Xnor => !(x(0) ^ x(1)),
            Fun::Not => !x(0),
            Fun::Neg => x(0).wrapping_neg(),
            Fun::Sh { arith } => {
                let c = sext(x(1) & 0x3F, 6) as i32;
                if c >= 0 {
                    if c >= 32 {
                        0
                    } else {
                        x(0) << c
                    }
                } else if arith {
                    ((s(0) as i64) >> (-c).min(63)) as u32
                } else if -c >= 32 {
                    0
                } else {
                    x(0) >> -c
                }
            }
            Fun::Eq => (x(0) == x(1)) as u32,
            Fun::Ne => (x(0) != x(1)) as u32,
            Fun::Lt => (s(0) < s(1)) as u32,
            Fun::LtU => (x(0) < x(1)) as u32,
            Fun::Ge => (s(0) >= s(1)) as u32,
            Fun::GeU => (x(0) >= x(1)) as u32,
            Fun::Min => s(0).min(s(1)) as u32,
            Fun::MinU => x(0).min(x(1)),
            Fun::Max => s(0).max(s(1)) as u32,
            Fun::MaxU => x(0).max(x(1)),
            Fun::AddSc(sh) => x(0).wrapping_add(x(1) << sh),
            Fun::Extr { signed } => {
                let (pos, w) = (x(1) & 31, x(2) & 31);
                if pos + w > 32 {
                    return None;
                }
                let v = if w == 0 { 0 } else { (x(0) >> pos) & (u32::MAX >> (32 - w)) };
                if signed && w > 0 {
                    sext(v, w)
                } else {
                    v
                }
            }
            Fun::Insert => {
                let (pos, w) = (x(2) & 31, x(3) & 31);
                if pos + w > 32 {
                    return None;
                }
                let m = if w == 0 { 0 } else { (u32::MAX >> (32 - w)) << pos };
                (x(0) & !m) | ((x(1) << pos) & m)
            }
            Fun::Dextr => {
                let p = x(2) & 31;
                if p == 0 {
                    x(0)
                } else {
                    (x(0) << p) | (x(1) >> (32 - p))
                }
            }
            Fun::Sel => {
                if x(0) != 0 {
                    x(1)
                } else {
                    x(2)
                }
            }
            Fun::SelN => {
                if x(0) == 0 {
                    x(1)
                } else {
                    x(2)
                }
            }
            Fun::Abs => s(0).wrapping_abs() as u32,
            Fun::Clz => x(0).leading_zeros(),
            Fun::Popcnt => x(0).count_ones(),
            Fun::DivQ { signed } => {
                if x(1) == 0 {
                    return None;
                }
                if signed {
                    s(0).checked_div(s(1))? as u32
                } else {
                    x(0) / x(1)
                }
            }
            Fun::DivR { signed } => {
                if x(1) == 0 {
                    return None;
                }
                if signed {
                    s(0).checked_rem(s(1))? as u32
                } else {
                    x(0) % x(1)
                }
            }
            Fun::Join(m) => (x(0) & !m) | (x(1) & m),
            Fun::Part(m) => x(0) & m,
            Fun::CallPsw => cdc_increment(x(0))? | 0x80,
            Fun::PcxiHi { ul } => ((x(0) & 0xFF) << 22) | (((x(0) >> 15) & 1) << 21) | ((ul as u32) << 20),
            Fun::IcrRfe => (x(0) & !(0xFF | 1 << 15)) | ((x(1) >> 22) & 0xFF) | (((x(1) >> 21) & 1) << 15),
            Fun::IcrIe(on) => {
                if on {
                    x(0) | 1 << 15
                } else {
                    x(0) & !(1 << 15)
                }
            }
            Fun::IcrBisr(n) => (x(0) & !0xFF) | n as u32 | 1 << 15,
            Fun::IcrRestore => (x(0) & !(1 << 15)) | ((x(1) & 1) << 15),
            Fun::CsaAddr => csa_addr(x(0)),
            Fun::IspSel => {
                if (x(0) >> 9) & 1 == 0 {
                    x(2)
                } else {
                    x(1)
                }
            }
            Fun::Bit(sh) => (x(0) >> sh) & 1,
        })
    }

    /// Input `i` from the result and the other inputs, where the operation can be undone.
    pub fn invert(self, dst: u32, a: &[Option<u32>], i: usize) -> Option<u32> {
        let o = |j: usize| a[j];
        match (self, i) {
            (Fun::Copy, 0) => Some(dst),
            (Fun::Add, 0) => Some(dst.wrapping_sub(o(1)?)),
            (Fun::Add, 1) => Some(dst.wrapping_sub(o(0)?)),
            (Fun::Sub, 0) => Some(dst.wrapping_add(o(1)?)),
            (Fun::Sub, 1) => Some(o(0)?.wrapping_sub(dst)),
            (Fun::Xor, 0) => Some(dst ^ o(1)?),
            (Fun::Xor, 1) => Some(dst ^ o(0)?),
            (Fun::Xnor, 0) => Some(!dst ^ o(1)?),
            (Fun::Xnor, 1) => Some(!dst ^ o(0)?),
            (Fun::Not, 0) => Some(!dst),
            (Fun::Neg, 0) => Some(dst.wrapping_neg()),
            (Fun::AddSc(sh), 0) => Some(dst.wrapping_sub(o(1)? << sh)),
            (Fun::AddSc(sh), 1) => {
                let d = dst.wrapping_sub(o(0)?);
                // Only when the index is the whole difference (no bits lost by the shift).
                (d & ((1u32 << sh) - 1) == 0 && sh == 0).then_some(d >> sh)
            }
            // The parts of a joined register are kept masked.
            (Fun::Join(m), 0) => Some(dst & !m),
            (Fun::Join(m), 1) => Some(dst & m),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Op {
    /// `dst = fun(args)`. The same version can be the target of several (a constraint).
    Set { dst: Ver, fun: Fun, args: Vec<Arg>, row: u32 },
    /// `a == b`, from a branch or jump outcome.
    Eq { a: Ver, b: Arg, row: u32 },
    /// `a != b`, from a branch outcome: not used to derive values, but to reject a value from
    /// memory that contradicts it ([`Solver::contradictions`]).
    Ne { a: Ver, b: Arg, row: u32 },
}

/// A memory access. The address is `addr + disp`; data is one version per 4-byte word, or one
/// version for a sub-word access (`sub`: size and sign extension of a load).
#[derive(Debug, Clone)]
pub struct Access {
    pub row: u32,
    pub store: bool,
    pub addr: Arg,
    pub disp: i32,
    pub len: u8,
    /// Data versions; empty: data not known (read-modify-write, an unmodelled store).
    pub words: Vec<Ver>,
    pub sub: Option<(u8, bool)>,
    /// A context save area access (CALL/RET/SVLCX/...).
    pub ctx: bool,
    pub epoch: u32,
}

/// Memory classes for the replay: volatile memory (peripherals) is never read and never linked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    Volatile,
    Ram,
    Flash,
}

pub trait MemSource {
    /// The address memory accesses are linked by (aliases folded together), and its class.
    fn place(&self, addr: u32) -> (u32, Class);
    /// One byte of memory at the halt, or `None` if not readable.
    fn halt_byte(&mut self, addr: u32) -> Option<u8>;
}

#[derive(Debug, Default, Clone)]
pub struct Solver {
    pub vals: Vec<Option<Val>>,
    pub ops: Vec<Op>,
    pub acc: Vec<Access>,
    /// The epoch of the halt (memory at the halt holds only for the last epoch).
    pub last_epoch: u32,
    /// Whether memory at the halt may be used (the flow reaches the halt).
    pub anchored: bool,
    /// Two derivations disagreed (a model or memory inconsistency); the first one stays.
    pub conflicts: u32,
    /// Per access: the address once known (after aliasing), for the effects column.
    pub ea: Vec<Option<u32>>,
    /// Memory at the halt not to use at these byte addresses (a branch outcome contradicted it).
    pub blocked: std::collections::HashSet<u32>,
    /// Versions that may not take a value from memory (it contradicted a branch outcome).
    pub poisoned: std::collections::HashSet<Ver>,
}

/// A byte of a memory segment.
#[derive(Debug, Clone, Copy)]
struct Byte {
    v: u8,
    mark: Mark,
    from: MemFrom,
}

impl Solver {
    pub fn new_ver(&mut self) -> Ver {
        self.vals.push(None);
        (self.vals.len() - 1) as Ver
    }

    pub fn get(&self, a: Arg) -> Option<(u32, Mark)> {
        match a {
            Arg::K(k) => Some((k, Mark::Exact)),
            Arg::V(v) => self.vals[v as usize].map(|x| (x.v, x.mark)),
        }
    }

    /// Sets a version; a value already known stays unless the new one is surer.
    pub fn set(&mut self, v: Ver, val: Val) -> bool {
        if val.mark == Mark::Mem && self.poisoned.contains(&v) {
            return false;
        }
        match self.vals[v as usize] {
            None => {
                self.vals[v as usize] = Some(val);
                true
            }
            Some(old) if old.v == val.v => {
                if val.mark > old.mark {
                    self.vals[v as usize] = Some(val);
                    true
                } else {
                    false
                }
            }
            Some(old) => {
                self.conflicts += 1;
                if val.mark > old.mark {
                    self.vals[v as usize] = Some(val);
                    true
                } else {
                    false
                }
            }
        }
    }

    fn apply(&mut self, i: usize) -> bool {
        let mut changed = false;
        match self.ops[i].clone() {
            Op::Set { dst, fun, args, row } => {
                let known: Vec<Option<(u32, Mark)>> = args.iter().map(|&a| self.get(a)).collect();
                if known.iter().all(Option::is_some) {
                    let vs: Vec<u32> = known.iter().map(|k| k.unwrap().0).collect();
                    let mark = known.iter().map(|k| k.unwrap().1).min().unwrap_or(Mark::Exact);
                    if let Some(v) = fun.eval(&vs) {
                        let better = self.vals[dst as usize].is_none_or(|o| o.mark < mark);
                        if better {
                            changed |= self.set(dst, Val { v, mark, row, how: How::Forward });
                        }
                    }
                }
                if let Some(d) = self.vals[dst as usize] {
                    let vals: Vec<Option<u32>> = known.iter().map(|k| k.map(|k| k.0)).collect();
                    for (j, a) in args.iter().enumerate() {
                        let Arg::V(av) = *a else { continue };
                        let others = known.iter().enumerate().filter(|(k, _)| *k != j).map(|(_, k)| k.map(|k| k.1)).collect::<Vec<_>>();
                        if self.vals[av as usize].is_some_and(|x| x.mark >= d.mark) {
                            continue;
                        }
                        if let Some(v) = fun.invert(d.v, &vals, j) {
                            let mark = others.iter().flatten().copied().chain([d.mark]).min().unwrap_or(d.mark);
                            if self.vals[av as usize].is_none_or(|o| o.mark < mark) {
                                changed |= self.set(av, Val { v, mark, row, how: How::Inverse });
                            }
                        }
                    }
                }
            }
            Op::Ne { .. } => {}
            Op::Eq { a, b, row } => {
                let (ka, kb) = (self.get(Arg::V(a)), self.get(b));
                match (ka, kb) {
                    (None, Some((v, mark))) => changed |= self.set(a, Val { v, mark, row, how: How::Fact }),
                    (Some((v, mark)), None) => {
                        if let Arg::V(bv) = b {
                            changed |= self.set(bv, Val { v, mark, row, how: How::Fact });
                        }
                    }
                    (Some((_, ma)), Some((v, mb))) if mb > ma => changed |= self.set(a, Val { v, mark: mb, row, how: How::Fact }),
                    (Some((v, ma)), Some((_, mb))) if ma > mb => {
                        if let Arg::V(bv) = b {
                            changed |= self.set(bv, Val { v, mark: ma, row, how: How::Fact });
                        }
                    }
                    _ => {}
                }
            }
        }
        changed
    }

    /// Register propagation until stable (backward, then forward, alternating).
    fn sweep(&mut self) -> bool {
        let mut any = false;
        for _ in 0..16 {
            let mut changed = false;
            for i in (0..self.ops.len()).rev() {
                changed |= self.apply(i);
            }
            for i in 0..self.ops.len() {
                changed |= self.apply(i);
            }
            any |= changed;
            if !changed {
                break;
            }
        }
        any
    }

    fn word_bytes(&self, a: &Access) -> Vec<Option<(u8, Mark, u32)>> {
        // Per byte: the value, its mark and the row of the data's version (for stores).
        let mut out = vec![None; a.len as usize];
        match a.sub {
            Some((size, _)) => {
                if let Some(&w) = a.words.first() {
                    if let Some(x) = self.vals[w as usize] {
                        for (k, o) in out.iter_mut().enumerate().take(size as usize) {
                            *o = Some(((x.v >> (8 * k)) as u8, x.mark, x.row));
                        }
                    }
                }
            }
            None => {
                for (wi, &w) in a.words.iter().enumerate() {
                    if let Some(x) = self.vals[w as usize] {
                        for k in 0..4 {
                            if let Some(o) = out.get_mut(wi * 4 + k) {
                                *o = Some(((x.v >> (8 * k)) as u8, x.mark, x.row));
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// Links the accesses whose address is known through memory; returns whether a register
    /// value changed.
    fn memory(&mut self, mem: &mut dyn MemSource) -> bool {
        // Addresses.
        let mut ea = vec![None; self.acc.len()];
        let mut unknown_stores = Vec::new();
        for (i, a) in self.acc.iter().enumerate() {
            match self.get(a.addr) {
                Some((base, _)) => {
                    let (p, class) = mem.place(base.wrapping_add(a.disp as u32));
                    if class != Class::Volatile {
                        ea[i] = Some((p, class));
                    }
                }
                None if a.store => unknown_stores.push(i),
                None => {}
            }
        }
        self.ea = ea.iter().map(|e| e.map(|e| e.0)).collect();
        // Per byte address: the accesses in program order.
        let mut by: HashMap<u32, Vec<(usize, u8)>> = HashMap::new();
        for (i, e) in ea.iter().enumerate() {
            if let Some((addr, _)) = e {
                for k in 0..self.acc[i].len {
                    by.entry(addr.wrapping_add(k as u32)).or_default().push((i, k));
                }
            }
        }
        // Segments: (value, per byte) and the access bytes in them. Weak links cross an
        // unknown-address store (values pass with Mark::Mem).
        let mut segs: Vec<Option<Byte>> = Vec::new();
        let mut members: Vec<(usize, u8, usize)> = Vec::new(); // (access, byte, segment)
        let mut weak: Vec<(usize, usize)> = Vec::new();
        let mut finals: Vec<(u32, usize, bool, Class)> = Vec::new(); // (addr, seg, ctx, class)
        let mut addrs: Vec<u32> = by.keys().copied().collect();
        addrs.sort_unstable();
        for addr in addrs {
            let list = &by[&addr];
            let class = ea[list[0].0].map(|e| e.1).unwrap_or(Class::Ram);
            let mut seg = segs.len();
            segs.push(None);
            let mut prev: Option<usize> = None;
            let mut seg_ctx = false;
            for &(i, k) in list {
                let a = &self.acc[i];
                if let Some(p) = prev {
                    if self.acc[p].epoch != a.epoch {
                        seg = segs.len();
                        segs.push(None);
                        seg_ctx = false;
                    } else if unknown_stores.iter().any(|&u| u > p && u < i) {
                        let s2 = segs.len();
                        segs.push(None);
                        weak.push((seg, s2));
                        seg = s2;
                        seg_ctx = false;
                    }
                }
                if a.store {
                    seg = segs.len();
                    segs.push(None);
                    seg_ctx = false;
                }
                seg_ctx |= a.ctx;
                members.push((i, k, seg));
                prev = Some(i);
            }
            let last = prev.unwrap();
            if self.anchored && self.acc[last].epoch == self.last_epoch {
                if unknown_stores.iter().any(|&u| u > last) {
                    let s2 = segs.len();
                    segs.push(None);
                    weak.push((seg, s2));
                    finals.push((addr, s2, false, class));
                } else {
                    finals.push((addr, seg, seg_ctx, class));
                }
            }
        }
        // Memory at the halt.
        for &(addr, s, ctx, class) in &finals {
            if self.blocked.contains(&addr) {
                continue;
            }
            if let Some(v) = mem.halt_byte(addr) {
                let (mark, from) = match class {
                    Class::Flash => (Mark::Exact, MemFrom::Flash),
                    _ if ctx => (Mark::Exact, MemFrom::Context),
                    _ => (Mark::Mem, MemFrom::Halt),
                };
                segs[s] = Some(Byte { v, mark, from });
            }
        }
        // Spread within segments.
        let bytes: Vec<Vec<Option<(u8, Mark, u32)>>> = self.acc.iter().map(|a| self.word_bytes(a)).collect();
        for _ in 0..8 {
            let mut changed = false;
            for &(i, k, s) in &members {
                if let Some((v, mark, _)) = bytes[i][k as usize] {
                    let from = if self.acc[i].store { MemFrom::Store(self.acc[i].row) } else { MemFrom::Load(self.acc[i].row) };
                    match segs[s] {
                        None => {
                            segs[s] = Some(Byte { v, mark, from });
                            changed = true;
                        }
                        Some(b) if b.mark < mark => {
                            if b.v != v {
                                self.conflicts += 1;
                            }
                            segs[s] = Some(Byte { v, mark, from });
                            changed = true;
                        }
                        _ => {}
                    }
                }
            }
            for &(a, b) in &weak {
                match (segs[a], segs[b]) {
                    (Some(x), None) => {
                        segs[b] = Some(Byte { mark: Mark::Mem, ..x });
                        changed = true;
                    }
                    (None, Some(x)) => {
                        segs[a] = Some(Byte { mark: Mark::Mem, ..x });
                        changed = true;
                    }
                    _ => {}
                }
            }
            if !changed {
                break;
            }
        }
        // Back into registers: loads get their data, stores their source register.
        let mut seg_of: HashMap<(usize, u8), usize> = HashMap::new();
        for &(i, k, s) in &members {
            seg_of.insert((i, k), s);
        }
        let mut changed = false;
        for i in 0..self.acc.len() {
            let Some((addr, _)) = ea[i] else { continue };
            let a = self.acc[i].clone();
            let byte = |k: u8| seg_of.get(&(i, k)).and_then(|&s| segs[s]);
            let word = |k0: u8, n: u8| -> Option<(u32, Mark, MemFrom)> {
                let mut v = 0u32;
                let mut mark = Mark::Exact;
                let mut from = None;
                for k in 0..n {
                    let b = byte(k0 + k)?;
                    v |= (b.v as u32) << (8 * k);
                    mark = mark.min(b.mark);
                    from.get_or_insert(b.from);
                }
                Some((v, mark, from?))
            };
            match a.sub {
                Some((size, signed)) => {
                    // A store of a part of a register says nothing about the rest of it.
                    if a.store {
                        continue;
                    }
                    if let (Some(&w), Some((v, mark, from))) = (a.words.first(), word(0, size)) {
                        let v = if signed { sext(v, 8 * size as u32) } else { v };
                        changed |= self.set(w, Val { v, mark, row: a.row, how: How::Mem(from, addr) });
                    }
                }
                None => {
                    for (wi, &w) in a.words.iter().enumerate() {
                        if let Some((v, mark, from)) = word(4 * wi as u8, 4) {
                            if a.store && matches!(from, MemFrom::Store(r) if r == a.row) {
                                continue;
                            }
                            if self.vals[w as usize].is_none_or(|o| o.mark < mark) {
                                changed |= self.set(w, Val { v, mark, row: a.row, how: How::Mem(from, addr + 4 * wi as u32) });
                            }
                        }
                    }
                }
            }
        }
        changed
    }

    /// Branch outcomes the values contradict: for each, the side that came from memory (to be
    /// rejected) and the address it was loaded from at the halt, if it came straight from there.
    /// Contradictions between exact values only count as conflicts.
    pub fn contradictions(&mut self) -> Vec<(Ver, Option<u32>)> {
        let mut out = Vec::new();
        for op in &self.ops {
            let Op::Ne { a, b, .. } = *op else { continue };
            let (Some(x), Some((bv, bm))) = (self.vals[a as usize], self.get(b)) else { continue };
            if x.v != bv {
                continue;
            }
            let side = if x.mark == Mark::Mem {
                Some((a, x.how))
            } else if bm == Mark::Mem {
                match b {
                    Arg::V(v) => self.vals[v as usize].map(|y| (v, y.how)),
                    Arg::K(_) => None,
                }
            } else {
                None
            };
            match side {
                Some((v, how)) => out.push((v, match how {
                    How::Mem(MemFrom::Halt, addr) => Some(addr),
                    _ => None,
                })),
                None => self.conflicts += 1,
            }
        }
        out
    }

    pub fn solve(&mut self, mem: &mut dyn MemSource) {
        for _ in 0..12 {
            let a = self.sweep();
            let b = self.memory(mem);
            if !a && !b {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn functions() {
        assert_eq!(Fun::Sh { arith: false }.eval(&[0x10, (-2i32) as u32]), Some(4));
        assert_eq!(Fun::Sh { arith: true }.eval(&[0x8000_0000, (-4i32) as u32]), Some(0xF800_0000));
        assert_eq!(Fun::Sh { arith: false }.eval(&[1, 2]), Some(4));
        assert_eq!(Fun::Extr { signed: false }.eval(&[0xABCD, 4, 8]), Some(0xBC));
        assert_eq!(Fun::Extr { signed: true }.eval(&[0x0F80, 4, 8]), Some(0xFFFF_FFF8));
        assert_eq!(Fun::Insert.eval(&[0xFFFF_FFFF, 0, 8, 4]), Some(0xFFFF_F0FF));
        assert_eq!(Fun::Dextr.eval(&[0x1234_5678, 0x9ABC_DEF0, 8]), Some(0x3456_789A));
        assert_eq!(Fun::Add.invert(10, &[None, Some(3)], 0), Some(7));
        assert_eq!(Fun::Sub.invert(10, &[Some(15), None], 1), Some(5));
        assert_eq!(cdc_increment(0x80), Some(0x81));
        assert_eq!(cdc_increment(0x7F | 0x80), Some(0xFF));
        assert_eq!(cdc_increment(0x80 | 0x3F), None);
        assert_eq!(csa_addr(0x0007_0A12), 0x7002_8480);
        assert_eq!(Fun::PcxiHi { ul: true }.eval(&[0x0000_8005]), Some(5 << 22 | 1 << 21 | 1 << 20));
    }
}
