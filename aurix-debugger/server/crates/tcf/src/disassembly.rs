//! TCF Disassembly service: TriCore (TC1.6.2) instructions decoded with Capstone.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use capstone::arch::tricore::ArchMode;
use capstone::prelude::*;
use serde_json::{json, Value};

use aurix_core::ops;

use crate::Shared;

const ERR_OTHER: i64 = 1;
const ERR_JSON_SYNTAX: i64 = 2;
const ERR_INV_ADDRESS: i64 = 17;
const ERR_INV_COMMAND: i64 = 25;

/// Largest range disassembled per request.
const MAX_SIZE: u64 = 0x10000;

pub type Res<T> = Result<T, (i64, String)>;

fn arg_u64(args: &[String], i: usize) -> Res<u64> {
    let s = args.get(i).ok_or((ERR_JSON_SYNTAX, format!("missing argument {}", i + 1)))?;
    serde_json::from_str::<Value>(s)
        .ok()
        .and_then(|v| v.as_u64())
        .ok_or((ERR_JSON_SYNTAX, format!("argument {} must be a number", i + 1)))
}

fn capstone() -> Res<Capstone> {
    Capstone::new().tricore().mode(ArchMode::TriCore162).build().map_err(|e| (ERR_OTHER, format!("disassembler: {e}")))
}

/// Disassembles `code` located at `addr` into (address, length, text).
/// Undecodable instructions become `.short`/`.word` lines of their TriCore
/// length (bit 0 of the first byte set: 32 bit); an instruction cut off at
/// the end of `code` is left out.
pub fn disassemble_bytes(cs: &Capstone, code: &[u8], addr: u32) -> Vec<(u32, usize, String)> {
    let mut out = Vec::new();
    let mut off = 0usize;
    while off < code.len() {
        let rest = &code[off..];
        let a = addr.wrapping_add(off as u32);
        let decoded = cs.disasm_count(rest, a as u64, 1).ok().and_then(|insns| {
            insns.iter().next().map(|i| {
                let text = match (i.mnemonic(), i.op_str()) {
                    (Some(m), Some(o)) if !o.is_empty() => format!("{m:<8}{o}"),
                    (Some(m), _) => m.to_string(),
                    _ => "?".to_string(),
                };
                (i.len(), text)
            })
        });
        let (len, text) = match decoded {
            Some(d) => d,
            None => {
                let len = if rest[0] & 1 == 1 { 4 } else { 2 };
                if rest.len() < len {
                    break;
                }
                let text = if len == 4 {
                    format!(".word   0x{:08x}", u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]))
                } else {
                    format!(".short  0x{:04x}", u16::from_le_bytes([rest[0], rest[1]]))
                };
                (len, text)
            }
        };
        out.push((a, len, text));
        off += len;
    }
    out
}

/// Appends the function containing a direct branch or call target, like objdump:
/// `call #0x80131140 <Os_EnableInterruptSource>`, `jz %d15, #0x80142a0c <Func+0x28>`.
/// Capstone prints resolved PC-relative and absolute targets as `#0x<address>`;
/// indirect branches (`ji`, `calli`, ...) carry no address and stay unchanged.
/// `function_at` returns the function name and the offset of the address in it.
pub fn annotate_target(text: &str, function_at: impl Fn(u32) -> Option<(String, u32)>) -> String {
    let mnemonic = text.split_whitespace().next().unwrap_or("");
    let branch = mnemonic.starts_with('j') || mnemonic.starts_with("call") || mnemonic.starts_with("fcall") || mnemonic.starts_with("loop");
    let Some(pos) = text.rfind("#0x").filter(|_| branch) else {
        return text.to_string();
    };
    let digits: String = text[pos + 3..].chars().take_while(char::is_ascii_hexdigit).collect();
    match u32::from_str_radix(&digits, 16).ok().and_then(function_at) {
        Some((name, 0)) => format!("{text} <{name}>"),
        Some((name, off)) => format!("{text} <{name}+0x{off:x}>"),
        None => text.to_string(),
    }
}

pub fn dispatch(sh: &Shared, command: &str, args: &[String]) -> Res<Vec<String>> {
    match command {
        "getCapabilities" => Ok(vec![
            String::new(),
            json!([{ "ISA": "TRICORE", "Simplified": false, "PseudoInstruction": false, "OpcodeValue": true }]).to_string(),
        ]),
        "disassemble" => {
            let start = arg_u64(args, 1)? as u32;
            let size = arg_u64(args, 2)?.min(MAX_SIZE) as usize;
            // Exactly the requested range: memory beyond it may be unreadable
            // (reading erased PFLASH raises an ECC error).
            let data = sh
                .handle
                .call(move |cx| ops::read_memory(cx, start, size).map_err(|e| e.to_string()))
                .unwrap_or_else(|| Err("server stopped".into()))
                .map_err(|e| (ERR_INV_ADDRESS, e))?;
            let cs = capstone()?;
            let syms = sh.active_symbols();
            let function_at = |addr: u32| syms.as_ref()?.function_at(addr).map(|f| (f.name.clone(), addr - f.low));
            let lines: Vec<Value> = disassemble_bytes(&cs, &data, start)
                .into_iter()
                .map(|(a, n, text)| {
                    let text = annotate_target(&text, &function_at);
                    let off = (a - start) as usize;
                    json!({
                        "ISA": "TRICORE",
                        "Address": a,
                        "Size": n,
                        "Instruction": [{ "Type": "String", "Text": text }],
                        "OpcodeValue": B64.encode(&data[off..off + n]),
                    })
                })
                .collect();
            Ok(vec![String::new(), json!(lines).to_string()])
        }
        _ => Err((ERR_INV_COMMAND, format!("Unsupported command: Disassembly.{command}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_16_and_32_bit_instructions() {
        let cs = capstone().unwrap();
        // nop (16 bit), ret (16 bit), then an undecodable 32-bit word and a truncated one.
        let code = [0x00, 0x00, 0x00, 0x90, 0xFF, 0xFF, 0xFF, 0xFF, 0x01];
        let lines = disassemble_bytes(&cs, &code, 0x8000_0000);
        assert_eq!(lines[0], (0x8000_0000, 2, "nop".to_string()));
        assert_eq!(lines[1], (0x8000_0002, 2, "ret".to_string()));
        assert_eq!((lines[2].0, lines[2].1), (0x8000_0004, 4));
        assert_eq!(lines.len(), 3, "{lines:?}");
    }

    #[test]
    fn annotates_direct_branch_targets() {
        let lookup = |a: u32| match a {
            0x8013_1140 => Some(("Os_EnableInterruptSource".to_string(), 0)),
            0x8014_2a0c => Some(("App_Task10ms".to_string(), 0x28)),
            _ => None,
        };
        assert_eq!(annotate_target("call    #0x80131140", lookup), "call    #0x80131140 <Os_EnableInterruptSource>");
        assert_eq!(annotate_target("jz      %d15, #0x80142a0c", lookup), "jz      %d15, #0x80142a0c <App_Task10ms+0x28>");
        // Not a branch, an indirect call, and a target outside any function.
        assert_eq!(annotate_target("mov     %d4, #0x80131140", lookup), "mov     %d4, #0x80131140");
        assert_eq!(annotate_target("calli   %a2", lookup), "calli   %a2");
        assert_eq!(annotate_target("call    #0x90000000", lookup), "call    #0x90000000");
    }

    /// Calls in Module_Init resolve to the names objdump shows.
    #[test]
    fn real_elf_call_names() {
        let path = std::path::Path::new("C:/workspace/app/build/bin/app.elf");
        if !path.exists() {
            return;
        }
        let img = aurix_core::image::Image::load(path, 0, None).unwrap();
        let syms = symbols::Symbols::load(path).unwrap();
        let (lo, hi) = (0x8014_29a0u32, 0x8014_29beu32);
        let s = img.sections.iter().find(|s| s.addr <= lo && s.end() >= hi as u64).unwrap();
        let code = &s.data[(lo - s.addr) as usize..(hi - s.addr) as usize];
        let function_at = |a: u32| syms.function_at(a).map(|f| (f.name.clone(), a - f.low));
        let calls: Vec<String> = disassemble_bytes(&capstone().unwrap(), code, lo)
            .into_iter()
            .map(|(_, _, t)| annotate_target(&t, &function_at))
            .filter(|t| t.starts_with("call"))
            .collect();
        eprintln!("{calls:#?}");
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(calls.iter().all(|t| t.ends_with("#0x80131140 <Os_EnableInterruptSource>")), "{calls:?}");
    }

    /// Prints a function of the test ELF for comparison with tricore-elf-objdump.
    #[test]
    fn real_elf_function() {
        let path = std::path::Path::new("C:/workspace/app/build/bin/app.elf");
        if !path.exists() {
            return;
        }
        let img = aurix_core::image::Image::load(path, 0, None).unwrap();
        let (lo, hi) = (0x8014_29a0u32, 0x8014_29beu32);
        let s = img.sections.iter().find(|s| s.addr <= lo && s.end() >= hi as u64).unwrap();
        let code = &s.data[(lo - s.addr) as usize..(hi - s.addr) as usize];
        let lines = disassemble_bytes(&capstone().unwrap(), code, lo);
        for (a, n, t) in &lines {
            eprintln!("{a:08x} [{n}] {t}");
        }
        assert!(lines.iter().all(|(_, _, t)| !t.starts_with('.')), "undecoded instructions: {lines:?}");
    }
}
