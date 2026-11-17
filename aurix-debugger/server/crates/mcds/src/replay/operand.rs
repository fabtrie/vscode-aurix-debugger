//! Operands of a TriCore instruction as Capstone prints them (`d15, [a4+]#8`, `e4, [sp]#0x10`,
//! `#0x80012a40`), and the immediates corrected to what the ISA uses.

/// Register numbers of the model: D0-D15 = 0..16, A0-A15 = 16..32 (A10 = SP).
pub type Reg = u8;

pub const A10: Reg = 26;
pub const A11: Reg = 27;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// `[a]#d`: base + displacement.
    Offset,
    /// `[a+]#d`: base, then base += d.
    PostInc,
    /// `[+a]#d`: base += d, then base.
    PreInc,
    /// Circular or bit-reverse addressing (`[p+c]`, `[p+r]`): not modelled.
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opnd {
    /// A D or A register.
    R(Reg),
    /// A data register pair `eN` (DN low, DN+1 high).
    E(Reg),
    /// An address register pair `pN`.
    P(Reg),
    /// An immediate as printed (`#n`, also a bare number).
    Imm(i64),
    /// A memory operand. `disp` of `[a+]` without a displacement is `None` (16-bit forms: the
    /// access size).
    Mem { base: Reg, disp: Option<i32>, mode: Mode },
}

impl Opnd {
    pub fn reg(&self) -> Option<Reg> {
        match *self {
            Opnd::R(r) => Some(r),
            _ => None,
        }
    }
    pub fn imm(&self) -> Option<i64> {
        match *self {
            Opnd::Imm(v) => Some(v),
            _ => None,
        }
    }
}

fn reg_name(s: &str) -> Option<Opnd> {
    let s = s.trim();
    if s == "sp" {
        return Some(Opnd::R(A10));
    }
    if s == "ra" {
        return Some(Opnd::R(A11));
    }
    let (k, n) = s.split_at(1);
    let n: u8 = n.parse().ok()?;
    if n > 15 {
        return None;
    }
    match k {
        "d" => Some(Opnd::R(n)),
        "a" => Some(Opnd::R(16 + n)),
        "e" => Some(Opnd::E(n)),
        "p" => Some(Opnd::P(16 + n)),
        _ => None,
    }
}

fn number(s: &str) -> Option<i64> {
    let s = s.trim().trim_start_matches('#');
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let v = match s.strip_prefix("0x") {
        Some(h) => i64::from_str_radix(h, 16).ok()?,
        None => s.parse::<i64>().ok()?,
    };
    Some(if neg { -v } else { v })
}

/// One operand.
pub fn parse_one(s: &str) -> Option<Opnd> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('[') {
        let close = rest.find(']')?;
        let inner = &rest[..close];
        let after = rest[close + 1..].trim();
        let disp = if after.is_empty() { None } else { Some(number(after)? as i32) };
        let (base, mode) = if let Some(b) = inner.strip_prefix('+') {
            (b, Mode::PreInc)
        } else if let Some(b) = inner.strip_suffix('+') {
            (b, Mode::PostInc)
        } else if inner.contains('+') {
            (inner.split('+').next()?, Mode::Other)
        } else {
            (inner, Mode::Offset)
        };
        let base = match reg_name(base)? {
            Opnd::R(r) | Opnd::P(r) => r,
            _ => return None,
        };
        return Some(Opnd::Mem { base, disp, mode });
    }
    if let Some(r) = reg_name(s) {
        return Some(r);
    }
    number(s).map(Opnd::Imm)
}

/// All operands of an instruction; `None` if one is not understood.
pub fn parse(ops: &str) -> Option<Vec<Opnd>> {
    if ops.trim().is_empty() {
        return Some(Vec::new());
    }
    ops.split(',').map(parse_one).collect()
}

/// The 9-bit constant of an RC-format instruction that the ISA zero-extends, which Capstone
/// prints sign-extended (`and d15, d15, #-0x81` is `and` with 0x17F).
pub fn zext9(v: i64) -> u32 {
    if v < 0 && v >= -256 {
        (v as u32) & 0x1FF
    } else {
        v as u32
    }
}

/// The 4-bit constant of `jge.u`/`jlt.u` (zero-extended by the ISA).
pub fn zext4(v: i64) -> u32 {
    if v < 0 && v >= -8 {
        (v as u32) & 0xF
    } else {
        v as u32
    }
}

pub fn reg_label(r: Reg) -> String {
    match r {
        0..=15 => format!("d{r}"),
        16..=31 => format!("a{}", r - 16),
        _ => format!("r{r}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capstone_forms() {
        assert_eq!(parse("d15, [a4]#8"), Some(vec![Opnd::R(15), Opnd::Mem { base: 20, disp: Some(8), mode: Mode::Offset }]));
        assert_eq!(parse("e8, [a12+]#8"), Some(vec![Opnd::E(8), Opnd::Mem { base: 28, disp: Some(8), mode: Mode::PostInc }]));
        assert_eq!(parse("d2, [sp]#0xc"), Some(vec![Opnd::R(2), Opnd::Mem { base: A10, disp: Some(12), mode: Mode::Offset }]));
        assert_eq!(parse("d4, [a4]"), Some(vec![Opnd::R(4), Opnd::Mem { base: 20, disp: None, mode: Mode::Offset }]));
        assert_eq!(parse("d8, [a12+]"), Some(vec![Opnd::R(8), Opnd::Mem { base: 28, disp: None, mode: Mode::PostInc }]));
        assert_eq!(parse("d5, [+a3]#-4"), Some(vec![Opnd::R(5), Opnd::Mem { base: 19, disp: Some(-4), mode: Mode::PreInc }]));
        assert_eq!(parse("a14, [a14]#-0x5eac").unwrap()[1], Opnd::Mem { base: 30, disp: Some(-0x5eac), mode: Mode::Offset });
        assert_eq!(parse("d15, #0xf0001800"), Some(vec![Opnd::R(15), Opnd::Imm(0xf000_1800)]));
        assert_eq!(parse("a7, 0x80127894"), Some(vec![Opnd::R(23), Opnd::Imm(0x8012_7894)]));
        assert_eq!(parse("#-0x200, d2"), Some(vec![Opnd::Imm(-0x200), Opnd::R(2)]));
        assert_eq!(parse(""), Some(vec![]));
        assert_eq!(zext9(-0x81), 0x17F);
        assert_eq!(zext9(5), 5);
        assert_eq!(zext4(-1), 0xF);
    }
}
