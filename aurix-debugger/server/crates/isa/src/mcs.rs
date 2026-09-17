//! GTM Multi Channel Sequencer (MCS24-3) instruction set, from the TC3xx user manual part 2,
//! chapter 28.17.7 (figures 113/114).
//!
//! Every instruction is one 32-bit word. Code and memory addresses are byte addresses with bits 1:0
//! clear (the instruction fields hold them shifted, `C << 2`), so a label value is used directly.

use std::sync::OnceLock;

/// Internal register indices (28.17.9): R0..R7 = 0..7, STA = 8, ... DSTAX = 0x1B.
pub const REGS: [&str; 28] = [
    "R0", "R1", "R2", "R3", "R4", "R5", "R6", "R7", "STA", "ACB", "CTRG", "STRG", "TBU_TS0", "TBU_TS1",
    "TBU_TS2", "MHB", "RS0", "RS1", "RS2", "RS3", "RS4", "RS5", "RS6", "RS7", "GMI0", "GMI1", "DSTA",
    "DSTAX",
];

/// Encoding of the dummy register ZERO of ARU reads (any value 8..15 is allowed; GHS asmcs uses 12).
pub const ZERO_REG: u32 = 12;

/// Register index of a register name (case insensitive), `ZERO` included.
pub fn reg_index(name: &str) -> Option<u32> {
    if name.eq_ignore_ascii_case("ZERO") {
        return Some(ZERO_REG);
    }
    REGS.iter().position(|r| r.eq_ignore_ascii_case(name)).map(|i| i as u32)
}

/// Operand field of an instruction word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    /// Register in 4 bits at the shift.
    Reg4(u8),
    /// Register in 4 bits at the shift plus bit 4 at the second position.
    Reg5(u8, u8),
    /// General purpose register in 3 bits at the shift (bus master instructions).
    Reg3(u8),
    /// ARU register (R0..R7 or ZERO) in 4 bits at the shift.
    Areg(u8),
    /// Unsigned literal: shift, width.
    Lit(u8, u8),
    /// Bit index: 4 bits at the shift plus bit 4 at the second position.
    Bit5(u8, u8),
    /// Memory address, bits 14:2.
    Addr,
    /// Signed memory offset, bits 14:2.
    Off,
    /// Bus master address, bits 15:2.
    Bus,
}

/// Effect of an instruction on the program flow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    Next,
    /// Jump to the address operand.
    Jump,
    /// Jump to the address operand or continue.
    Branch,
    /// Call the address operand.
    Call,
    Ret,
    /// Jump to R6.
    JumpR6,
    /// Jump to R6 or continue.
    BranchR6,
    /// Call R6.
    CallR6,
    /// Suspends the channel until an event (WURM, WURMX, WURCX, WUCE).
    Wait,
}

pub struct Op {
    pub name: &'static str,
    pattern: &'static str,
    pub args: &'static [Field],
    /// Value of the last operand when the source leaves it out.
    pub default_last: Option<u32>,
    pub flow: Flow,
}

use Field::*;
use Flow::*;

const fn op(name: &'static str, pattern: &'static str, args: &'static [Field], flow: Flow) -> Op {
    Op { name, pattern, args, default_last: None, flow }
}

const fn opt(name: &'static str, pattern: &'static str, args: &'static [Field], default: u32) -> Op {
    Op { name, pattern, args, default_last: Some(default), flow: Next }
}

const A4: Field = Reg4(24);
const B4: Field = Reg4(20);
const A5: Field = Reg5(24, 14);
const B5: Field = Reg5(20, 12);
const L24: Field = Lit(0, 24);
const W: u32 = 24;

pub static OPS: &[Op] = &[
    op("MOVL", "0001aaaacccccccccccccccccccccccc", &[A4, L24], Next),
    op("MOV", "1010aaaabbbb0000-a-b------------", &[A5, B5], Next),
    op("MRD", "1010aaaa----0001-ccccccccccccc--", &[A4, Addr], Next),
    op("MWR", "1010aaaa----0010-ccccccccccccc--", &[A4, Addr], Next),
    opt("MRDI", "1010aaaabbbb0011-ccccccccccccc--", &[A4, B4, Off], 0),
    opt("MWRI", "1010aaaabbbb0100-ccccccccccccc--", &[A4, B4, Off], 0),
    op("POP", "1010aaaa----0101----------------", &[A4], Next),
    op("PUSH", "1010aaaa----0110----------------", &[A4], Next),
    op("MWRL", "1010aaaa----0111-ccccccccccccc--", &[A4, Addr], Next),
    op("MWRIL", "1010aaaabbbb1000----------------", &[A4, B4], Next),
    op("BRD", "1010-aaa----1001cccccccccccccc--", &[Reg3(24), Bus], Next),
    op("BWR", "1010-aaa----1010cccccccccccccc--", &[Reg3(24), Bus], Next),
    op("BRDI", "1010-aaa-bbb1011----------------", &[Reg3(24), Reg3(20)], Next),
    op("BWRI", "1010-aaa-bbb1100----------------", &[Reg3(24), Reg3(20)], Next),
    op("MRDIO", "1010aaaabbbb1101-a-b------------", &[A5, B5], Next),
    op("MWRIO", "1010aaaabbbb1110-a-b------------", &[A5, B5], Next),
    op("XCHB", "1010aaaabbbb1111-a-b------------", &[A5, B5], Next),
    op("ARD", "1011aaaabbbb0000-------ccccccccc", &[Areg(24), Areg(20), Lit(0, 9)], Next),
    op("AWR", "1011aaaabbbb0001-----------ccccc", &[A4, B4, Lit(0, 5)], Next),
    op("NARD", "1011aaaabbbb0010-------ccccccccc", &[Areg(24), Areg(20), Lit(0, 9)], Next),
    op("NARDI", "1011aaaabbbb0011----------------", &[Areg(24), Areg(20)], Next),
    op("ARDI", "1011aaaabbbb0100----------------", &[Areg(24), Areg(20)], Next),
    op("AWRI", "1011aaaabbbb0101----------------", &[A4, B4], Next),
    op("SETB", "1011aaaabbbb0110-a-b------------", &[A5, B5], Next),
    op("CLRB", "1011aaaabbbb0111-a-b------------", &[A5, B5], Next),
    op("ADDL", "0010aaaacccccccccccccccccccccccc", &[A4, L24], Next),
    op("ADD", "1100aaaabbbb0000-a-b------------", &[A5, B5], Next),
    op("SUBL", "0011aaaacccccccccccccccccccccccc", &[A4, L24], Next),
    op("SUB", "1100aaaabbbb0001-a-b------------", &[A5, B5], Next),
    op("NEG", "1100aaaabbbb0010-a-b------------", &[A5, B5], Next),
    op("ANDL", "0100aaaacccccccccccccccccccccccc", &[A4, L24], Next),
    op("AND", "1100aaaabbbb0011-a-b------------", &[A5, B5], Next),
    op("ORL", "0101aaaacccccccccccccccccccccccc", &[A4, L24], Next),
    op("OR", "1100aaaabbbb0100-a-b------------", &[A5, B5], Next),
    op("XORL", "0110aaaacccccccccccccccccccccccc", &[A4, L24], Next),
    op("XOR", "1100aaaabbbb0101-a-b------------", &[A5, B5], Next),
    op("SHR", "1100aaaa----0110-a---------ccccc", &[A5, Lit(0, 5)], Next),
    op("SHL", "1100aaaa----0111-a---------ccccc", &[A5, Lit(0, 5)], Next),
    opt("MULU", "1100aaaabbbb1000-a-b-------ccccc", &[A5, B5, Lit(0, 5)], W),
    opt("MULS", "1100aaaabbbb1001-a-b-------ccccc", &[A5, B5, Lit(0, 5)], W),
    opt("DIVU", "1100aaaabbbb1010-a-b-------ccccc", &[A5, B5, Lit(0, 5)], W),
    opt("DIVS", "1100aaaabbbb1011-a-b-------ccccc", &[A5, B5, Lit(0, 5)], W),
    op("MINU", "1100aaaabbbb1100-a-b------------", &[A5, B5], Next),
    op("MINS", "1100aaaabbbb1101-a-b------------", &[A5, B5], Next),
    op("MAXU", "1100aaaabbbb1110-a-b------------", &[A5, B5], Next),
    op("MAXS", "1100aaaabbbb1111-a-b------------", &[A5, B5], Next),
    op("ASL", "1101aaaabbbb0011-a-b------------", &[A5, B5], Next),
    op("ASRU", "1101aaaabbbb0100-a-b------------", &[A5, B5], Next),
    op("ASRS", "1101aaaabbbb0101-a-b------------", &[A5, B5], Next),
    op("ADDC", "1101aaaabbbb0110-a-b------------", &[A5, B5], Next),
    op("SUBC", "1101aaaabbbb0111-a-b------------", &[A5, B5], Next),
    op("ATUL", "0111aaaacccccccccccccccccccccccc", &[A4, L24], Next),
    op("ATU", "1101aaaabbbb0000-a-b------------", &[A5, B5], Next),
    op("ATSL", "1000aaaacccccccccccccccccccccccc", &[A4, L24], Next),
    op("ATS", "1101aaaabbbb0001-a-b------------", &[A5, B5], Next),
    op("BTL", "1001aaaacccccccccccccccccccccccc", &[A4, L24], Next),
    op("BT", "1101aaaabbbb0010-a-b------------", &[A5, B5], Next),
    op("JMP", "1110--------0000-ccccccccccccc--", &[Addr], Jump),
    op("JBS", "1110aaaabbbb0001-ccccccccccccc--", &[A4, Lit(20, 4), Addr], Branch),
    op("JBC", "1110aaaabbbb0010-ccccccccccccc--", &[A4, Lit(20, 4), Addr], Branch),
    op("CALL", "1110--------0011-ccccccccccccc--", &[Addr], Call),
    op("RET", "1110--------0100----------------", &[], Ret),
    op("JMPI", "1110--------0101----------------", &[], JumpR6),
    op("JBSI", "1110aaaabbbb0110-a-b------------", &[A5, Bit5(20, 12)], BranchR6),
    op("JBCI", "1110aaaabbbb0111-a-b------------", &[A5, Bit5(20, 12)], BranchR6),
    op("CALLI", "1110--------1000----------------", &[], CallR6),
    op("WURM", "1111aaaabbbb0000cccccccccccccccc", &[A4, B4, Lit(0, 16)], Wait),
    op("WURMX", "1111aaaabbbb0001---b------------", &[A4, B5], Wait),
    op("WURCX", "1111aaaabbbb0010---b------------", &[A4, B5], Wait),
    op("WUCE", "1111aaaabbbb0011----------------", &[A4, B4], Wait),
    op("NOP", "0000----------------------------", &[], Next),
];

fn mask_value(pattern: &str) -> (u32, u32) {
    pattern.bytes().fold((0, 0), |(m, v), c| match c {
        b'0' => (m << 1 | 1, v << 1),
        b'1' => (m << 1 | 1, v << 1 | 1),
        _ => (m << 1, v << 1),
    })
}

fn masks() -> &'static [(u32, u32)] {
    static M: OnceLock<Vec<(u32, u32)>> = OnceLock::new();
    M.get_or_init(|| OPS.iter().map(|o| mask_value(o.pattern)).collect())
}

pub fn find_op(name: &str) -> Option<&'static Op> {
    OPS.iter().find(|o| o.name.eq_ignore_ascii_case(name))
}

/// The instruction of a word, or None for an invalid opcode (which disables the channel).
pub fn decode(word: u32) -> Option<&'static Op> {
    masks().iter().position(|&(m, v)| word & m == v).map(|i| &OPS[i])
}

fn bits(v: u32, shift: u8, width: u8) -> u32 {
    (v >> shift) & ((1u32 << width) - 1)
}

/// Operand values of a decoded word, in source order. Addresses are byte addresses, offsets are
/// sign extended.
pub fn operands(op: &Op, word: u32) -> Vec<i64> {
    op.args
        .iter()
        .map(|f| match *f {
            Reg4(s) | Areg(s) => bits(word, s, 4) as i64,
            Reg5(s, x) | Bit5(s, x) => (bits(word, s, 4) | bits(word, x, 1) << 4) as i64,
            Reg3(s) => bits(word, s, 3) as i64,
            Lit(s, w) => bits(word, s, w) as i64,
            Addr => (word & 0x7FFC) as i64,
            Off => (((word & 0x7FFC) << 17) as i32 >> 17) as i64,
            Bus => (word & 0xFFFC) as i64,
        })
        .collect()
}

/// Checks and places an operand value into its field.
pub fn encode_field(f: Field, v: i64) -> Result<u32, String> {
    let range = |lo: i64, hi: i64| {
        if v < lo || v > hi {
            Err(format!("value {v} out of range {lo}..={hi}"))
        } else {
            Ok(v as u32)
        }
    };
    Ok(match f {
        Reg4(s) | Areg(s) => range(0, 15)? << s,
        Reg5(s, x) | Bit5(s, x) => {
            let r = range(0, 31)?;
            (r & 15) << s | (r >> 4) << x
        }
        Reg3(s) => range(0, 7)? << s,
        // Literals are taken modulo their width (e.g. "~MASK" or negative values).
        Lit(s, w) => ((v as u32) & ((1u32 << w) - 1)) << s,
        Addr => {
            if v & 3 != 0 {
                return Err(format!("address {v:#x} is not word aligned"));
            }
            range(0, 0x7FFC)?
        }
        Off => {
            if v & 3 != 0 {
                return Err(format!("offset {v} is not word aligned"));
            }
            range(-0x4000, 0x3FFC)? & 0x7FFC
        }
        Bus => {
            if v & 3 != 0 {
                return Err(format!("bus address {v:#x} is not word aligned"));
            }
            range(0, 0xFFFC)?
        }
    })
}

/// Encodes an instruction from its operand values (a missing optional last operand is filled in).
pub fn encode(op: &Op, operands: &[i64]) -> Result<u32, String> {
    let mut vals = operands.to_vec();
    if vals.len() + 1 == op.args.len() {
        if let Some(d) = op.default_last {
            vals.push(d as i64);
        }
    }
    if vals.len() != op.args.len() {
        return Err(format!("{} takes {} operands, got {}", op.name, op.args.len(), operands.len()));
    }
    let (_, value) = mask_value(op.pattern);
    op.args.iter().zip(&vals).try_fold(value, |w, (f, v)| {
        encode_field(*f, *v).map(|b| w | b).map_err(|e| format!("{}: {e}", op.name))
    })
}

fn reg_name(i: i64, areg: bool) -> String {
    if areg && i >= 8 {
        return "ZERO".into();
    }
    REGS.get(i as usize).map(|s| s.to_string()).unwrap_or_else(|| format!("REG{i:#x}"))
}

/// Disassembly text of a word ("movl R0, 0x12"). `label` names an address operand.
pub fn format(word: u32, label: &dyn Fn(u32) -> Option<String>) -> String {
    let Some(op) = decode(word) else {
        return format!(".word 0x{word:08x}");
    };
    let args: Vec<String> = op
        .args
        .iter()
        .zip(operands(op, word))
        .map(|(f, v)| match f {
            Reg4(_) | Reg5(..) | Reg3(_) => reg_name(v, false),
            Areg(_) => reg_name(v, true),
            Addr => label(v as u32).unwrap_or_else(|| format!("0x{v:x}")),
            Bus => format!("0x{v:x}"),
            Off | Bit5(..) => v.to_string(),
            Lit(_, w) if *w <= 5 => v.to_string(),
            Lit(..) => format!("0x{v:x}"),
        })
        .collect();
    let name = op.name.to_ascii_lowercase();
    if args.is_empty() {
        name
    } else {
        format!("{name} {}", args.join(", "))
    }
}

/// Branch target encoded in a word (jump, branch or call with an address operand).
pub fn direct_target(word: u32) -> Option<u32> {
    let op = decode(word)?;
    matches!(op.flow, Jump | Branch | Call).then_some(word & 0x7FFC)
}

/// A self-jump at `addr`: a channel reaching it loops there (software breakpoint).
pub fn self_jump(addr: u32) -> u32 {
    0xE000_0000 | (addr & 0x7FFC)
}

/// Addresses at which a channel at `pc` can be after executing the word there. `r6` is the value
/// of R6 and `ret` the return address on top of the stack. With `over`, a call counts as done
/// when it returns (pc + 4).
pub fn successors(word: u32, pc: u32, r6: u32, ret: Option<u32>, over: bool) -> Vec<u32> {
    let next = pc.wrapping_add(4) & 0xFFFC;
    let r6 = r6 & 0x7FFC;
    let mut v = match decode(word).map(|o| o.flow) {
        None => vec![],
        Some(Next) | Some(Wait) => vec![next],
        Some(Jump) => vec![word & 0x7FFC],
        Some(Branch) => vec![next, word & 0x7FFC],
        Some(Call) if over => vec![next],
        Some(Call) => vec![word & 0x7FFC],
        Some(CallR6) if over => vec![next],
        Some(CallR6) | Some(JumpR6) => vec![r6],
        Some(BranchR6) => vec![next, r6],
        Some(Ret) => ret.map(|r| vec![r & 0x7FFC]).unwrap_or_default(),
    };
    v.dedup();
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_are_32_bits_and_unambiguous() {
        for o in OPS {
            assert_eq!(o.pattern.len(), 32, "{}", o.name);
        }
        let m = masks();
        for (i, a) in m.iter().enumerate() {
            for (j, b) in m.iter().enumerate().skip(i + 1) {
                let common = a.0 & b.0;
                assert!(a.1 & common != b.1 & common, "{} overlaps {}", OPS[i].name, OPS[j].name);
            }
        }
    }

    #[test]
    fn encode_decode_round_trip() {
        let jmp = encode(find_op("jmp").unwrap(), &[0x200]).unwrap();
        assert_eq!(jmp, 0xE000_0200);
        assert_eq!(jmp, self_jump(0x200));
        let jbs = encode(find_op("JBS").unwrap(), &[8, 5, 0x25C]).unwrap();
        let op = decode(jbs).unwrap();
        assert_eq!(op.name, "JBS");
        assert_eq!(operands(op, jbs), vec![8, 5, 0x25C]);
        assert_eq!(format(jbs, &|_| None), "jbs STA, 5, 0x25c");
        let mov = encode(find_op("MOV").unwrap(), &[0, 0x18]).unwrap();
        assert_eq!(format(mov, &|_| None), "mov R0, GMI0");
        let mulu = encode(find_op("MULU").unwrap(), &[1, 0]).unwrap();
        assert_eq!(operands(decode(mulu).unwrap(), mulu), vec![1, 0, 24]);
        let movl = encode(find_op("MOVL").unwrap(), &[0, -1]).unwrap();
        assert_eq!(movl, 0x10FF_FFFF);
        assert!(decode(0xF00F_0000).is_none());
    }

    #[test]
    fn successor_addresses() {
        let jbs = encode(find_op("JBS").unwrap(), &[8, 5, 0x300]).unwrap();
        assert_eq!(successors(jbs, 0x200, 0, None, false), vec![0x204, 0x300]);
        let call = encode(find_op("CALL").unwrap(), &[0x400]).unwrap();
        assert_eq!(successors(call, 0x200, 0, None, false), vec![0x400]);
        assert_eq!(successors(call, 0x200, 0, None, true), vec![0x204]);
        let ret = encode(find_op("RET").unwrap(), &[]).unwrap();
        assert_eq!(successors(ret, 0x400, 0, Some(0x204), false), vec![0x204]);
        let jmpi = encode(find_op("JMPI").unwrap(), &[]).unwrap();
        assert_eq!(successors(jmpi, 0x400, 0x123_0500, None, false), vec![0x500]);
    }
}
