//! SDCC debug files (`.cdb`), as HighTec's `scr-cc` writes them for the TC3xx standby controller
//! (SCR, an XC800/8051): functions, globals with their types, and the line table.
//!
//! The format is line based (SDCC manual, "CDB file format"):
//!
//! ```text
//! M:scr_main                                         module
//! F:G$main$0_0$0({2}DF,SV:S),C,0,0,0,0,0             function: scope, name, type, address space
//! S:G$debounce$0_0$0({1}SC:U),E,0,0                  symbol: scope, name, type, address space
//! L:G$main$0$0:EE                                    linker: address of a symbol
//! L:XG$main$0$0:F1                                   linker: the function's exit instruction
//! L:C$scr_main.c$128$1_0$1:EE                        linker: a C line starts here
//! L:A$scr_main$423:0                                 linker: an assembler line (not used)
//! ```
//!
//! Scopes are `G` (global), `F<module>` (file static) and `L<module>.<function>` (local).
//! Addresses are made SCR virtual addresses (TC3xx UM Table 558), as the debugger reads them:
//! code and code constants as they are, external RAM (XDATA) from 0x10000, internal RAM from
//! 0x20000, standard SFRs from 0x40000.
//!
//! Locals become [`TableLocal`]s of their function: in registers (`R,0,0,[r6,r7]`, least
//! significant first, by [`isa::xc800::dwarf_register`]), in memory (overlaid locals and
//! parameters with an address), or optimized out (`R,0,0,[]`). SDCC gives no live ranges: a
//! register local is shown for its whole function, also where the register holds something
//! else. Locals on the stack (reentrant functions) are left out.
//!
//! A function ends after its exit instruction; its length comes from the program image, the
//! Intel HEX file next to the `.cdb` (same name, `.ihx`). Without one a function ends where the
//! next begins.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::scope::{LocDesc, TableLocal};
use crate::types::{TypeClass, TypeId};
use crate::Symbols;

/// Start of each space in the SCR's virtual address map.
const XDATA: u32 = 0x1_0000;
const IRAM: u32 = 0x2_0000;
const SFR: u32 = 0x4_0000;

/// One `F:`/`S:` record.
#[derive(Debug, Clone, PartialEq)]
struct Sym {
    scope: String,
    name: String,
    ty: Option<TypeId>,
    space: char,
    /// On the stack (reentrant function).
    on_stack: bool,
    /// Registers of a register local, as named (`[r6,r7]`); `None` when no list is given.
    regs: Option<Vec<String>>,
}

/// The fields after the type: `,space,onstack,offset[,[regs]]`.
fn parse_tail(rest: &str) -> (bool, Option<Vec<String>>) {
    let after = rest.rfind(')').map_or("", |i| &rest[i + 1..]);
    let on_stack = after.split(',').nth(2).is_some_and(|f| f.trim() == "1");
    let regs = match (after.find('['), after.rfind(']')) {
        (Some(a), Some(b)) if b > a => Some(after[a + 1..b].split(',').map(str::trim).filter(|r| !r.is_empty()).map(String::from).collect()),
        _ => None,
    };
    (on_stack, regs)
}

/// Splits `G$name$...` / `Fmod$name$...` / `Lmod.fn$name$...` into (scope, name, rest).
fn scoped(s: &str) -> Option<(&str, &str, &str)> {
    let (scope, rest) = s.split_once('$')?;
    let (name, rest) = rest.split_once('$')?;
    Some((scope, name, rest))
}

/// `({size}chain),space,...` -> type and space. The chain is comma separated, outermost first:
/// `DA14d` array of 14, `DG`/`DC`/`DX`/`DD`/`DI`/`DP` pointers, `DF` function, then `SC`/`SS`/
/// `SI`/`SL` integers, `SF` float, `SV` void, `ST<name>` struct, `SX` sbit, `SB...` bit field,
/// each with `:U` or `:S`.
fn parse_type(rest: &str) -> Option<(Option<TypeId>, char)> {
    let open = rest.find('(')?;
    let close = rest[open..].find(')')? + open;
    let inner = &rest[open + 1..close];
    let space = rest[close + 1..].trim_start_matches(',').chars().next()?;
    let (size, chain) = inner.strip_prefix('{')?.split_once('}')?;
    let size: u32 = size.parse().ok()?;
    let (chain, sign) = match chain.rsplit_once(':') {
        Some((c, s)) => (c, s),
        None => (chain, "U"),
    };
    let parts: Vec<&str> = chain.split(',').collect();
    Some((chain_type(&parts, size, sign == "S"), space))
}

fn chain_type(parts: &[&str], size: u32, signed: bool) -> Option<TypeId> {
    let (first, rest) = parts.split_first()?;
    let class = if signed { TypeClass::Integer } else { TypeClass::Cardinal };
    match first.get(..2)? {
        "DA" => {
            let n: u32 = first[2..].trim_end_matches('d').parse().ok()?;
            let elem = chain_type(rest, size.checked_div(n).unwrap_or(0), signed)?;
            Some(TypeId::ArrayOf(Box::new(elem), n))
        }
        // Pointers are shown as their value: dereferencing needs the 8051 memory spaces.
        "DG" => Some(TypeId::Base(TypeClass::Cardinal, 3)),
        "DC" | "DX" | "DP" => Some(TypeId::Base(TypeClass::Cardinal, 2)),
        "DD" | "DI" => Some(TypeId::Base(TypeClass::Cardinal, 1)),
        "DF" => None,
        "SC" => Some(TypeId::Base(class, 1)),
        "SS" | "SI" => Some(TypeId::Base(class, 2)),
        "SL" => Some(TypeId::Base(class, 4)),
        "SF" => Some(TypeId::Base(TypeClass::Real, 4)),
        "SX" | "SB" => Some(TypeId::Base(TypeClass::Cardinal, 1)),
        _ if size > 0 && size <= 8 => Some(TypeId::Base(TypeClass::Cardinal, size as u8)),
        _ => None,
    }
}

/// The SCR virtual address of `addr` in SDCC address space `space`; `None` for spaces the
/// debugger cannot address (registers, bits, paged SFRs).
fn virtual_address(space: char, addr: u32) -> Option<u32> {
    match space {
        'C' | 'D' => Some(addr),
        'A' | 'F' | 'P' => Some(XDATA + addr),
        'B' | 'E' | 'G' | 'H' => Some(IRAM + addr),
        'I' if addr >= 0x80 => Some(SFR | (addr & 0x7F)),
        _ => None,
    }
}

/// Reads an Intel HEX file into (address, byte) pairs.
pub fn read_ihx(text: &str) -> HashMap<u32, u8> {
    let mut mem = HashMap::new();
    let mut base = 0u32;
    for line in text.lines() {
        let Some(hex) = line.trim().strip_prefix(':') else { continue };
        let bytes: Option<Vec<u8>> = (0..hex.len() / 2).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()).collect();
        let Some(b) = bytes else { continue };
        if b.len() < 5 || b.len() < 5 + b[0] as usize {
            continue;
        }
        let (n, addr, kind) = (b[0] as usize, u16::from_be_bytes([b[1], b[2]]) as u32, b[3]);
        match kind {
            0 => {
                for (i, &x) in b[4..4 + n].iter().enumerate() {
                    mem.insert(base + addr + i as u32, x);
                }
            }
            2 if n == 2 => base = (u16::from_be_bytes([b[4], b[5]]) as u32) << 4,
            4 if n == 2 => base = (u16::from_be_bytes([b[4], b[5]]) as u32) << 16,
            _ => {}
        }
    }
    mem
}

/// Where a source file named in the `.cdb` (without directory) is: next to it, or in the
/// `src`, `inc` and `api` folders beside its folder (the SCR project's layout).
fn find_source(dir: &Path, name: &str) -> String {
    let candidates = [dir.to_path_buf(), dir.join("../src"), dir.join("../inc"), dir.join("../api"), dir.join("..")];
    for c in candidates {
        let p = c.join(name);
        if p.is_file() {
            return std::fs::canonicalize(&p)
                .map(|p| p.to_string_lossy().trim_start_matches(r"\\?\").to_string())
                .unwrap_or_else(|_| p.to_string_lossy().into_owned());
        }
    }
    name.to_string()
}

/// Loads a `.cdb` file (and the `.ihx` beside it, if any).
pub fn load(path: &Path) -> Result<Symbols, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let ihx = std::fs::read_to_string(path.with_extension("ihx")).ok();
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
    Ok(parse(path, &text, ihx.as_deref(), &|name| find_source(&dir, name)))
}

/// Builds symbols from `.cdb` text; `ihx` is the program image, `source` names a source file.
pub fn parse(path: &Path, text: &str, ihx: Option<&str>, source: &dyn Fn(&str) -> String) -> Symbols {
    let t0 = std::time::Instant::now();
    let mut funcs: Vec<Sym> = Vec::new();
    let mut vars: Vec<Sym> = Vec::new();
    let mut locals: Vec<Sym> = Vec::new();
    // (scope, name) -> address, and the exit addresses of functions.
    let mut addrs: HashMap<(String, String), u32> = HashMap::new();
    let mut exits: HashMap<(String, String), u32> = HashMap::new();
    let mut files: Vec<String> = Vec::new();
    let mut file_ids: HashMap<String, u32> = HashMap::new();
    // address -> (file, line): a later line at the same address wins (empty lines before a
    // statement share its address).
    let mut lines: HashMap<u32, (u32, u32)> = HashMap::new();

    for raw in text.lines() {
        let line = raw.trim_end();
        let Some((kind, rest)) = line.split_once(':') else { continue };
        match kind {
            "F" | "S" => {
                let Some((scope, name, tail)) = scoped(rest) else { continue };
                let Some((ty, space)) = parse_type(tail) else { continue };
                let (on_stack, regs) = parse_tail(tail);
                let s = Sym { scope: scope.to_string(), name: name.to_string(), ty, space, on_stack, regs };
                if kind == "F" {
                    funcs.push(s);
                } else if scope.starts_with('L') {
                    locals.push(s);
                } else if space != 'C' {
                    vars.push(s);
                }
            }
            "L" => {
                let Some((sym, addr)) = rest.rsplit_once(':') else { continue };
                let Ok(addr) = u32::from_str_radix(addr, 16) else { continue };
                if let Some(c) = sym.strip_prefix("C$") {
                    // C$<file>$<line>$<level>$<block>
                    let mut it = c.split('$');
                    let (Some(file), Some(Ok(n))) = (it.next(), it.next().map(str::parse::<u32>)) else { continue };
                    let id = *file_ids.entry(file.to_string()).or_insert_with(|| {
                        files.push(source(file));
                        (files.len() - 1) as u32
                    });
                    lines.insert(addr, (id, n));
                } else if sym.starts_with("A$") {
                } else if let Some(x) = sym.strip_prefix('X') {
                    if let Some((scope, name, _)) = scoped(x) {
                        exits.insert((scope.to_string(), name.to_string()), addr);
                    }
                } else if let Some((scope, name, _)) = scoped(sym) {
                    addrs.insert((scope.to_string(), name.to_string()), addr);
                }
            }
            _ => {}
        }
    }

    let image = ihx.map(read_ihx);
    let insn_len = |a: u32| -> Option<u32> {
        let m = image.as_ref()?;
        Some(isa::xc800::length(*m.get(&a)?))
    };

    // Functions: start, exit instruction, end.
    let mut fl: Vec<(String, u32, Option<u32>)> = funcs
        .iter()
        .filter_map(|f| {
            let key = (f.scope.clone(), f.name.clone());
            Some((f.name.clone(), *addrs.get(&key)?, exits.get(&key).copied()))
        })
        .collect();
    fl.sort_by_key(|f| f.1);
    fl.dedup_by_key(|f| f.1);
    let mut functions = Vec::new();
    let mut fexits = HashMap::new();
    for (i, (name, low, exit)) in fl.iter().enumerate() {
        let next = fl.get(i + 1).map(|f| f.1);
        let after_exit = exit.map(|e| e + insn_len(e).unwrap_or(1));
        let high = match (after_exit, next) {
            (Some(a), Some(n)) => a.min(n),
            (Some(a), None) => a,
            (None, Some(n)) => n,
            (None, None) => low + 1,
        }
        .max(low + 1);
        functions.push((name.clone(), *low, high));
        if let Some(e) = exit {
            fexits.insert(*low, *e);
        }
    }

    // Globals with an address the debugger can read.
    let mut globals = Vec::new();
    let mut gtypes = HashMap::new();
    for v in &vars {
        let Some(&a) = addrs.get(&(v.scope.clone(), v.name.clone())) else { continue };
        let Some(va) = virtual_address(v.space, a) else { continue };
        globals.push((v.name.clone(), va));
        if let Some(t) = &v.ty {
            gtypes.insert((v.name.clone(), va), t.clone());
        }
    }

    // Line rows, ended at the end of each function.
    let mut rows: Vec<(u32, Option<u32>, u32)> = lines.iter().map(|(&a, &(f, n))| (a, Some(f), n)).collect();
    for &(_, _, high) in &functions {
        if !functions.iter().any(|f| f.1 == high) && !lines.contains_key(&high) {
            rows.push((high, None, 0));
        }
    }

    // Locals of each function, visible over its whole range.
    let mut table_locals = Vec::new();
    for l in &locals {
        let Some(fname) = l.scope.rsplit('.').next() else { continue };
        let Some(&(_, low, high)) = functions.iter().find(|f| f.0 == fname) else { continue };
        let Some(ty) = l.ty.clone() else { continue };
        let loc = match (&l.regs, addrs.get(&(l.scope.clone(), l.name.clone()))) {
            _ if l.on_stack => continue,
            (Some(regs), _) if l.space == 'R' => {
                let pieces: Option<Vec<(u16, u8)>> = regs.iter().map(|r| isa::xc800::dwarf_register(r).map(|n| (n, 1))).collect();
                match pieces {
                    Some(p) if !p.is_empty() => LocDesc::Registers(p),
                    Some(_) => LocDesc::OptimizedOut,
                    None => continue,
                }
            }
            (_, Some(&a)) => match virtual_address(l.space, a) {
                Some(va) => LocDesc::Memory(va),
                None => continue,
            },
            _ => LocDesc::OptimizedOut,
        };
        table_locals.push(TableLocal { low, high, name: l.name.clone(), ty, param: false, loc });
    }

    let mut syms = Symbols::from_tables(path, functions, globals, files, rows);
    syms.table_locals = table_locals;
    for f in &mut syms.functions {
        f.exit = fexits.get(&f.low).copied();
    }
    for g in &mut syms.globals {
        g.ty = gtypes.get(&(g.name.clone(), g.addr)).cloned();
    }
    syms.load_time = t0.elapsed();
    syms
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `scr_main.cdb` in the shape HighTec's scr-cc writes, with a matching image.
    const CDB: &str = "\
M:scr_main
F:G$main$0_0$0({2}DF,SV:S),C,0,0,0,0,0
F:G$process$0_0$0({2}DF,SV:S),Z,0,0,0,0,0
F:Fscr_main$helper$0_0$0({2}DF,SV:S),Z,0,0,0,0,0
S:G$main$0_0$0({2}DF,SC:U),C,0,0
S:G$debounce$0_0$0({1}SC:U),E,0,0
S:G$P00_PortInputValues$0_0$0({1}SC:U),F,0,0
S:G$clock_delay$0_0$0({28}DA14d,SI:U),D,0,0
S:G$SCR_A$0_0$0({1}SC:U),I,0,0
S:G$SCR_SYSCON0$0_0$0({1}SC:U),I,0,0
S:Fscr_main$counter$0_0$0({2}SI:S),E,0,0
S:Lscr_main.process$request$1_0$22({1}SC:U),R,0,0,[r7]
S:Lscr_main.process$wide$1_0$22({2}SI:U),R,0,0,[r6,r7]
S:Lscr_main.process$gone$1_0$22({1}SC:U),R,0,0,[]
S:Lscr_main.helper$Stuck$1_0$28({1}SC:U),E,0,0
S:Lscr_main.helper$deep$1_0$28({1}SC:U),B,1,-3
L:Lscr_main.helper$Stuck$1_0$28:10
L:G$main$0$0:EE
L:XG$main$0$0:F1
L:G$process$0$0:F3
L:XG$process$0$0:F8
L:Fscr_main$helper$0$0:F9
L:XFscr_main$helper$0$0:F9
L:G$debounce$0$0:2A
L:G$P00_PortInputValues$0$0:1FE8
L:G$clock_delay$0$0:566
L:G$SCR_A$0$0:E0
L:G$SCR_SYSCON0$0$0:0
L:Fscr_main$counter$0$0:30
L:C$scr_main.c$127$1_0$1:EE
L:C$scr_main.c$128$1_0$1:EE
L:C$scr_main.c$130$1_0$1:F1
L:C$scr_main.c$140$1_0$2:F3
L:C$scr_main.c$141$1_0$2:F6
L:C$scr_main.c$150$1_0$3:F9
L:A$scr_main$423:0
";
    /// 0xEE: LCALL 0x00F3; 0xF1: SJMP 0xEE; 0xF3: MOV A,#1; 0xF5: NOP; 0xF6: MOV A,#2; 0xF8: RET;
    /// 0xF9: RET.
    const IHX: &str = ":0C00EE001200F380FB7401007402222297\n:00000001FF\n";

    fn syms() -> Symbols {
        parse(Path::new("scr_main.cdb"), CDB, Some(IHX), &|n| format!("/src/{n}"))
    }

    #[test]
    fn functions_end_after_their_exit_instruction() {
        let s = syms();
        let f: Vec<(&str, u32, u32, Option<u32>)> = s.functions.iter().map(|f| (f.name.as_str(), f.low, f.high, f.exit)).collect();
        assert_eq!(f, vec![("main", 0xEE, 0xF3, Some(0xF1)), ("process", 0xF3, 0xF9, Some(0xF8)), ("helper", 0xF9, 0xFA, Some(0xF9))]);
        assert_eq!(s.function_at(0xF2).unwrap().name, "main");
        assert!(s.function_at(0xFA).is_none());
    }

    #[test]
    fn lines() {
        let s = syms();
        let li = s.line_at(0xEE).unwrap();
        assert_eq!((li.file.as_str(), li.line, li.start, li.end), ("/src/scr_main.c", 128, 0xEE, 0xF1), "the later line at one address");
        assert_eq!(s.line_at(0xF4).unwrap().line, 140);
        assert_eq!(s.line_at(0xF9).unwrap().line, 150);
        assert_eq!(s.resolve_line("scr_main.c", 141).unwrap(), (141, vec![0xF6]));
    }

    #[test]
    fn globals_get_scr_addresses_and_types() {
        let s = syms();
        let g = |n: &str| {
            let i = s.globals.iter().position(|g| g.name == n).unwrap_or_else(|| panic!("{n}"));
            (s.globals[i].addr, s.global_type(i))
        };
        assert_eq!(g("debounce"), (0x2002A, TypeId::Base(TypeClass::Cardinal, 1)));
        assert_eq!(g("P00_PortInputValues"), (0x11FE8, TypeId::Base(TypeClass::Cardinal, 1)));
        assert_eq!(g("counter"), (0x20030, TypeId::Base(TypeClass::Integer, 2)));
        assert_eq!(g("SCR_A"), (0x40060, TypeId::Base(TypeClass::Cardinal, 1)));
        let (a, t) = g("clock_delay");
        assert_eq!(a, 0x566);
        let ti = s.type_info(&t);
        assert_eq!((ti.class, ti.size, ti.length, ti.name.as_deref()), (TypeClass::Array, 28, Some(14), Some("unsigned short[14]")));
        assert!(!s.globals.iter().any(|g| g.name == "SCR_SYSCON0" || g.name == "request" || g.name == "main"), "paged SFR, register local, function");
    }

    #[test]
    fn locals_in_registers_memory_or_nowhere() {
        use crate::scope::{LocDesc, VarRef};
        let s = syms();
        let at = |pc: u32| s.locals_at(pc).iter().map(|l| l.name.clone()).collect::<Vec<_>>();
        assert_eq!(at(0xF4), vec!["request", "wide", "gone"], "process's, over its whole range");
        assert_eq!(at(0xF9), vec!["Stuck"], "helper's; the stack local is left out");
        assert!(at(0xEE).is_empty());
        let loc = |n: &str| s.table_locals.iter().find(|l| l.name == n).unwrap().loc.clone();
        assert_eq!(loc("request"), LocDesc::Registers(vec![(7, 1)]));
        assert_eq!(loc("wide"), LocDesc::Registers(vec![(6, 1), (7, 1)]));
        assert_eq!(loc("gone"), LocDesc::OptimizedOut);
        assert_eq!(loc("Stuck"), LocDesc::Memory(0x20010));
        let l = &s.locals_at(0xF4)[1];
        assert_eq!((s.local_name(l.var).as_deref(), s.local_type(l.var)), (Some("wide"), TypeId::Base(TypeClass::Cardinal, 2)));
        assert!(matches!(l.var, VarRef::Table(_)));
    }

    #[test]
    fn locals_evaluate_from_the_frame_registers() {
        use crate::expr::{Eval, Place, SymbolRef, Target};
        struct Frame(std::collections::HashMap<u16, u32>);
        impl Target for Frame {
            fn pc(&self) -> Option<u32> {
                Some(0xF4)
            }
            fn reg(&self, r: u16) -> Option<u32> {
                self.0.get(&r).copied()
            }
            fn read(&mut self, addr: u32, len: usize) -> Result<Vec<u8>, String> {
                Ok((0..len as u32).map(|i| (addr + i) as u8).collect())
            }
            fn symbol(&mut self, _: &str) -> Option<SymbolRef> {
                None
            }
        }
        let s = syms();
        // R6 = 0x34, R7 = 0x12 (bank registers of the frame, DWARF 6 and 7).
        let mut t = Frame([(6, 0x34), (7, 0x12)].into_iter().collect());
        let mut ev = Eval::new(&s, &mut t);
        let v = ev.evaluate("request").unwrap();
        assert_eq!(v.place, Place::Register(7));
        assert_eq!(ev.load(&v).unwrap(), vec![0x12]);
        let v = ev.evaluate("wide").unwrap();
        assert_eq!(ev.load(&v).unwrap(), vec![0x34, 0x12], "R6 the low byte");
        assert_eq!(ev.evaluate("wide + 1").map(|v| ev.load(&v).unwrap()).unwrap()[..2], [0x35, 0x12]);
        assert!(ev.evaluate("gone").is_err(), "optimized out");
    }

    #[test]
    fn the_real_scr_debug_file() {
        // SCR_CDB=<path to an SCR .cdb> (built by HighTec scr-cc), with a `main`.
        let Some(p) = std::env::var_os("SCR_CDB") else { return };
        let s = load(Path::new(&p)).unwrap();
        assert!(!s.functions.is_empty());
        let main = s.functions_named("main")[0];
        assert!(main.exit.is_some(), "main has an exit instruction");
        assert!(s.line_at(main.low).is_some(), "a source line at main");
        assert!(!s.globals.is_empty());
    }
}
