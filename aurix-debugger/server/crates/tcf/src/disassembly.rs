//! TCF Disassembly service: TriCore (TC1.6.2) instructions decoded with Capstone.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use capstone::arch::arm;
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

/// Instruction set of a core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Isa {
    TriCore,
    /// Thumb-2 of the Cortex-M (HSM).
    Thumb,
    /// GTM MCS (32-bit instructions, decoded by the `isa` crate).
    Mcs,
}

impl Isa {
    fn for_ctx(sh: &Shared, ctx: &str) -> Isa {
        match crate::contexts::find_core(&sh.handle, ctx) {
            Some(c) if c.arch.name == "cortex_m" => Isa::Thumb,
            Some(c) if c.arch.name == "gtm_mcs" => Isa::Mcs,
            _ => Isa::TriCore,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Isa::TriCore => "TRICORE",
            Isa::Thumb => "Thumb",
            Isa::Mcs => "MCS",
        }
    }

    /// Length of an undecodable instruction starting with `b0`, `b1`.
    fn insn_len(self, b0: u8, b1: u8) -> usize {
        match self {
            Isa::TriCore if b0 & 1 == 1 => 4,
            Isa::Mcs => 4,
            Isa::Thumb if aurix_core::unwind_arm::is_thumb32(u16::from_le_bytes([b0, b1])) => 4,
            _ => 2,
        }
    }
}

fn capstone(isa: Isa) -> Res<Capstone> {
    match isa {
        Isa::TriCore => Capstone::new().tricore().mode(ArchMode::TriCore162).build(),
        Isa::Thumb => Capstone::new().arm().mode(arm::ArchMode::Thumb).extra_mode([arm::ArchExtraMode::MClass].into_iter()).build(),
        Isa::Mcs => return Err((ERR_OTHER, "no Capstone disassembler for the MCS".into())),
    }
    .map_err(|e| (ERR_OTHER, format!("disassembler: {e}")))
}

/// Disassembles MCS words at `addr` (32-bit instructions; a cut-off word is left out). Branch
/// targets are shown as `label` names them.
pub fn disassemble_mcs(code: &[u8], addr: u32, label: &dyn Fn(u32) -> Option<String>) -> Vec<(u32, usize, String)> {
    code.chunks_exact(4)
        .enumerate()
        .map(|(i, w)| (addr + 4 * i as u32, 4, isa::mcs::format(u32::from_le_bytes([w[0], w[1], w[2], w[3]]), label)))
        .collect()
}

/// Disassembles `code` located at `addr` into (address, length, text).
/// Undecodable instructions become `.short`/`.word` lines of their length
/// (TriCore: bit 0 of the first byte set, Thumb: a 32-bit prefix half-word);
/// an instruction cut off at the end of `code` is left out.
pub fn disassemble_bytes(cs: &Capstone, isa: Isa, code: &[u8], addr: u32) -> Vec<(u32, usize, String)> {
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
                let len = isa.insn_len(rest[0], rest.get(1).copied().unwrap_or(0));
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
pub fn annotate_target(text: &str, isa: Isa, function_at: impl Fn(u32) -> Option<(String, u32)>) -> String {
    let mnemonic = text.split_whitespace().next().unwrap_or("");
    let branch = match isa {
        Isa::TriCore => mnemonic.starts_with('j') || mnemonic.starts_with("call") || mnemonic.starts_with("fcall") || mnemonic.starts_with("loop"),
        // b, bl, blx, b<cond>, cbz, cbnz (not bic).
        Isa::Thumb => (mnemonic.starts_with('b') && !mnemonic.starts_with("bic")) || mnemonic.starts_with("cb"),
        // Targets are already shown as labels.
        Isa::Mcs => false,
    };
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
        "getCapabilities" => {
            let ctx = args.first().and_then(|s| serde_json::from_str::<String>(s).ok()).unwrap_or_default();
            let isa = Isa::for_ctx(sh, &ctx).name();
            Ok(vec![String::new(), json!([{ "ISA": isa, "Simplified": false, "PseudoInstruction": false, "OpcodeValue": true }]).to_string()])
        }
        "disassemble" => {
            let start = arg_u64(args, 1)? as u32;
            let size = arg_u64(args, 2)?.min(MAX_SIZE) as usize;
            // Exactly the requested range: memory beyond it may be unreadable
            // (reading erased PFLASH raises an ECC error).
            let ctx = args.first().and_then(|s| serde_json::from_str::<String>(s).ok()).unwrap_or_default();
            let core = crate::contexts::find_core(&sh.handle, &ctx).map(|c| c.idx);
            let data = sh
                .handle
                .call(move |cx| match core {
                    Some(t) => ops::read_memory_as(cx, t, start, size),
                    None => ops::read_memory(cx, start, size),
                }
                .map_err(|e| e.to_string()))
                .unwrap_or_else(|| Err("server stopped".into()))
                .map_err(|e| (ERR_INV_ADDRESS, e))?;
            let isa = Isa::for_ctx(sh, &ctx);
            let syms = sh.symbols_for(&ctx);
            let function_at = |addr: u32| syms.as_ref()?.function_at(addr).map(|f| (f.name.clone(), addr - (f.low & !1)));
            let decoded = if isa == Isa::Mcs {
                let label = |a: u32| function_at(a).filter(|(_, off)| *off == 0).map(|(name, _)| name);
                disassemble_mcs(&data, start, &label)
            } else {
                disassemble_bytes(&capstone(isa)?, isa, &data, start)
            };
            let lines: Vec<Value> = decoded
                .into_iter()
                .map(|(a, n, text)| {
                    let text = annotate_target(&text, isa, &function_at);
                    let off = (a - start) as usize;
                    json!({
                        "ISA": isa.name(),
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
        let cs = capstone(Isa::TriCore).unwrap();
        // nop (16 bit), ret (16 bit), then an undecodable 32-bit word and a truncated one.
        let code = [0x00, 0x00, 0x00, 0x90, 0xFF, 0xFF, 0xFF, 0xFF, 0x01];
        let lines = disassemble_bytes(&cs, Isa::TriCore, &code, 0x8000_0000);
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
        assert_eq!(annotate_target("call    #0x80131140", Isa::TriCore, lookup), "call    #0x80131140 <Os_EnableInterruptSource>");
        assert_eq!(annotate_target("jz      %d15, #0x80142a0c", Isa::TriCore, lookup), "jz      %d15, #0x80142a0c <App_Task10ms+0x28>");
        // Not a branch, an indirect call, and a target outside any function.
        assert_eq!(annotate_target("mov     %d4, #0x80131140", Isa::TriCore, lookup), "mov     %d4, #0x80131140");
        assert_eq!(annotate_target("calli   %a2", Isa::TriCore, lookup), "calli   %a2");
        assert_eq!(annotate_target("call    #0x90000000", Isa::TriCore, lookup), "call    #0x90000000");
    }

    /// The test ELF (AURIX_TEST_ELF) and the range of a function in it: AURIX_TEST_FUNCTION,
    /// else `main`.
    fn real_elf_function_range() -> Option<(aurix_core::image::Image, symbols::Symbols, u32, u32)> {
        let path = std::path::PathBuf::from(std::env::var_os("AURIX_TEST_ELF")?);
        if !path.exists() {
            return None;
        }
        let img = aurix_core::image::Image::load(&path, 0, None).unwrap();
        let syms = symbols::Symbols::load(&path).unwrap();
        let name = std::env::var("AURIX_TEST_FUNCTION").unwrap_or_else(|_| "main".into());
        let f = *syms.functions_named(&name).first().unwrap_or_else(|| panic!("no function {name} in the test ELF"));
        let (lo, hi) = (f.low, f.high);
        Some((img, syms, lo, hi))
    }

    /// Direct calls in the test function are annotated with the names of their targets.
    #[test]
    fn real_elf_call_names() {
        let Some((img, syms, lo, hi)) = real_elf_function_range() else { return };
        let s = img.sections.iter().find(|s| s.addr <= lo && s.end() >= hi as u64).unwrap();
        let code = &s.data[(lo - s.addr) as usize..(hi - s.addr) as usize];
        let function_at = |a: u32| syms.function_at(a).map(|f| (f.name.clone(), a - f.low));
        let calls: Vec<String> = disassemble_bytes(&capstone(Isa::TriCore).unwrap(), Isa::TriCore, code, lo)
            .into_iter()
            .map(|(_, _, t)| annotate_target(&t, Isa::TriCore, &function_at))
            .filter(|t| t.starts_with("call") && t.contains("#0x"))
            .collect();
        eprintln!("{calls:#?}");
        assert!(!calls.is_empty(), "no direct calls in the test function");
        for t in &calls {
            let hex: String = t.split("#0x").nth(1).unwrap().chars().take_while(char::is_ascii_hexdigit).collect();
            if let Some(f) = syms.function_at(u32::from_str_radix(&hex, 16).unwrap()) {
                assert!(t.contains(&format!("<{}>", f.name)) || t.contains(&format!("<{}+", f.name)), "{t}");
            }
        }
    }

    /// Prints the test function for comparison with tricore-elf-objdump.
    #[test]
    fn real_elf_function() {
        let Some((img, _, lo, hi)) = real_elf_function_range() else { return };
        let s = img.sections.iter().find(|s| s.addr <= lo && s.end() >= hi as u64).unwrap();
        let code = &s.data[(lo - s.addr) as usize..(hi - s.addr) as usize];
        let lines = disassemble_bytes(&capstone(Isa::TriCore).unwrap(), Isa::TriCore, code, lo);
        for (a, n, t) in &lines {
            eprintln!("{a:08x} [{n}] {t}");
        }
        assert!(lines.iter().all(|(_, _, t)| !t.starts_with('.')), "undecoded instructions: {lines:?}");
    }
}

#[cfg(test)]
mod hsm_tests {
    use super::*;

    /// GHS Thumb-2 code of the HSM ELF (AURIX_TEST_HSM_ELF): decodes, and the prologue decoder
    /// finds the saved return address of the functions that call others.
    #[test]
    fn real_hsm_elf_thumb_code_and_prologues() {
        let Some(path) = std::env::var_os("AURIX_TEST_HSM_ELF").map(std::path::PathBuf::from) else { return };
        if !path.exists() {
            return;
        }
        let path = path.as_path();
        let img = aurix_core::image::Image::load(path, 0, None).unwrap();
        let syms = symbols::Symbols::load(path).unwrap();
        let cs = capstone(Isa::Thumb).unwrap();
        let (mut calls, mut with_lr, mut undecoded) = (0, 0, 0);
        assert!(syms.functions.iter().all(|f| f.high > f.low));
        for f in syms.functions.iter().filter(|f| f.high - f.low >= 16).take(400) {
            let lo = f.low & !1;
            let Some(s) = img.sections.iter().find(|s| s.addr <= lo && s.end() >= f.high as u64) else { continue };
            let code = &s.data[(lo - s.addr) as usize..(f.high - s.addr) as usize];
            let lines = disassemble_bytes(&cs, Isa::Thumb, code, lo);
            undecoded += lines.iter().filter(|(_, _, t)| t.starts_with('.')).count();
            if lines.iter().any(|(_, _, t)| t.starts_with("bl ")) {
                calls += 1;
                let p = aurix_core::unwind_arm::analyze_prologue(code, lo, f.high);
                if p.saved.iter().any(|(r, _)| *r == aurix_core::unwind_arm::R_LR) {
                    with_lr += 1;
                } else {
                    eprintln!("{} (no LR save found):", f.name);
                    for (a, n, t) in lines.iter().take(6) {
                        eprintln!("  {a:08x} [{n}] {t}");
                    }
                }
            }
        }
        eprintln!("functions with calls: {calls}, LR save found: {with_lr}, undecoded instructions: {undecoded}");
        assert!(calls > 20);
        assert!(with_lr * 10 >= calls * 9, "LR save found in only {with_lr} of {calls} calling functions");
    }
}
