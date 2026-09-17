//! Assembler for GTM MCS sources in the GHS `asmcs` syntax, used as the symbol source of MCS
//! channels: the assembler produces no debug information, so the debugger assembles the source
//! itself and gets the address of every source line and label.
//!
//! Supported: `.include "file"`, `.define NAME expr`, `.register NAME expr`, `.org expr`,
//! `label:`, `.var expr`, all MCS24-3 instructions, `;` and `#` comments, operands separated by
//! commas or blanks, C-like integer expressions with `**`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use isa::mcs::{self, Field};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AsmLine {
    pub addr: u32,
    pub file: u32,
    pub line: u32,
    pub code: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AsmLabel {
    pub name: String,
    pub addr: u32,
    /// Label of an instruction (otherwise of data or an empty location).
    pub code: bool,
    pub file: u32,
    pub line: u32,
}

#[derive(Debug, Default)]
pub struct McsProgram {
    pub files: Vec<PathBuf>,
    /// Memory contents by address (instructions and data words).
    pub words: BTreeMap<u32, u32>,
    /// Source lines that emit a word, sorted by address.
    pub lines: Vec<AsmLine>,
    /// Sorted by address.
    pub labels: Vec<AsmLabel>,
}

impl McsProgram {
    /// Symbols of the program: code labels as functions (up to the next label), data labels as
    /// globals, instruction lines as the line table.
    pub fn symbols(&self, path: &Path) -> crate::Symbols {
        let code_end = |from: u32| {
            let mut a = from;
            while self.lines.iter().any(|l| l.code && l.addr == a) {
                a += 4;
            }
            a
        };
        let mut functions = Vec::new();
        let mut globals = Vec::new();
        for (i, l) in self.labels.iter().enumerate() {
            if l.code {
                let next = self.labels[i + 1..].iter().map(|n| n.addr).find(|&a| a > l.addr).unwrap_or(u32::MAX);
                functions.push((l.name.clone(), l.addr, next.min(code_end(l.addr)).max(l.addr + 4)));
            } else {
                globals.push((l.name.clone(), l.addr));
            }
        }
        let mut rows = Vec::new();
        for l in self.lines.iter().filter(|l| l.code) {
            rows.push((l.addr, Some(l.file), l.line));
            if !self.lines.iter().any(|n| n.code && n.addr == l.addr + 4) {
                rows.push((l.addr + 4, None, 0));
            }
        }
        let files = self.files.iter().map(|f| f.to_string_lossy().into_owned()).collect();
        crate::Symbols::from_tables(path, functions, globals, files, rows)
    }

    /// Memory image from address 0 up to the last emitted word (gaps are zero).
    pub fn image(&self) -> Vec<u32> {
        let end = self.words.keys().next_back().map_or(0, |a| a / 4 + 1);
        (0..end).map(|i| self.words.get(&(i * 4)).copied().unwrap_or(0)).collect()
    }
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(i64),
    Ident(String),
    Op(&'static str),
    LParen,
    RParen,
    Comma,
}

fn tokenize(s: &str) -> Result<Vec<Tok>, String> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let t = s[start..i].replace('_', "");
            let v = if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                i64::from_str_radix(h, 16)
            } else if let Some(bin) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
                i64::from_str_radix(bin, 2)
            } else {
                t.parse()
            };
            out.push(Tok::Num(v.map_err(|_| format!("bad number '{}'", &s[start..i]))?));
        } else if c.is_ascii_alphabetic() || c == b'_' || c == b'.' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'.') {
                i += 1;
            }
            out.push(Tok::Ident(s[start..i].to_string()));
        } else {
            let two = s.get(i..i + 2).unwrap_or("");
            let op = ["**", "<<", ">>"].into_iter().find(|o| *o == two);
            if let Some(o) = op {
                out.push(Tok::Op(o));
                i += 2;
                continue;
            }
            out.push(match c {
                b'(' => Tok::LParen,
                b')' => Tok::RParen,
                b',' => Tok::Comma,
                b'+' => Tok::Op("+"),
                b'-' => Tok::Op("-"),
                b'*' => Tok::Op("*"),
                b'/' => Tok::Op("/"),
                b'%' => Tok::Op("%"),
                b'&' => Tok::Op("&"),
                b'|' => Tok::Op("|"),
                b'^' => Tok::Op("^"),
                b'~' => Tok::Op("~"),
                _ => return Err(format!("unexpected character '{}'", c as char)),
            });
            i += 1;
        }
    }
    Ok(out)
}

#[derive(Clone, Debug)]
enum Expr {
    Num(i64),
    Ident(String),
    Unary(&'static str, Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
}

fn precedence(op: &str) -> Option<u8> {
    Some(match op {
        "|" => 1,
        "^" => 2,
        "&" => 3,
        "<<" | ">>" => 4,
        "+" | "-" => 5,
        "*" | "/" | "%" => 6,
        "**" => 7,
        _ => return None,
    })
}

struct Parser<'a> {
    toks: &'a [Tok],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn primary(&mut self) -> Result<Expr, String> {
        let t = self.peek().cloned().ok_or("missing operand")?;
        self.pos += 1;
        match t {
            Tok::Num(v) => Ok(Expr::Num(v)),
            Tok::Ident(n) => Ok(Expr::Ident(n)),
            Tok::Op(o @ ("-" | "~" | "+")) => Ok(Expr::Unary(o, Box::new(self.primary()?))),
            Tok::LParen => {
                let e = self.expr(0)?;
                if self.peek() != Some(&Tok::RParen) {
                    return Err("missing ')'".into());
                }
                self.pos += 1;
                Ok(e)
            }
            other => Err(format!("unexpected {other:?}")),
        }
    }

    fn expr(&mut self, min: u8) -> Result<Expr, String> {
        let mut lhs = self.primary()?;
        while let Some(Tok::Op(o)) = self.peek().cloned() {
            let Some(p) = precedence(o) else { break };
            if p < min {
                break;
            }
            self.pos += 1;
            // "**" is right associative, the others left associative.
            let rhs = self.expr(if o == "**" { p } else { p + 1 })?;
            lhs = Expr::Binary(o, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    /// Operands separated by commas or, where no operator joins them, by blanks.
    fn operands(mut self) -> Result<Vec<Expr>, String> {
        let mut out = Vec::new();
        while self.peek().is_some() {
            out.push(self.expr(0)?);
            if self.peek() == Some(&Tok::Comma) {
                self.pos += 1;
            }
        }
        Ok(out)
    }
}

fn parse_operands(s: &str) -> Result<Vec<Expr>, String> {
    let toks = tokenize(s)?;
    Parser { toks: &toks, pos: 0 }.operands()
}

fn parse_expr(s: &str) -> Result<Expr, String> {
    let toks = tokenize(s)?;
    let mut p = Parser { toks: &toks, pos: 0 };
    let e = p.expr(0)?;
    if p.pos != toks.len() {
        return Err(format!("unexpected text after expression '{s}'"));
    }
    Ok(e)
}

enum Stmt {
    Insn(&'static mcs::Op, Vec<Expr>),
    Var(Vec<Expr>),
}

struct Item {
    addr: u32,
    file: u32,
    line: u32,
    stmt: Stmt,
}

#[derive(Default)]
struct Asm {
    files: Vec<PathBuf>,
    include_dirs: Vec<PathBuf>,
    defines: HashMap<String, Expr>,
    registers: HashMap<String, Expr>,
    labels: HashMap<String, u32>,
    label_list: Vec<AsmLabel>,
    items: Vec<Item>,
    addr: u32,
    /// Labels waiting for the next emitted word (to classify them as code or data).
    pending: Vec<usize>,
}

fn key(name: &str) -> String {
    name.to_ascii_uppercase()
}

impl Asm {
    fn value(&self, e: &Expr, depth: u32) -> Result<i64, String> {
        if depth > 64 {
            return Err("recursive definition".into());
        }
        Ok(match e {
            Expr::Num(v) => *v,
            Expr::Ident(n) => {
                if let Some(d) = self.defines.get(&key(n)) {
                    self.value(d, depth + 1)?
                } else if let Some(a) = self.labels.get(&key(n)) {
                    *a as i64
                } else {
                    return Err(format!("undefined symbol '{n}'"));
                }
            }
            Expr::Unary(o, a) => {
                let v = self.value(a, depth + 1)?;
                match *o {
                    "-" => -v,
                    "~" => !v,
                    _ => v,
                }
            }
            Expr::Binary(o, a, b) => {
                let (x, y) = (self.value(a, depth + 1)?, self.value(b, depth + 1)?);
                match *o {
                    "+" => x.wrapping_add(y),
                    "-" => x.wrapping_sub(y),
                    "*" => x.wrapping_mul(y),
                    "/" | "%" if y == 0 => return Err("division by zero".into()),
                    "/" => x / y,
                    "%" => x % y,
                    "&" => x & y,
                    "|" => x | y,
                    "^" => x ^ y,
                    "<<" => x.wrapping_shl(y as u32),
                    ">>" => x.wrapping_shr(y as u32),
                    "**" => x.wrapping_pow(y.clamp(0, 63) as u32),
                    _ => unreachable!(),
                }
            }
        })
    }

    /// Register operand: a register name, a `.register` alias or a define naming one of them.
    fn register(&self, e: &Expr, depth: u32) -> Result<i64, String> {
        if depth > 64 {
            return Err("recursive definition".into());
        }
        if let Expr::Ident(n) = e {
            if let Some(r) = mcs::reg_index(n) {
                return Ok(r as i64);
            }
            if let Some(r) = self.registers.get(&key(n)) {
                return self.register(r, depth + 1);
            }
            if let Some(d) = self.defines.get(&key(n)) {
                return self.register(d, depth + 1);
            }
        }
        self.value(e, depth + 1)
    }

    fn include(&mut self, from: &Path, name: &str) -> Result<PathBuf, String> {
        let dir = from.parent().unwrap_or(Path::new(""));
        let mut candidates = vec![dir.join(name)];
        candidates.extend(self.include_dirs.iter().map(|d| d.join(name)));
        // asmcs is called with -I..\inc in the usual MCS project layout.
        candidates.push(dir.join("..").join("inc").join(name));
        candidates
            .into_iter()
            .find(|p| p.is_file())
            .ok_or_else(|| format!("include file '{name}' not found"))
    }

    fn file(&mut self, path: &Path, depth: u32) -> Result<(), String> {
        if depth > 16 {
            return Err("includes nested too deeply".into());
        }
        let text = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let text = String::from_utf8_lossy(&text).into_owned();
        let file = self.files.len() as u32;
        self.files.push(path.to_path_buf());
        for (i, raw) in text.lines().enumerate() {
            let line = i as u32 + 1;
            self.line(path, file, line, raw, depth)
                .map_err(|e| format!("{}:{line}: {e}", path.display()))?;
        }
        Ok(())
    }

    fn line(&mut self, path: &Path, file: u32, line: u32, raw: &str, depth: u32) -> Result<(), String> {
        let mut text = raw;
        // Comments; ';' and '#' do not occur in include names.
        if let Some(p) = text.find([';', '#']) {
            text = &text[..p];
        }
        let mut text = text.trim();
        while let Some((name, rest)) = split_label(text) {
            let k = key(name);
            if self.labels.insert(k, self.addr).is_some() {
                return Err(format!("label '{name}' defined twice"));
            }
            self.pending.push(self.label_list.len());
            self.label_list.push(AsmLabel { name: name.to_string(), addr: self.addr, code: false, file, line });
            text = rest.trim();
        }
        if text.is_empty() {
            return Ok(());
        }
        let (word, rest) = text.split_once(|c: char| c.is_whitespace()).unwrap_or((text, ""));
        let rest = rest.trim();
        match word.to_ascii_lowercase().as_str() {
            ".include" => {
                let name = rest.trim_matches('"');
                let inc = self.include(path, name)?;
                self.file(&inc, depth + 1)
            }
            ".define" => {
                let (name, e) = rest.split_once(|c: char| c.is_whitespace()).ok_or(".define needs a value")?;
                self.defines.insert(key(name), parse_expr(e.trim())?);
                Ok(())
            }
            ".register" => {
                let (name, e) = rest.split_once(|c: char| c.is_whitespace()).ok_or(".register needs a value")?;
                self.registers.insert(key(name), parse_expr(e.trim())?);
                Ok(())
            }
            ".org" => {
                let v = self.value(&parse_expr(rest)?, 0)?;
                if !(0..=0xFFFC).contains(&v) || v & 3 != 0 {
                    return Err(format!(".org {v:#x} is not a word address"));
                }
                self.addr = v as u32;
                // A label before an .org marks reserved space (e.g. a stack), not code.
                self.pending.clear();
                Ok(())
            }
            ".var" => {
                let vals = parse_operands(rest)?;
                let n = vals.len().max(1) as u32;
                self.emit(file, line, Stmt::Var(vals), false, n);
                Ok(())
            }
            other if other.starts_with('.') => Err(format!("unsupported directive '{word}'")),
            _ => {
                let op = mcs::find_op(word).ok_or_else(|| format!("unknown instruction '{word}'"))?;
                let ops = parse_operands(rest)?;
                self.emit(file, line, Stmt::Insn(op, ops), true, 1);
                Ok(())
            }
        }
    }

    fn emit(&mut self, file: u32, line: u32, stmt: Stmt, code: bool, words: u32) {
        for i in self.pending.drain(..) {
            self.label_list[i].code = code;
        }
        self.items.push(Item { addr: self.addr, file, line, stmt });
        self.addr += 4 * words;
    }

    fn encode(&self, op: &mcs::Op, args: &[Expr]) -> Result<u32, String> {
        let vals = args
            .iter()
            .zip(op.args)
            .map(|(e, f)| match f {
                Field::Reg4(_) | Field::Reg5(..) | Field::Reg3(_) | Field::Areg(_) => self.register(e, 0),
                _ => self.value(e, 0),
            })
            .collect::<Result<Vec<_>, _>>()?;
        if args.len() > op.args.len() {
            return Err(format!("{} takes {} operands, got {}", op.name, op.args.len(), args.len()));
        }
        mcs::encode(op, &vals)
    }
}

/// `name:` at the start of the text: the label and the rest of the line.
fn split_label(text: &str) -> Option<(&str, &str)> {
    let end = text.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))?;
    if end == 0 || text.as_bytes()[0].is_ascii_digit() || !text[end..].starts_with(':') {
        return None;
    }
    Some((&text[..end], &text[end + 1..]))
}

/// Assembles an MCS source file.
pub fn assemble(path: &Path, include_dirs: &[PathBuf]) -> Result<McsProgram, String> {
    let mut a = Asm { include_dirs: include_dirs.to_vec(), ..Default::default() };
    a.file(path, 0)?;
    let mut prog = McsProgram::default();
    for it in &a.items {
        let at = |e: String| format!("{}:{}: {e}", a.files[it.file as usize].display(), it.line);
        match &it.stmt {
            Stmt::Insn(op, args) => {
                prog.words.insert(it.addr, a.encode(op, args).map_err(at)?);
            }
            Stmt::Var(vals) => {
                if vals.is_empty() {
                    prog.words.insert(it.addr, 0);
                }
                for (i, e) in vals.iter().enumerate() {
                    let v = a.value(e, 0).map_err(at)?;
                    prog.words.insert(it.addr + 4 * i as u32, v as u32);
                }
            }
        }
        prog.lines.push(AsmLine { addr: it.addr, file: it.file, line: it.line, code: matches!(it.stmt, Stmt::Insn(..)) });
    }
    prog.lines.sort_by_key(|l| l.addr);
    a.label_list.sort_by_key(|l| l.addr);
    prog.labels = a.label_list;
    prog.files = a.files;
    Ok(prog)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, text).unwrap();
        p
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mcs_asm_{name}_{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn small_program() {
        let d = tmp("small");
        write(&d, "defs.inc", ".define Z 5\n.define MSK 2**3 | 1\n");
        let src = write(
            &d,
            "t.asm",
            ".include \"defs.inc\"\n.org 0\njmp start\n.register CNT 3\n.org 0x10\nvar1: .var 7\nstack:\n.org (stack+8)\n\
             .org 0x20\nstart:\n  movl R7 (stack-4) # sp\n  movl CNT, ~MSK\nloop: jbs STA, Z, loop ; wait\n  call start\n  ret\n",
        );
        let p = assemble(&src, &[]).unwrap();
        let img = p.image();
        assert_eq!(img[0], 0xE000_0020);
        assert_eq!(img[4], 7);
        assert_eq!(img[8], 0x1700_0010);
        assert_eq!(img[9], 0x13FF_FFF6);
        assert_eq!(img[10], 0xE851_0028);
        assert_eq!(img[11], 0xE003_0020);
        assert_eq!(img[12], 0xE004_0000);
        let syms = p.symbols(&src);
        assert_eq!(syms.function_at(0x2C).map(|f| (f.name.as_str(), f.low, f.high)), Some(("loop", 0x28, 0x34)));
        assert_eq!(syms.line_at(0x28).map(|l| (l.line, l.start, l.end)), Some((13, 0x28, 0x2C)));
        assert!(syms.line_at(0x34).is_none());
        assert_eq!(syms.resolve_line("t.asm", 12).unwrap(), (12, vec![0x24]));
        assert_eq!(syms.globals_named("var1")[0].addr, 0x10);
        let names: Vec<_> = p.labels.iter().map(|l| (l.name.as_str(), l.addr, l.code)).collect();
        assert_eq!(names, vec![("var1", 0x10, false), ("stack", 0x14, false), ("start", 0x20, true), ("loop", 0x28, true)]);
        let l = p.lines.iter().find(|l| l.addr == 0x28).unwrap();
        assert_eq!((l.line, l.code), (13, true));
    }

    /// Words of a GHS asmcs C output file (`const unsigned long mcs0_mem[500] = { 0x..., };`).
    fn gen_c_words(path: &Path) -> Vec<u32> {
        let text = std::fs::read_to_string(path).unwrap();
        let body = &text[text.find('{').unwrap()..];
        body.split(|c: char| c == ',' || c.is_whitespace() || c == '{' || c == '}' || c == ';')
            .filter_map(|t| t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")))
            .map(|h| u32::from_str_radix(h, 16).unwrap())
            .collect()
    }

    #[test]
    fn real_mcs_sources_match_the_ghs_output() {
        let base = Path::new("C:/workspace/app/bsw/AutosarAbstractionLayers/MCAL/TriMCS");
        if !base.is_dir() {
            eprintln!("skipped: {} not found", base.display());
            return;
        }
        for n in ["MCS0", "MCS1"] {
            let prog = assemble(&base.join(format!("src/{n}.asm")), &[base.join("inc")]).unwrap();
            let expect = gen_c_words(&base.join(format!("gen/{n}_Gen.c")));
            let got = prog.image();
            let mismatches: Vec<String> = expect
                .iter()
                .enumerate()
                .filter(|(i, w)| got.get(*i).copied().unwrap_or(0) != **w)
                .take(10)
                .map(|(i, w)| {
                    let line = prog.lines.iter().find(|l| l.addr == i as u32 * 4);
                    format!(
                        "{n} @{:#x}: expected {w:#010x} got {:#010x} (line {:?}: {})",
                        i * 4,
                        got.get(i).copied().unwrap_or(0),
                        line.map(|l| l.line),
                        isa::mcs::format(*w, &|_| None)
                    )
                })
                .collect();
            assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
            assert!(got.len() <= expect.len() || got[expect.len()..].iter().all(|w| *w == 0));
            // The disassembler decodes every instruction back to the same word.
            for l in prog.lines.iter().filter(|l| l.code) {
                let w = prog.words[&l.addr];
                let op = isa::mcs::decode(w).unwrap_or_else(|| panic!("{n} @{:#x}: {w:#010x} not decoded", l.addr));
                assert_eq!(isa::mcs::encode(op, &isa::mcs::operands(op, w)).unwrap(), w, "{n} @{:#x}", l.addr);
            }
            assert!(prog.labels.iter().any(|l| l.code && l.name == "task0_init" || n == "MCS1"));
        }
    }
}
