//! Which TriCore instructions an ELF's code uses, and how Capstone prints their operands:
//! `cargo run -p mcds --features capstone --example insn_stats -- <elf> [mnemonic]`.
//! Used to see which instructions the register model (`replay`) must know.

use std::collections::BTreeMap;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1]).expect("read the ELF");
    let only = args.get(2).cloned();
    use capstone::arch::tricore::ArchMode;
    use capstone::prelude::*;
    let cs = Capstone::new().tricore().mode(ArchMode::TriCore162).detail(true).build().unwrap();
    use object::{Object, ObjectSection, SectionKind};
    let obj = object::File::parse(&*data).unwrap();
    let mut count: BTreeMap<String, (u64, Vec<String>)> = BTreeMap::new();
    let mut total = 0u64;
    for s in obj.sections().filter(|s| s.kind() == SectionKind::Text) {
        let Ok(bytes) = s.data() else { continue };
        let mut off = 0usize;
        while off + 2 <= bytes.len() {
            let addr = s.address() + off as u64;
            match cs.disasm_count(&bytes[off..(off + 4).min(bytes.len())], addr, 1) {
                Ok(insns) if insns.len() == 1 => {
                    let i = insns.iter().next().unwrap();
                    let m = i.mnemonic().unwrap_or("?").to_string();
                    let e = count.entry(m).or_default();
                    e.0 += 1;
                    total += 1;
                    let shape = shape(i.op_str().unwrap_or(""));
                    if !e.1.iter().any(|x| x.ends_with(&format!(" {shape}"))) && e.1.len() < 12 {
                        e.1.push(format!("{:08x} {}  ={} {shape}", addr, i.op_str().unwrap_or(""), ""));
                    }
                    off += i.bytes().len();
                }
                _ => off += 2,
            }
        }
    }
    let mut v: Vec<_> = count.into_iter().collect();
    v.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    println!("{total} instructions, {} mnemonics", v.len());
    for (m, (n, ex)) in v {
        if only.as_deref().is_some_and(|o| o != m) {
            continue;
        }
        println!("{n:7} {m:10} {}", ex.join(" | "));
    }
}

/// The operand form: registers by class, numbers as N.
fn shape(ops: &str) -> String {
    let mut out = String::new();
    let b = ops.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i] as char;
        if (c == 'd' || c == 'a' || c == 'e' || c == 'p') && i + 1 < b.len() && (b[i + 1] as char).is_ascii_digit() && (i == 0 || !(b[i - 1] as char).is_ascii_alphanumeric()) {
            out.push(c.to_ascii_uppercase());
            i += 1;
            while i < b.len() && (b[i] as char).is_ascii_digit() {
                i += 1;
            }
        } else if c.is_ascii_digit() {
            out.push('N');
            if ops[i..].starts_with("0x") {
                i += 2;
            }
            while i < b.len() && (b[i] as char).is_ascii_hexdigit() {
                i += 1;
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}
