//! TriCore instructions as relations for the solver ([`super::solve`]).
//!
//! An instruction the model does not know makes its destination registers unknown (and a store
//! of it makes memory unknown where it cannot be placed), so it can only lose information,
//! never invent it. The PSW status flags and PCXI's link are separate model registers so that the
//! many instructions changing the flags do not lose the rest of the PSW.

use super::operand::{self, Mode, Opnd, Reg, A10, A11};
use super::solve::{Access, Arg, Fun, Op, Solver, Ver, PCXI_LINK, PSW_FLAGS};

/// Model registers beyond D0-D15, A0-A15.
pub const PSWC: Reg = 32;
pub const PSWF: Reg = 33;
pub const PCXL: Reg = 34;
pub const PCXH: Reg = 35;
pub const FCX: Reg = 36;
pub const ICR: Reg = 37;
pub const NREG: usize = 38;
/// The ICR bits the model follows (IE, CCPN); PIPN is set by the interrupt system at any time.
pub const ICR_TRACKED: u32 = 0x0000_80FF;

pub const CSFR_PCXI: u32 = 0xFE00;
pub const CSFR_PSW: u32 = 0xFE04;
pub const CSFR_ICR: u32 = 0xFE2C;
pub const CSFR_FCX: u32 = 0xFE38;

/// Where execution may go after an instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// The next instruction in memory.
    Seq,
    /// Always the target.
    Direct(u32),
    /// The target or the next instruction.
    Cond(u32),
    /// Anywhere (a register or a return).
    Indirect,
}

const UPPER: [Reg; 14] = [A10, A11, 8, 9, 10, 11, 28, 29, 30, 31, 12, 13, 14, 15];
const LOWER: [Reg; 14] = [A11, 18, 19, 0, 1, 2, 3, 20, 21, 22, 23, 4, 5, 6];
// The lower context also holds D7 as its last word.
const LOWER_LAST: Reg = 7;

fn lower_words() -> Vec<Reg> {
    let mut v = LOWER.to_vec();
    v.push(LOWER_LAST);
    v
}

/// Instructions that change the PSW status flags (C, V, SV, AV, SAV and the FPU flags).
fn sets_flags(m: &str) -> bool {
    let base = m.split('.').next().unwrap_or(m);
    matches!(
        base,
        "add" | "addc" | "addi" | "addih" | "addx" | "adds" | "sub" | "subc" | "subx" | "subs" | "rsub" | "rsubs" | "mul" | "muls" | "madd" | "madds" | "maddm"
            | "maddms" | "maddr" | "maddrs" | "maddsu" | "maddsus" | "msub" | "msubs" | "msubm" | "msubms" | "msubr" | "msubrs" | "msubad" | "abs" | "abss"
            | "absdif" | "absdifs" | "sha" | "shas" | "div" | "dvstep" | "dvadj" | "dvinit" | "cmp" | "ftoi" | "ftoiz" | "ftou" | "ftouz" | "ftoq31" | "ftoq31z"
            | "itof" | "utof" | "q31tof" | "qseed" | "updfl" | "mulr" | "mulm" | "sat"
    ) && !m.ends_with(".a")
        || m.ends_with(".f")
}

pub fn classify(m: &str, ops: &[Opnd]) -> Next {
    let target = ops.iter().rev().find_map(Opnd::imm).map(|t| t as u32);
    match m {
        "j" | "ja" | "jl" | "jla" | "call" | "calla" | "fcall" | "fcalla" | "loopu" => target.map(Next::Direct).unwrap_or(Next::Indirect),
        "ji" | "jli" | "calli" | "fcalli" | "ret" | "rfe" | "rfm" | "fret" => Next::Indirect,
        "jeq" | "jne" | "jge" | "jge.u" | "jlt" | "jlt.u" | "jz" | "jnz" | "jgez" | "jgtz" | "jlez" | "jltz" | "jz.a" | "jnz.a" | "jeq.a" | "jne.a"
        | "jz.t" | "jnz.t" | "jned" | "jnei" | "loop" => target.map(Next::Cond).unwrap_or(Next::Indirect),
        _ => Next::Seq,
    }
}

/// The builder of one trace: current versions per model register.
pub struct Builder<'a> {
    pub s: &'a mut Solver,
    pub cur: [Ver; NREG],
    pub row: u32,
    pub epoch: u32,
    /// ISP at the halt (for interrupt entries).
    pub isp: Option<u32>,
}

impl<'a> Builder<'a> {
    pub fn new(s: &'a mut Solver) -> Self {
        let mut cur = [0; NREG];
        for c in cur.iter_mut() {
            *c = s.new_ver();
        }
        Builder { s, cur, row: 0, epoch: 0, isp: None }
    }

    fn rd(&self, r: Reg) -> Arg {
        Arg::V(self.cur[r as usize])
    }
    fn wr(&mut self, r: Reg) -> Ver {
        let v = self.s.new_ver();
        self.cur[r as usize] = v;
        v
    }
    fn tmp(&mut self, fun: Fun, args: Vec<Arg>) -> Arg {
        let v = self.s.new_ver();
        self.s.ops.push(Op::Set { dst: v, fun, args, row: self.row });
        Arg::V(v)
    }
    fn set(&mut self, r: Reg, fun: Fun, args: Vec<Arg>) {
        let d = self.wr(r);
        self.s.ops.push(Op::Set { dst: d, fun, args, row: self.row });
    }
    fn constrain(&mut self, dst: Ver, fun: Fun, args: Vec<Arg>) {
        self.s.ops.push(Op::Set { dst, fun, args, row: self.row });
    }
    fn eq(&mut self, a: Arg, b: Arg) {
        match (a, b) {
            (Arg::V(a), b) | (b, Arg::V(a)) => self.s.ops.push(Op::Eq { a, b, row: self.row }),
            _ => {}
        }
    }
    fn ne(&mut self, a: Arg, b: Arg) {
        match (a, b) {
            (Arg::V(a), b) | (b, Arg::V(a)) => self.s.ops.push(Op::Ne { a, b, row: self.row }),
            _ => {}
        }
    }
    fn unknown(&mut self, r: Reg) {
        self.wr(r);
    }
    fn access(&mut self, store: bool, addr: Arg, disp: i32, len: u8, words: Vec<Ver>, sub: Option<(u8, bool)>, ctx: bool) {
        self.s.acc.push(Access { row: self.row, store, addr, disp, len, words, sub, ctx, epoch: self.epoch });
    }
    /// A store the model cannot place: memory written somewhere unknown.
    fn taint(&mut self) {
        let v = Arg::V(self.s.new_ver());
        self.access(true, v, 0, 4, Vec::new(), None, false);
    }

    /// Everything unknown: a gap in the trace (code ran that was not recorded).
    pub fn gap(&mut self) {
        for r in 0..NREG as Reg {
            self.unknown(r);
        }
        self.epoch += 1;
    }

    fn psw_word(&mut self) -> Arg {
        self.tmp(Fun::Join(PSW_FLAGS), vec![self.rd(PSWC), self.rd(PSWF)])
    }
    fn pcxi_word(&mut self) -> Arg {
        self.tmp(Fun::Join(!PCXI_LINK), vec![self.rd(PCXL), self.rd(PCXH)])
    }
    /// Splits a loaded PSW word into the model's parts (and back).
    fn psw_from(&mut self, w: Ver, keep_rm: bool) {
        let old = self.rd(PSWC);
        if keep_rm {
            // RET keeps PSW[25:24].
            let lo = self.tmp(Fun::Part(!PSW_FLAGS & !0x0300_0000), vec![Arg::V(w)]);
            let keep = self.tmp(Fun::Part(0x0300_0000), vec![old]);
            self.set(PSWC, Fun::Or, vec![lo, keep]);
        } else {
            self.set(PSWC, Fun::Part(!PSW_FLAGS), vec![Arg::V(w)]);
            let c = self.cur[PSWC as usize];
            self.set(PSWF, Fun::Part(PSW_FLAGS), vec![Arg::V(w)]);
            let f = self.cur[PSWF as usize];
            self.constrain(w, Fun::Join(PSW_FLAGS), vec![Arg::V(c), Arg::V(f)]);
            return;
        }
        self.set(PSWF, Fun::Part(PSW_FLAGS), vec![Arg::V(w)]);
    }
    fn pcxi_from(&mut self, w: Ver) {
        self.set(PCXL, Fun::Part(PCXI_LINK), vec![Arg::V(w)]);
        let l = self.cur[PCXL as usize];
        self.set(PCXH, Fun::Part(!PCXI_LINK), vec![Arg::V(w)]);
        let h = self.cur[PCXH as usize];
        self.constrain(w, Fun::Join(!PCXI_LINK), vec![Arg::V(l), Arg::V(h)]);
    }

    /// Saves a context (upper with PSW, or lower with A11) to the free list's head; PCXI and FCX
    /// follow as CALL, SVLCX or an interrupt entry do.
    fn save_context(&mut self, upper: bool) {
        let ea = self.tmp(Fun::CsaAddr, vec![self.rd(FCX)]);
        let link = self.s.new_ver();
        self.access(false, ea, 0, 4, vec![link], None, true);
        let mut words = vec![self.pcxi_word()];
        if upper {
            words.push(self.psw_word());
            words.extend(UPPER.iter().map(|&r| self.rd(r)));
        } else {
            words.extend(lower_words().iter().map(|&r| self.rd(r)));
        }
        let vs: Vec<Ver> = words.iter().map(|a| self.as_ver(*a)).collect();
        self.access(true, ea, 0, 64, vs, None, true);
        let icr = self.rd(ICR);
        self.set(PCXH, Fun::PcxiHi { ul: upper }, vec![icr]);
        let fcx = self.rd(FCX);
        self.set(PCXL, Fun::Copy, vec![fcx]);
        self.set(FCX, Fun::Part(PCXI_LINK), vec![Arg::V(link)]);
    }

    /// Restores a context from PCXI's CSA and returns the CSA to the free list (RET, RFE,
    /// RSLCX).
    fn restore_context(&mut self, upper: bool, rfe: bool) {
        let ea = self.tmp(Fun::CsaAddr, vec![self.rd(PCXL)]);
        let n = 16;
        let vs: Vec<Ver> = (0..n).map(|_| self.s.new_ver()).collect();
        self.access(false, ea, 0, 64, vs.clone(), None, true);
        let fcx = self.cur[FCX as usize];
        self.access(true, ea, 0, 4, vec![fcx], None, true);
        let old_pcxl = self.rd(PCXL);
        let old_pcxh = self.rd(PCXH);
        self.set(FCX, Fun::Copy, vec![old_pcxl]);
        if rfe {
            let icr = self.rd(ICR);
            self.set(ICR, Fun::IcrRfe, vec![icr, old_pcxh]);
        }
        self.pcxi_from(vs[0]);
        let regs: Vec<Reg> = if upper { UPPER.to_vec() } else { lower_words() };
        if upper {
            self.psw_from(vs[1], !rfe);
        }
        let first = if upper { 2 } else { 1 };
        for (k, &r) in regs.iter().enumerate() {
            self.cur[r as usize] = vs[first + k];
        }
    }

    fn as_ver(&mut self, a: Arg) -> Ver {
        match a {
            Arg::V(v) => v,
            Arg::K(k) => {
                let v = self.s.new_ver();
                self.s.ops.push(Op::Set { dst: v, fun: Fun::Copy, args: vec![Arg::K(k)], row: self.row });
                v
            }
        }
    }

    /// An interrupt or trap was taken before the next instruction; `resume`: the address it
    /// returns to, if known.
    pub fn entry(&mut self, resume: Option<u32>) {
        let pswc = self.rd(PSWC);
        let a10 = self.rd(A10);
        self.save_context(true);
        match self.isp {
            Some(isp) => self.set(A10, Fun::IspSel, vec![pswc, a10, Arg::K(isp)]),
            None => self.unknown(A10),
        }
        match resume {
            Some(r) => self.set(A11, Fun::Copy, vec![Arg::K(r)]),
            None => self.unknown(A11),
        }
        self.unknown(PSWC);
        self.unknown(PSWF);
        self.unknown(ICR);
    }

    fn val(&mut self, o: &Opnd) -> Option<Arg> {
        match *o {
            Opnd::R(r) => Some(self.rd(r)),
            Opnd::Imm(k) => Some(Arg::K(k as u32)),
            _ => None,
        }
    }

    /// Address of a memory operand (before any base update) and the base update, or None.
    fn ea(&mut self, o: &Opnd, len: u8) -> Option<(Arg, i32)> {
        match *o {
            Opnd::Mem { base, disp, mode } => match mode {
                Mode::Offset => Some((self.rd(base), disp.unwrap_or(0))),
                Mode::PostInc => {
                    let a = self.rd(base);
                    let d = disp.unwrap_or(len as i32);
                    self.set(base, Fun::Add, vec![a, Arg::K(d as u32)]);
                    Some((a, 0))
                }
                Mode::PreInc => {
                    let a = self.rd(base);
                    let d = disp.unwrap_or(len as i32);
                    self.set(base, Fun::Add, vec![a, Arg::K(d as u32)]);
                    Some((self.rd(base), 0))
                }
                Mode::Other => {
                    self.unknown(base);
                    None
                }
            },
            Opnd::Imm(k) => Some((Arg::K(k as u32), 0)),
            _ => None,
        }
    }

    fn load(&mut self, ops: &[Opnd], len: u8, sub: Option<(u8, bool)>) -> bool {
        let (Some(dst), Some(m)) = (ops.first(), ops.get(1)) else { return false };
        let regs: Vec<Reg> = match *dst {
            Opnd::R(r) => vec![r],
            Opnd::E(r) | Opnd::P(r) => vec![r, r + 1],
            _ => return false,
        };
        match self.ea(m, len) {
            Some((a, d)) => {
                let vs: Vec<Ver> = regs.iter().map(|&r| self.wr(r)).collect();
                self.access(false, a, d, len, vs, sub, false);
            }
            None => regs.iter().for_each(|&r| self.unknown(r)),
        }
        true
    }

    fn store(&mut self, ops: &[Opnd], len: u8, sub: Option<(u8, bool)>) -> bool {
        let (Some(m), Some(src)) = (ops.first(), ops.get(1)) else { return false };
        let regs: Vec<Reg> = match *src {
            Opnd::R(r) => vec![r],
            Opnd::E(r) | Opnd::P(r) => vec![r, r + 1],
            _ => return false,
        };
        let vs: Vec<Ver> = regs.iter().map(|&r| self.cur[r as usize]).collect();
        match self.ea(m, len) {
            Some((a, d)) => self.access(true, a, d, len, vs, sub, false),
            None => self.taint(),
        }
        true
    }

    /// Two- or three-operand form: `op d, x` is `d = d op x`.
    fn bin(&mut self, fun: Fun, ops: &[Opnd], zext: bool) -> bool {
        let fix = |o: &Opnd| match (*o, zext) {
            (Opnd::Imm(k), true) => Opnd::Imm(operand::zext9(k) as i64),
            _ => *o,
        };
        let (d, a, b) = match ops.len() {
            2 => (ops[0].reg(), Some(ops[0]), Some(fix(&ops[1]))),
            3 => (ops[0].reg(), Some(ops[1]), Some(fix(&ops[2]))),
            _ => return false,
        };
        let (Some(d), Some(a), Some(b)) = (d, a, b) else { return false };
        let (Some(a), Some(b)) = (self.val(&a), self.val(&b)) else { return false };
        self.set(d, fun, vec![a, b]);
        true
    }

    /// One executed instruction. `next`: the address executed after it, when known (for branch
    /// outcomes and indirect targets).
    pub fn insn(&mut self, addr: u32, size: u8, m: &str, ops_text: &str, next: Option<u32>) {
        let Some(ops) = operand::parse(ops_text) else {
            self.fallback(m, &[]);
            return;
        };
        let ok = self.model(addr, size, m, &ops, next);
        if !ok {
            self.fallback(m, &ops);
        } else if sets_flags(m) {
            self.unknown(PSWF);
        }
    }

    /// An instruction the model does not know: its destinations become unknown.
    fn fallback(&mut self, m: &str, ops: &[Opnd]) {
        let stores_first = m.starts_with("st") || matches!(m, "ldmst" | "swap.w" | "cmpswap.w" | "swapmsk.w" | "cachea.w" | "cachea.wi" | "cachei.w" | "cachei.wi");
        if let Some(first) = ops.first() {
            match *first {
                Opnd::R(r) if !stores_first => self.unknown(r),
                Opnd::E(r) | Opnd::P(r) if !stores_first => {
                    self.unknown(r);
                    self.unknown(r + 1);
                }
                _ => {}
            }
        }
        for o in ops {
            if let Opnd::Mem { base, mode: Mode::PostInc | Mode::PreInc | Mode::Other, .. } = *o {
                self.unknown(base);
            }
        }
        if stores_first || matches!(m, "swap.w" | "cmpswap.w" | "swapmsk.w") {
            self.taint();
            if let Some(Opnd::R(r) | Opnd::E(r)) = ops.get(1) {
                if m != "ldmst" {
                    self.unknown(*r);
                }
            }
        }
        if sets_flags(m) || ops.is_empty() && !matches!(m, "nop" | "isync" | "dsync" | "debug" | "wait") {
            self.unknown(PSWF);
        }
        if matches!(m, "mtcr") {
            for r in [PSWC, PSWF, PCXL, PCXH, FCX, ICR] {
                self.unknown(r);
            }
        }
    }

    fn model(&mut self, addr: u32, size: u8, m: &str, ops: &[Opnd], next: Option<u32>) -> bool {
        let fall = addr.wrapping_add(size as u32);
        let k = |o: Option<&Opnd>| o.and_then(Opnd::imm);
        let target = ops.iter().rev().find_map(Opnd::imm).map(|t| t as u32);
        let taken = next.zip(target).map(|(n, t)| n == t && t != fall);
        let not_taken = next.map(|n| n == fall && target != Some(fall));
        match m {
            // ---- moves and constants
            "mov" => match ops {
                [Opnd::R(d), Opnd::Imm(v)] => self.set(*d, Fun::Copy, vec![Arg::K(*v as u32)]),
                [Opnd::R(d), Opnd::R(s)] => self.set(*d, Fun::Copy, vec![self.rd(*s)]),
                [Opnd::E(d), Opnd::Imm(v)] => {
                    self.set(*d, Fun::Copy, vec![Arg::K(*v as u32)]);
                    self.set(*d + 1, Fun::Copy, vec![Arg::K(if *v < 0 { u32::MAX } else { 0 })]);
                }
                [Opnd::E(d), Opnd::R(a), Opnd::R(b)] => {
                    let (va, vb) = (self.rd(*a), self.rd(*b));
                    self.set(*d, Fun::Copy, vec![vb]);
                    self.set(*d + 1, Fun::Copy, vec![va]);
                }
                _ => return false,
            },
            "mov.u" => self.set(ops[0].reg().unwrap_or(0), Fun::Copy, vec![Arg::K((k(ops.get(1)).unwrap_or(0) as u32) & 0xFFFF)]),
            "movh" | "movh.a" => match ops {
                [Opnd::R(d), Opnd::Imm(v)] => self.set(*d, Fun::Copy, vec![Arg::K(((*v as u32) & 0xFFFF) << 16)]),
                _ => return false,
            },
            "mov.a" | "mov.aa" | "mov.d" => match ops {
                [Opnd::R(d), Opnd::Imm(v)] => self.set(*d, Fun::Copy, vec![Arg::K(*v as u32)]),
                [Opnd::R(d), Opnd::R(s)] => self.set(*d, Fun::Copy, vec![self.rd(*s)]),
                _ => return false,
            },
            "lea" => match ops {
                [Opnd::R(d), Opnd::Mem { base, disp, mode: Mode::Offset }] => {
                    let b = self.rd(*base);
                    self.set(*d, Fun::Add, vec![b, Arg::K(disp.unwrap_or(0) as u32)]);
                }
                [Opnd::R(d), Opnd::Imm(v)] => self.set(*d, Fun::Copy, vec![Arg::K(*v as u32)]),
                _ => return false,
            },
            "addih" | "addih.a" => match ops {
                [Opnd::R(d), Opnd::R(a), Opnd::Imm(v)] => {
                    let va = self.rd(*a);
                    self.set(*d, Fun::Add, vec![va, Arg::K(((*v as u32) & 0xFFFF) << 16)]);
                }
                _ => return false,
            },
            // ---- arithmetic
            "add" | "addi" | "add.a" | "addx" => return self.bin(Fun::Add, ops, false),
            "sub" | "sub.a" | "subx" => return self.bin(Fun::Sub, ops, false),
            "rsub" => match ops {
                [Opnd::R(d)] => self.set(*d, Fun::Neg, vec![self.rd(*d)]),
                [Opnd::R(d), Opnd::R(a), Opnd::Imm(v)] => {
                    let va = self.rd(*a);
                    self.set(*d, Fun::Sub, vec![Arg::K(*v as u32), va]);
                }
                _ => return false,
            },
            "addsc.a" => match ops {
                [Opnd::R(d), Opnd::R(a), Opnd::R(x), Opnd::Imm(n)] => {
                    let (va, vx) = (self.rd(*a), self.rd(*x));
                    self.set(*d, Fun::AddSc(*n as u8 & 3), vec![va, vx]);
                }
                _ => return false,
            },
            "mul" => match ops {
                [Opnd::E(d), Opnd::R(a), b] => {
                    let (Some(va), Some(vb)) = (Some(self.rd(*a)), self.val(b)) else { return false };
                    self.set(*d, Fun::Mul, vec![va, vb]);
                    self.set(*d + 1, Fun::MulHiS, vec![va, vb]);
                }
                _ => return self.bin(Fun::Mul, ops, false),
            },
            "mul.u" => match ops {
                [Opnd::E(d), Opnd::R(a), b] => {
                    let b = match *b {
                        Opnd::Imm(v) => Opnd::Imm(operand::zext9(v) as i64),
                        o => o,
                    };
                    let (va, Some(vb)) = (self.rd(*a), self.val(&b)) else { return false };
                    self.set(*d, Fun::Mul, vec![va, vb]);
                    self.set(*d + 1, Fun::MulHiU, vec![va, vb]);
                }
                _ => return false,
            },
            "madd" | "msub" | "madd.u" => match ops {
                [Opnd::R(d), Opnd::R(acc), Opnd::R(a), b] if m != "madd.u" => {
                    let (vacc, va, Some(vb)) = (self.rd(*acc), self.rd(*a), self.val(b)) else { return false };
                    self.set(*d, if m == "madd" { Fun::Madd } else { Fun::Msub }, vec![vacc, va, vb]);
                }
                [Opnd::E(d), Opnd::E(acc), Opnd::R(a), b] if m != "msub" => {
                    let signed = m == "madd";
                    let b = match (*b, signed) {
                        (Opnd::Imm(v), false) => Opnd::Imm(operand::zext9(v) as i64),
                        (o, _) => o,
                    };
                    let (lo, hi, va, Some(vb)) = (self.rd(*acc), self.rd(*acc + 1), self.rd(*a), self.val(&b)) else { return false };
                    self.set(*d, Fun::Madd64 { signed, hi: false }, vec![lo, hi, va, vb]);
                    self.set(*d + 1, Fun::Madd64 { signed, hi: true }, vec![lo, hi, va, vb]);
                }
                _ => return false,
            },
            "div" | "div.u" => match ops {
                [Opnd::E(d), Opnd::R(a), Opnd::R(b)] => {
                    let signed = m == "div";
                    let (va, vb) = (self.rd(*a), self.rd(*b));
                    self.set(*d, Fun::DivQ { signed }, vec![va, vb]);
                    self.set(*d + 1, Fun::DivR { signed }, vec![va, vb]);
                }
                _ => return false,
            },
            "abs" => match ops {
                [Opnd::R(d), Opnd::R(a)] => self.set(*d, Fun::Abs, vec![self.rd(*a)]),
                _ => return false,
            },
            "clz" | "popcnt.w" => match ops {
                [Opnd::R(d), Opnd::R(a)] => self.set(*d, if m == "clz" { Fun::Clz } else { Fun::Popcnt }, vec![self.rd(*a)]),
                _ => return false,
            },
            // ---- logic and bit fields (const9 zero-extended)
            "and" => return self.bin(Fun::And, ops, true),
            "or" => return self.bin(Fun::Or, ops, true),
            "xor" => return self.bin(Fun::Xor, ops, true),
            "andn" => return self.bin(Fun::AndN, ops, true),
            "orn" => return self.bin(Fun::OrN, ops, true),
            "nand" => return self.bin(Fun::Nand, ops, true),
            "nor" => return self.bin(Fun::Nor, ops, true),
            "xnor" => return self.bin(Fun::Xnor, ops, true),
            "not" => match ops {
                [Opnd::R(d)] => self.set(*d, Fun::Not, vec![self.rd(*d)]),
                _ => return false,
            },
            "sh" => return self.bin(Fun::Sh { arith: false }, ops, false),
            "sha" => return self.bin(Fun::Sh { arith: true }, ops, false),
            "extr" | "extr.u" => {
                let signed = m == "extr";
                match ops {
                    [Opnd::R(d), Opnd::R(a), Opnd::Imm(p), Opnd::Imm(w)] => {
                        let va = self.rd(*a);
                        self.set(*d, Fun::Extr { signed }, vec![va, Arg::K(*p as u32), Arg::K(*w as u32)]);
                    }
                    [Opnd::R(d), Opnd::R(a), Opnd::E(e)] => {
                        let (va, p, w) = (self.rd(*a), self.rd(*e), self.rd(*e + 1));
                        self.set(*d, Fun::Extr { signed }, vec![va, p, w]);
                    }
                    [Opnd::R(d), Opnd::R(a), Opnd::R(p), Opnd::Imm(w)] => {
                        let (va, vp) = (self.rd(*a), self.rd(*p));
                        self.set(*d, Fun::Extr { signed }, vec![va, vp, Arg::K(*w as u32)]);
                    }
                    _ => return false,
                }
            }
            "insert" => match ops {
                [Opnd::R(d), Opnd::R(a), b, Opnd::Imm(p), Opnd::Imm(w)] => {
                    let b = match *b {
                        Opnd::Imm(v) => Opnd::Imm((v as u32 & 0xF) as i64),
                        o => o,
                    };
                    let (va, Some(vb)) = (self.rd(*a), self.val(&b)) else { return false };
                    self.set(*d, Fun::Insert, vec![va, vb, Arg::K(*p as u32), Arg::K(*w as u32)]);
                }
                [Opnd::R(d), Opnd::R(a), b, Opnd::E(e)] => {
                    let b = match *b {
                        Opnd::Imm(v) => Opnd::Imm((v as u32 & 0xF) as i64),
                        o => o,
                    };
                    let (va, Some(vb)) = (self.rd(*a), self.val(&b)) else { return false };
                    let (p, w) = (self.rd(*e), self.rd(*e + 1));
                    self.set(*d, Fun::Insert, vec![va, vb, p, w]);
                }
                _ => return false,
            },
            "dextr" => match ops {
                [Opnd::R(d), Opnd::R(a), Opnd::R(b), p] => {
                    let (va, vb, Some(vp)) = (self.rd(*a), self.rd(*b), self.val(p)) else { return false };
                    self.set(*d, Fun::Dextr, vec![va, vb, vp]);
                }
                _ => return false,
            },
            "imask" => match ops {
                [Opnd::E(d), b, p, Opnd::Imm(w)] => {
                    let b = match *b {
                        Opnd::Imm(v) => Opnd::Imm((v as u32 & 0xF) as i64),
                        o => o,
                    };
                    let (Some(vb), Some(vp)) = (self.val(&b), self.val(p)) else { return false };
                    let mask = if *w <= 0 { 0 } else { u32::MAX >> (32 - (*w as u32).min(32)) };
                    self.set(*d, Fun::Sh { arith: false }, vec![vb, vp]);
                    self.set(*d + 1, Fun::Sh { arith: false }, vec![Arg::K(mask), vp]);
                }
                _ => return false,
            },
            // ---- comparisons and selects
            "eq" | "ne" | "lt" | "ge" | "min" | "max" => {
                let f = match m {
                    "eq" => Fun::Eq,
                    "ne" => Fun::Ne,
                    "lt" => Fun::Lt,
                    "ge" => Fun::Ge,
                    "min" => Fun::Min,
                    _ => Fun::Max,
                };
                return self.bin(f, ops, false);
            }
            "lt.u" | "ge.u" | "min.u" | "max.u" => {
                let f = match m {
                    "lt.u" => Fun::LtU,
                    "ge.u" => Fun::GeU,
                    "min.u" => Fun::MinU,
                    _ => Fun::MaxU,
                };
                return self.bin(f, ops, true);
            }
            "eq.a" | "ne.a" | "lt.a" | "ge.a" => {
                let f = match m {
                    "eq.a" => Fun::Eq,
                    "ne.a" => Fun::Ne,
                    "lt.a" => Fun::LtU,
                    _ => Fun::GeU,
                };
                return self.bin(f, ops, false);
            }
            "eqz.a" | "nez.a" => match ops {
                [Opnd::R(d), Opnd::R(a)] => {
                    let va = self.rd(*a);
                    self.set(*d, if m == "eqz.a" { Fun::Eq } else { Fun::Ne }, vec![va, Arg::K(0)]);
                }
                _ => return false,
            },
            "sel" | "seln" => match ops {
                [Opnd::R(d), Opnd::R(c), Opnd::R(a), b] => {
                    let (vc, va, Some(vb)) = (self.rd(*c), self.rd(*a), self.val(b)) else { return false };
                    self.set(*d, if m == "sel" { Fun::Sel } else { Fun::SelN }, vec![vc, va, vb]);
                }
                _ => return false,
            },
            "cmov" | "cmovn" => match ops {
                [Opnd::R(d), Opnd::R(c), b] => {
                    let (vd, vc, Some(vb)) = (self.rd(*d), self.rd(*c), self.val(b)) else { return false };
                    self.set(*d, if m == "cmov" { Fun::Sel } else { Fun::SelN }, vec![vc, vb, vd]);
                }
                _ => return false,
            },
            // ---- loads and stores
            "ld.b" => return self.load(ops, 1, Some((1, true))),
            "ld.bu" => return self.load(ops, 1, Some((1, false))),
            "ld.h" => return self.load(ops, 2, Some((2, true))),
            "ld.hu" => return self.load(ops, 2, Some((2, false))),
            "ld.w" | "ld.a" => return self.load(ops, 4, None),
            "ld.d" | "ld.da" => return self.load(ops, 8, None),
            "st.b" => return self.store(ops, 1, Some((1, false))),
            "st.h" => return self.store(ops, 2, Some((2, false))),
            "st.w" | "st.a" => return self.store(ops, 4, None),
            "st.d" | "st.da" => return self.store(ops, 8, None),
            "swap.w" => match ops {
                [mo, Opnd::R(d)] => {
                    let old = self.cur[*d as usize];
                    match self.ea(mo, 4) {
                        Some((a, disp)) => {
                            let nv = self.wr(*d);
                            self.access(false, a, disp, 4, vec![nv], None, false);
                            self.access(true, a, disp, 4, vec![old], None, false);
                        }
                        None => {
                            self.unknown(*d);
                            self.taint();
                        }
                    }
                }
                _ => return false,
            },
            "ldmst" | "cmpswap.w" | "swapmsk.w" | "st.q" => match ops.first() {
                Some(mo) => {
                    let len = if m == "st.q" { 2 } else { 4 };
                    match self.ea(mo, len) {
                        Some((a, disp)) => {
                            if m != "ldmst" && m != "st.q" {
                                if let Some(Opnd::E(e)) = ops.get(1) {
                                    let nv = self.wr(*e);
                                    self.access(false, a, disp, 4, vec![nv], None, false);
                                }
                            }
                            self.access(true, a, disp, len, Vec::new(), None, false);
                        }
                        None => self.taint(),
                    }
                }
                None => return false,
            },
            // ---- context
            "call" | "calla" | "calli" => {
                if m == "calli" {
                    if let (Some(Opnd::R(a)), Some(n)) = (ops.first(), next) {
                        let va = self.rd(*a);
                        self.eq(va, Arg::K(n));
                    }
                }
                // The CSA keeps the caller's PSW; the callee runs one call deeper (PSW.CDC).
                let pswc = self.rd(PSWC);
                self.save_context(true);
                self.set(PSWC, Fun::CallPsw, vec![pswc]);
                self.set(A11, Fun::Copy, vec![Arg::K(fall)]);
            }
            "fcall" | "fcalla" | "fcalli" => {
                if m == "fcalli" {
                    if let (Some(Opnd::R(a)), Some(n)) = (ops.first(), next) {
                        let va = self.rd(*a);
                        self.eq(va, Arg::K(n));
                    }
                }
                let sp = self.rd(A10);
                self.set(A10, Fun::Sub, vec![sp, Arg::K(4)]);
                let ra = self.cur[A11 as usize];
                let nsp = self.rd(A10);
                self.access(true, nsp, 0, 4, vec![ra], None, false);
                self.set(A11, Fun::Copy, vec![Arg::K(fall)]);
            }
            "fret" => {
                if let Some(n) = next {
                    let ra = self.rd(A11);
                    self.eq(ra, Arg::K(n));
                }
                let sp = self.rd(A10);
                let ra = self.wr(A11);
                self.access(false, sp, 0, 4, vec![ra], None, false);
                self.set(A10, Fun::Add, vec![sp, Arg::K(4)]);
            }
            "ret" | "rfe" => {
                if let Some(n) = next {
                    let ra = self.rd(A11);
                    self.eq(ra, Arg::K(n));
                }
                self.restore_context(true, m == "rfe");
            }
            "svlcx" => self.save_context(false),
            "bisr" => {
                let n = k(ops.first()).unwrap_or(0) as u8;
                self.save_context(false);
                let icr = self.rd(ICR);
                self.set(ICR, Fun::IcrBisr(n), vec![icr]);
            }
            "rslcx" => self.restore_context(false, false),
            "ldlcx" | "lducx" | "stlcx" | "stucx" => {
                let Some(mo) = ops.first() else { return false };
                let Some((a, disp)) = self.ea(mo, 64) else {
                    if m.starts_with("st") {
                        self.taint();
                    } else {
                        let regs: Vec<Reg> = if m == "ldlcx" { LOWER[1..].iter().copied().chain([LOWER_LAST]).collect() } else { UPPER.to_vec() };
                        regs.iter().for_each(|&r| self.unknown(r));
                    }
                    return true;
                };
                match m {
                    "ldlcx" | "lducx" => {
                        // Words 2..15: A2..D7 (lower) or A10..D15 (upper).
                        let regs: Vec<Reg> = if m == "ldlcx" { LOWER[1..].iter().copied().chain([LOWER_LAST]).collect() } else { UPPER.to_vec() };
                        let vs: Vec<Ver> = regs.iter().map(|&r| self.wr(r)).collect();
                        self.access(false, a, disp.wrapping_add(8), 56, vs, None, false);
                    }
                    _ => {
                        let mut words = vec![self.pcxi_word()];
                        if m == "stucx" {
                            words.push(self.psw_word());
                            words.extend(UPPER.iter().map(|&r| self.rd(r)));
                        } else {
                            words.extend(lower_words().iter().map(|&r| self.rd(r)));
                        }
                        let vs: Vec<Ver> = words.iter().map(|a| self.as_ver(*a)).collect();
                        self.access(true, a, disp, 64, vs, None, false);
                    }
                }
            }
            "mfcr" => match ops {
                [Opnd::R(d), Opnd::Imm(c)] => match (*c as u32) & 0xFFFF {
                    CSFR_PSW => {
                        let w = self.psw_word();
                        self.set(*d, Fun::Copy, vec![w]);
                    }
                    CSFR_PCXI => {
                        let w = self.pcxi_word();
                        self.set(*d, Fun::Copy, vec![w]);
                    }
                    CSFR_FCX => self.set(*d, Fun::Copy, vec![self.rd(FCX)]),
                    // PIPN is not followed: the value read is not known.
                    CSFR_ICR => self.unknown(*d),
                    _ => self.unknown(*d),
                },
                _ => return false,
            },
            "mtcr" => match ops {
                [Opnd::Imm(c), Opnd::R(s)] => {
                    let v = self.rd(*s);
                    match (*c as u32) & 0xFFFF {
                        CSFR_PSW => {
                            self.set(PSWC, Fun::Part(!PSW_FLAGS), vec![v]);
                            self.set(PSWF, Fun::Part(PSW_FLAGS), vec![v]);
                        }
                        CSFR_PCXI => {
                            self.set(PCXL, Fun::Part(PCXI_LINK), vec![v]);
                            self.set(PCXH, Fun::Part(!PCXI_LINK), vec![v]);
                        }
                        CSFR_FCX => self.set(FCX, Fun::Part(PCXI_LINK), vec![v]),
                        CSFR_ICR => self.set(ICR, Fun::Part(ICR_TRACKED), vec![v]),
                        _ => {}
                    }
                }
                _ => return false,
            },
            "disable" => {
                if let [Opnd::R(d)] = ops {
                    let icr = self.rd(ICR);
                    self.set(*d, Fun::Bit(15), vec![icr]);
                }
                let icr = self.rd(ICR);
                self.set(ICR, Fun::IcrIe(false), vec![icr]);
            }
            "enable" => {
                let icr = self.rd(ICR);
                self.set(ICR, Fun::IcrIe(true), vec![icr]);
            }
            "restore" => match ops {
                [Opnd::R(d)] => {
                    let (icr, vd) = (self.rd(ICR), self.rd(*d));
                    self.set(ICR, Fun::IcrRestore, vec![icr, vd]);
                }
                _ => return false,
            },
            "nop" | "isync" | "dsync" | "debug" | "wait" => {}
            // ---- jumps and branches
            "j" | "ja" | "loopu" => {}
            "jl" | "jla" => self.set(A11, Fun::Copy, vec![Arg::K(fall)]),
            "ji" | "jli" => {
                if let (Some(Opnd::R(a)), Some(n)) = (ops.first(), next) {
                    let va = self.rd(*a);
                    self.eq(va, Arg::K(n));
                }
                if m == "jli" {
                    self.set(A11, Fun::Copy, vec![Arg::K(fall)]);
                }
            }
            "jeq" | "jne" | "jeq.a" | "jne.a" => {
                let (Some(a), Some(b)) = (ops.first(), ops.get(1)) else { return false };
                let (Some(va), Some(vb)) = (self.val(a), self.val(b)) else { return false };
                let equal = if m.starts_with("jeq") { taken } else { not_taken };
                let differ = if m.starts_with("jeq") { not_taken } else { taken };
                if equal == Some(true) {
                    self.eq(va, vb);
                } else if differ == Some(true) {
                    self.ne(va, vb);
                }
            }
            "jz" | "jnz" | "jz.a" | "jnz.a" => {
                let Some(Opnd::R(a)) = ops.first() else { return false };
                let zero = if m.starts_with("jz") { taken } else { not_taken };
                let nonzero = if m.starts_with("jz") { not_taken } else { taken };
                let va = self.rd(*a);
                if zero == Some(true) {
                    self.eq(va, Arg::K(0));
                } else if nonzero == Some(true) {
                    self.ne(va, Arg::K(0));
                }
            }
            "jge" | "jge.u" | "jlt" | "jlt.u" | "jgez" | "jgtz" | "jlez" | "jltz" | "jz.t" | "jnz.t" => {}
            "jned" | "jnei" => {
                let (Some(Opnd::R(a)), Some(b)) = (ops.first(), ops.get(1)) else { return false };
                let (va, Some(vb)) = (self.rd(*a), self.val(b)) else { return false };
                if not_taken == Some(true) {
                    self.eq(va, vb);
                } else if taken == Some(true) {
                    self.ne(va, vb);
                }
                self.set(*a, Fun::Add, vec![va, Arg::K(if m == "jned" { u32::MAX } else { 1 })]);
            }
            "loop" => {
                let Some(Opnd::R(a)) = ops.first() else { return false };
                let va = self.rd(*a);
                if not_taken == Some(true) {
                    self.eq(va, Arg::K(0));
                }
                self.set(*a, Fun::Sub, vec![va, Arg::K(1)]);
            }
            "syscall" | "trapv" | "trapsv" => {}
            _ => return false,
        }
        true
    }
}
