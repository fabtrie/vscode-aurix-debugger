//! Prints the encodings of instructions whose text contains a pattern:
//! `cargo run -p mcds --features capstone --example insn_find -- <elf> <mnemonic> <operand text> [max]`.
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&a[1]).unwrap();
    let max: usize = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(5);
    use capstone::arch::tricore::ArchMode;
    use capstone::prelude::*;
    use object::{Object, ObjectSection, SectionKind};
    let cs = Capstone::new().tricore().mode(ArchMode::TriCore162).build().unwrap();
    let obj = object::File::parse(&*data).unwrap();
    let mut n = 0;
    for s in obj.sections().filter(|s| s.kind() == SectionKind::Text) {
        let Ok(b) = s.data() else { continue };
        let mut off = 0;
        while off + 2 <= b.len() && n < max {
            match cs.disasm_count(&b[off..(off + 4).min(b.len())], s.address() + off as u64, 1) {
                Ok(i) if i.len() == 1 => {
                    let i = i.iter().next().unwrap();
                    let (m, o) = (i.mnemonic().unwrap_or(""), i.op_str().unwrap_or(""));
                    if m == a[2] && o.contains(&a[3]) {
                        let hex: Vec<String> = i.bytes().iter().map(|x| format!("{x:02x}")).collect();
                        println!("{:08x} {:12} {m} {o}", i.address(), hex.join(""));
                        n += 1;
                    }
                    off += i.bytes().len();
                }
                _ => off += 2,
            }
        }
    }
}
