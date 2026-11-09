//! XC800 instruction set: the 8051 instructions of the TC3xx standby controller (SCR).
//!
//! Instructions are 1 to 3 bytes at any address of the 64 KB code space. Opcode `A5`, reserved
//! on a plain 8051, is the XC800's `TRAP` (software breakpoint, enabled by EO.TRAP_EN, TC3xx UM
//! 49.5.2.3.6).

/// How an instruction continues.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flow {
    /// Falls through to the next instruction.
    Next,
    /// Unconditional jump (`AJMP`, `LJMP`, `SJMP`).
    Jump,
    /// Conditional jump: the target or the next instruction (`JZ`, `CJNE`, `DJNZ`, ...).
    Branch,
    /// Subroutine call (`ACALL`, `LCALL`): returns to the next instruction.
    Call,
    /// `RET`, `RETI`: continues at the address on the stack.
    Ret,
    /// `JMP @A+DPTR`: target known only at run time.
    Indirect,
    /// `TRAP`: enters the debug monitor.
    Trap,
}

/// One decoded instruction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Insn {
    pub len: u32,
    pub flow: Flow,
    /// Jump or call target, when it is encoded in the instruction.
    pub target: Option<u32>,
    pub mnemonic: &'static str,
    /// Operands as text, a target as `{target}` for [`format`] to label.
    pub operands: String,
}

/// Length of the instruction starting with `op`.
pub fn length(op: u8) -> u32 {
    match op {
        // 3 bytes
        0x02 | 0x10 | 0x12 | 0x20 | 0x30 | 0x43 | 0x53 | 0x63 | 0x75 | 0x85 | 0x90 | 0xB4..=0xBF | 0xD5 => 3,
        // 2 bytes
        0x05 | 0x15 | 0x24 | 0x25 | 0x34 | 0x35 | 0x40 | 0x42 | 0x44 | 0x45 | 0x50 | 0x52 | 0x54 | 0x55 | 0x60 | 0x62
        | 0x64 | 0x65 | 0x70 | 0x72 | 0x74 | 0x76..=0x7F | 0x80 | 0x82 | 0x86..=0x8F | 0x92 | 0x94 | 0x95 | 0xA0 | 0xA2
        | 0xA6..=0xAF | 0xB0 | 0xB2 | 0xC0 | 0xC2 | 0xC5 | 0xD0 | 0xD2 | 0xD8..=0xDF | 0xE5 | 0xF5 => 2,
        // AJMP / ACALL
        op if op & 0x1F == 0x01 || op & 0x1F == 0x11 => 2,
        _ => 1,
    }
}

fn rel(pc: u32, len: u32, off: u8) -> u32 {
    (pc.wrapping_add(len).wrapping_add(off as i8 as i32 as u32)) & 0xFFFF
}

fn hex8(v: u8) -> String {
    format!("0x{v:02X}")
}

/// A direct address: the core SFRs of the TC3xx SCR by name (UM 49.5, Step B layout as used
/// by its compiler), anything else as a number.
fn direct(v: u8) -> String {
    let name = match v {
        0x80 => "P0",
        0xD0 => "PSW",
        0xD4 => "SP",
        0xD5 => "DPL",
        0xD6 => "DPH",
        0xDA => "B",
        0xE0 => "ACC",
        _ => return hex8(v),
    };
    name.to_string()
}

fn bit(v: u8) -> String {
    if v < 0x80 {
        format!("0x{:02X}.{}", 0x20 + (v >> 3), v & 7)
    } else {
        format!("{}.{}", direct(v & 0xF8), v & 7)
    }
}

/// Decodes the instruction at `pc` from `bytes` (at least [`length`] bytes). `None` if `bytes`
/// is too short.
pub fn decode(bytes: &[u8], pc: u32) -> Option<Insn> {
    let op = *bytes.first()?;
    let len = length(op);
    if (bytes.len() as u32) < len {
        return None;
    }
    let b1 = bytes.get(1).copied().unwrap_or(0);
    let b2 = bytes.get(2).copied().unwrap_or(0);
    let rn = format!("R{}", op & 7);
    let ri = format!("@R{}", op & 1);
    let lo = op & 0x0F;
    // Register forms: x6/x7 = @Ri, x8..xF = Rn.
    let reg = if lo >= 8 { rn.clone() } else { ri.clone() };
    let mut flow = Flow::Next;
    let mut target = None;
    let t = "{target}";
    let (m, o): (&'static str, String) = match op {
        0x00 => ("NOP", String::new()),
        _ if op & 0x1F == 0x01 || op & 0x1F == 0x11 => {
            let a = ((pc + 2) & 0xF800) | ((op as u32 >> 5) << 8) | b1 as u32;
            target = Some(a);
            if op & 0x10 == 0 {
                flow = Flow::Jump;
                ("AJMP", t.into())
            } else {
                flow = Flow::Call;
                ("ACALL", t.into())
            }
        }
        0x02 | 0x12 => {
            target = Some(u16::from_be_bytes([b1, b2]) as u32);
            if op == 0x02 {
                flow = Flow::Jump;
                ("LJMP", t.into())
            } else {
                flow = Flow::Call;
                ("LCALL", t.into())
            }
        }
        0x03 => ("RR", "A".into()),
        0x13 => ("RRC", "A".into()),
        0x23 => ("RL", "A".into()),
        0x33 => ("RLC", "A".into()),
        0x04 => ("INC", "A".into()),
        0x14 => ("DEC", "A".into()),
        0x05 => ("INC", direct(b1)),
        0x15 => ("DEC", direct(b1)),
        0x06..=0x0F => ("INC", reg),
        0x16..=0x1F => ("DEC", reg),
        0x10 | 0x20 | 0x30 => {
            flow = Flow::Branch;
            target = Some(rel(pc, 3, b2));
            (["JBC", "JB", "JNB"][(op >> 4) as usize - 1], format!("{}, {t}", bit(b1)))
        }
        0x40 | 0x50 | 0x60 | 0x70 => {
            flow = Flow::Branch;
            target = Some(rel(pc, 2, b1));
            (["JC", "JNC", "JZ", "JNZ"][(op >> 4) as usize - 4], t.into())
        }
        0x80 => {
            flow = Flow::Jump;
            target = Some(rel(pc, 2, b1));
            ("SJMP", t.into())
        }
        0x22 => {
            flow = Flow::Ret;
            ("RET", String::new())
        }
        0x32 => {
            flow = Flow::Ret;
            ("RETI", String::new())
        }
        0x73 => {
            flow = Flow::Indirect;
            ("JMP", "@A+DPTR".into())
        }
        // ADD/ADDC/ORL/ANL/XRL/SUBB A,src
        0x24 | 0x34 | 0x44 | 0x54 | 0x64 | 0x94 => (alu(op), format!("A, #{}", hex8(b1))),
        0x25 | 0x35 | 0x45 | 0x55 | 0x65 | 0x95 => (alu(op), format!("A, {}", direct(b1))),
        0x26..=0x2F | 0x36..=0x3F | 0x46..=0x4F | 0x56..=0x5F | 0x66..=0x6F | 0x96..=0x9F => (alu(op), format!("A, {reg}")),
        0x42 | 0x52 | 0x62 => (alu(op), format!("{}, A", direct(b1))),
        0x43 | 0x53 | 0x63 => (alu(op), format!("{}, #{}", direct(b1), hex8(b2))),
        0x72 => ("ORL", format!("C, {}", bit(b1))),
        0x82 => ("ANL", format!("C, {}", bit(b1))),
        0xA0 => ("ORL", format!("C, /{}", bit(b1))),
        0xB0 => ("ANL", format!("C, /{}", bit(b1))),
        0x74 => ("MOV", format!("A, #{}", hex8(b1))),
        0x75 => ("MOV", format!("{}, #{}", direct(b1), hex8(b2))),
        0x76 | 0x77 => ("MOV", format!("{ri}, #{}", hex8(b1))),
        0x78..=0x7F => ("MOV", format!("{rn}, #{}", hex8(b1))),
        0x83 => ("MOVC", "A, @A+PC".into()),
        0x93 => ("MOVC", "A, @A+DPTR".into()),
        0x84 => ("DIV", "AB".into()),
        0xA4 => ("MUL", "AB".into()),
        // MOV dest, src: the source comes first in the encoding.
        0x85 => ("MOV", format!("{}, {}", direct(b2), direct(b1))),
        0x86 | 0x87 => ("MOV", format!("{}, {ri}", direct(b1))),
        0x88..=0x8F => ("MOV", format!("{}, {rn}", direct(b1))),
        0x90 => ("MOV", format!("DPTR, #0x{:04X}", u16::from_be_bytes([b1, b2]))),
        0x92 => ("MOV", format!("{}, C", bit(b1))),
        0xA2 => ("MOV", format!("C, {}", bit(b1))),
        0xA3 => ("INC", "DPTR".into()),
        0xA5 => {
            flow = Flow::Trap;
            ("TRAP", String::new())
        }
        0xA6 | 0xA7 => ("MOV", format!("{ri}, {}", direct(b1))),
        0xA8..=0xAF => ("MOV", format!("{rn}, {}", direct(b1))),
        0xB2 => ("CPL", bit(b1)),
        0xB3 => ("CPL", "C".into()),
        0xB4..=0xBF => {
            flow = Flow::Branch;
            target = Some(rel(pc, 3, b2));
            let first = match op {
                0xB4 => format!("A, #{}", hex8(b1)),
                0xB5 => format!("A, {}", direct(b1)),
                _ => format!("{reg}, #{}", hex8(b1)),
            };
            ("CJNE", format!("{first}, {t}"))
        }
        0xC0 => ("PUSH", direct(b1)),
        0xD0 => ("POP", direct(b1)),
        0xC2 => ("CLR", bit(b1)),
        0xC3 => ("CLR", "C".into()),
        0xD2 => ("SETB", bit(b1)),
        0xD3 => ("SETB", "C".into()),
        0xC4 => ("SWAP", "A".into()),
        0xD4 => ("DA", "A".into()),
        0xC5 => ("XCH", format!("A, {}", direct(b1))),
        0xC6..=0xCF => ("XCH", format!("A, {reg}")),
        0xD5 => {
            flow = Flow::Branch;
            target = Some(rel(pc, 3, b2));
            ("DJNZ", format!("{}, {t}", direct(b1)))
        }
        0xD6 | 0xD7 => ("XCHD", format!("A, {ri}")),
        0xD8..=0xDF => {
            flow = Flow::Branch;
            target = Some(rel(pc, 2, b1));
            ("DJNZ", format!("{rn}, {t}"))
        }
        0xE0 => ("MOVX", "A, @DPTR".into()),
        0xE2 | 0xE3 => ("MOVX", format!("A, {ri}")),
        0xF0 => ("MOVX", "@DPTR, A".into()),
        0xF2 | 0xF3 => ("MOVX", format!("{ri}, A")),
        0xE4 => ("CLR", "A".into()),
        0xF4 => ("CPL", "A".into()),
        0xE5 => ("MOV", format!("A, {}", direct(b1))),
        0xE6..=0xEF => ("MOV", format!("A, {reg}")),
        0xF5 => ("MOV", format!("{}, A", direct(b1))),
        0xF6..=0xFF => ("MOV", format!("{reg}, A")),
        _ => ("DB", hex8(op)),
    };
    Some(Insn { len, flow, target, mnemonic: m, operands: o })
}

fn alu(op: u8) -> &'static str {
    match op >> 4 {
        0x2 => "ADD",
        0x3 => "ADDC",
        0x4 => "ORL",
        0x5 => "ANL",
        0x6 => "XRL",
        _ => "SUBB",
    }
}

/// Instruction text; `label` names a target address (e.g. a symbol), otherwise it is shown in
/// hex.
pub fn format(insn: &Insn, label: &dyn Fn(u32) -> Option<String>) -> String {
    let ops = match insn.target {
        Some(a) => insn.operands.replace("{target}", &label(a).unwrap_or_else(|| format!("0x{a:04X}"))),
        None => insn.operands.clone(),
    };
    if ops.is_empty() {
        insn.mnemonic.to_string()
    } else {
        format!("{:<6} {ops}", insn.mnemonic)
    }
}

/// Where execution can continue after the instruction at `pc`: `ret` is the return address on
/// the stack (for `RET`/`RETI`), `over` treats a call as returning to the next instruction.
/// Empty if unknown (`JMP @A+DPTR`, `RET` without `ret`).
pub fn successors(insn: &Insn, pc: u32, ret: Option<u32>, over: bool) -> Vec<u32> {
    let next = (pc + insn.len) & 0xFFFF;
    match insn.flow {
        Flow::Next | Flow::Trap => vec![next],
        Flow::Jump => insn.target.into_iter().collect(),
        Flow::Branch => {
            let mut v = vec![next];
            v.extend(insn.target.filter(|&t| t != next));
            v
        }
        Flow::Call if over => vec![next],
        Flow::Call => insn.target.into_iter().collect(),
        Flow::Ret => ret.into_iter().collect(),
        Flow::Indirect => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(bytes: &[u8], pc: u32) -> String {
        format(&decode(bytes, pc).unwrap(), &|_| None)
    }

    #[test]
    fn scr_startup_code() {
        // XRAM 0x0000 and 0x0079 of the TC377 SCR image (scr2-417.jsonl):
        // two single steps from 0 end at 0x007C, as winIDEA showed.
        let i = decode(&[0x02, 0x00, 0x79], 0).unwrap();
        assert_eq!((i.len, i.flow, i.target), (3, Flow::Jump, Some(0x79)));
        assert_eq!(successors(&i, 0, None, false), vec![0x79]);
        let i = decode(&[0x75, 0xD4, 0x20], 0x79).unwrap();
        assert_eq!(i.len, 3);
        assert_eq!(successors(&i, 0x79, None, false), vec![0x7C]);
        assert_eq!(text(&[0x75, 0xD4, 0x20], 0x79), "MOV    SP, #0x20");
        assert_eq!(text(&[0x12, 0x05, 0x37], 0x7C), "LCALL  0x0537");
        assert_eq!(text(&[0xE5, 0xD5], 0), "MOV    A, DPL");
        assert_eq!(text(&[0x60, 0x03], 0x84), "JZ     0x0089");
        assert_eq!(text(&[0x32], 0), "RETI");
    }

    #[test]
    fn lengths_match_the_decoder() {
        for op in 0..=255u8 {
            let i = decode(&[op, 0, 0], 0x100).unwrap();
            assert_eq!(i.len, length(op), "opcode {op:02X}");
            assert_ne!(i.mnemonic, "DB", "opcode {op:02X} undecoded");
        }
    }

    #[test]
    fn jumps_and_calls() {
        // AJMP/ACALL: 11-bit target in the 2 KB page of the next instruction.
        let i = decode(&[0xE1, 0x23], 0x07FE).unwrap();
        assert_eq!((i.flow, i.target), (Flow::Jump, Some(0x0F23)));
        let i = decode(&[0x31, 0x10], 0x1000).unwrap();
        assert_eq!((i.flow, i.target), (Flow::Call, Some(0x1110)));
        assert_eq!(successors(&i, 0x1000, None, true), vec![0x1002]);
        assert_eq!(successors(&i, 0x1000, None, false), vec![0x1110]);
        // SJMP backwards, DJNZ, CJNE.
        assert_eq!(decode(&[0x80, 0xFE], 0x200).unwrap().target, Some(0x200));
        let i = decode(&[0xD8, 0xFC], 0x110).unwrap();
        assert_eq!(successors(&i, 0x110, None, false), vec![0x112, 0x10E]);
        assert_eq!(text(&[0xB4, 0x05, 0x02], 0x10), "CJNE   A, #0x05, 0x0015");
        assert_eq!(text(&[0x20, 0x07, 0x00], 0), "JB     0x20.7, 0x0003");
        assert_eq!(text(&[0x30, 0xE7, 0x00], 0), "JNB    ACC.7, 0x0003");
        // RET needs the stack; JMP @A+DPTR is unknown.
        let r = decode(&[0x22], 0).unwrap();
        assert!(successors(&r, 0, None, false).is_empty());
        assert_eq!(successors(&r, 0, Some(0x123), false), vec![0x123]);
        assert!(successors(&decode(&[0x73], 0).unwrap(), 0, None, false).is_empty());
        assert_eq!(text(&[0x85, 0xE0, 0xD5], 0), "MOV    DPL, ACC");
        assert_eq!(decode(&[0xA5], 0).unwrap().flow, Flow::Trap);
        assert!(decode(&[0x02, 0x00], 0).is_none());
    }
}
