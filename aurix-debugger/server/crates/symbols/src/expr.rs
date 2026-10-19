//! C expressions over DWARF symbols: parsing, evaluation and assignment.
//!
//! Supported: identifiers (locals, globals, functions, enumerators),
//! `$d0`..`$a15`/`$sp`/`$ra`/`$pc`, `${SymbolID}`, integer/float/char
//! literals, `.` `->` `[]`, unary `- + ~ ! * &`, casts to named, builtin and
//! pointer types, `sizeof`, the conditional operator `?:`, and the binary C
//! operators except assignment.

use crate::scope::Loc;
use crate::types::{TypeClass, TypeId, TypeInfo};
use crate::{DieRef, Symbols, PC_REGISTER};

// --- Lexer --------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Ident(String),
    Int(u64),
    Float(f64),
    SymRef(String),
    P(&'static str),
}

const PUNCTS: &[&str] = &[
    "->", "<<", ">>", "<=", ">=", "==", "!=", "&&", "||", "+", "-", "*", "/", "%", "&", "|", "^", "~", "!", "<", ">", "?", ":", "(", ")", "[", "]", ".",
];

fn skip_suffix(b: &[u8], i: &mut usize, allowed: &[u8]) {
    while *i < b.len() && allowed.contains(&b[*i]) {
        *i += 1;
    }
}

fn lex(s: &str) -> Result<Vec<Tok>, String> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'$' && b.get(i + 1) == Some(&b'{') {
            let end = s[i + 2..].find('}').ok_or("unterminated ${...}")? + i + 2;
            out.push(Tok::SymRef(s[i + 2..end].to_string()));
            i = end + 1;
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' || c == b'$' {
            let st = i;
            i += 1;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            out.push(Tok::Ident(s[st..i].to_string()));
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && b.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            let radix = match (c, b.get(i + 1)) {
                (b'0', Some(b'x' | b'X')) => 16,
                (b'0', Some(b'b' | b'B')) => 2,
                _ => 10,
            };
            if radix != 10 {
                i += 2;
                let st = i;
                while i < b.len() && (b[i] as char).is_digit(radix) {
                    i += 1;
                }
                let v = u64::from_str_radix(&s[st..i], radix).map_err(|e| format!("invalid number: {e}"))?;
                skip_suffix(b, &mut i, b"uUlL");
                out.push(Tok::Int(v));
                continue;
            }
            let st = i;
            let mut float = false;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            if i < b.len() && b[i] == b'.' {
                float = true;
                i += 1;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
                float = true;
                i += 1;
                if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
                    i += 1;
                }
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
            }
            let text = &s[st..i];
            if float {
                out.push(Tok::Float(text.parse().map_err(|e| format!("invalid number '{text}': {e}"))?));
                skip_suffix(b, &mut i, b"fFlL");
            } else {
                out.push(Tok::Int(text.parse().map_err(|e| format!("invalid number '{text}': {e}"))?));
                skip_suffix(b, &mut i, b"uUlL");
            }
            continue;
        }
        if c == b'\'' {
            let (v, len) = match (b.get(i + 1), b.get(i + 2)) {
                (Some(b'\\'), Some(e)) => match e {
                    b'n' => (10, 2),
                    b't' => (9, 2),
                    b'r' => (13, 2),
                    b'0' => (0, 2),
                    b'x' => {
                        let st = i + 3;
                        let mut j = st;
                        while j < b.len() && b[j].is_ascii_hexdigit() {
                            j += 1;
                        }
                        (u64::from_str_radix(&s[st..j], 16).map_err(|_| "invalid character literal")?, j - i - 1)
                    }
                    other => (*other as u64, 2),
                },
                (Some(ch), _) => (*ch as u64, 1),
                _ => return Err("invalid character literal".into()),
            };
            if b.get(i + 1 + len) != Some(&b'\'') {
                return Err("invalid character literal".into());
            }
            out.push(Tok::Int(v));
            i += len + 2;
            continue;
        }
        if let Some(p) = PUNCTS.iter().find(|p| s[i..].starts_with(**p)) {
            out.push(Tok::P(p));
            i += p.len();
            continue;
        }
        return Err(format!("unexpected character '{}'", s[i..].chars().next().unwrap_or('?')));
    }
    Ok(out)
}

// --- Parser -------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Int(u64),
    Float(f64),
    Ident(String),
    SymRef(String),
    Member(Box<Expr>, String),
    Arrow(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    Unary(&'static str, Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    /// `cond ? then : otherwise`; only the taken branch is evaluated.
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    Cast(TypeId, Box<Expr>),
    Sizeof(Box<Expr>),
    SizeofType(TypeId),
}

/// Resolves a type name ("uint8", "struct foo", "unsigned int") to a type.
pub type TypeResolver<'a> = &'a dyn Fn(&str) -> Option<TypeId>;

struct Parser<'a> {
    toks: Vec<Tok>,
    pos: usize,
    types: TypeResolver<'a>,
}

fn precedence(op: &str) -> Option<u8> {
    Some(match op {
        "||" => 1,
        "&&" => 2,
        "|" => 3,
        "^" => 4,
        "&" => 5,
        "==" | "!=" => 6,
        "<" | ">" | "<=" | ">=" => 7,
        "<<" | ">>" => 8,
        "+" | "-" => 9,
        "*" | "/" | "%" => 10,
        _ => return None,
    })
}

const TYPE_WORDS: &[&str] = &["unsigned", "signed", "short", "long", "int", "char", "float", "double", "void", "_Bool"];

fn builtin_type(words: &[String], types: TypeResolver) -> Option<TypeId> {
    if words.is_empty() {
        return None;
    }
    if let Some(t) = types(&words.join(" ")) {
        return Some(t);
    }
    let has = |w: &str| words.iter().any(|x| x == w);
    let unsigned = has("unsigned");
    let class = if unsigned { TypeClass::Cardinal } else { TypeClass::Integer };
    Some(if has("void") {
        TypeId::Void
    } else if has("_Bool") {
        TypeId::Base(TypeClass::Cardinal, 1)
    } else if has("char") {
        TypeId::Base(class, 1)
    } else if has("short") {
        TypeId::Base(class, 2)
    } else if has("float") {
        TypeId::Base(TypeClass::Real, 4)
    } else if has("double") {
        TypeId::Base(TypeClass::Real, 8)
    } else if words.iter().filter(|w| *w == "long").count() >= 2 {
        TypeId::Base(class, 8)
    } else {
        TypeId::Base(class, 4)
    })
}

impl Parser<'_> {
    fn is_p(&self, p: &str) -> bool {
        matches!(self.toks.get(self.pos), Some(Tok::P(x)) if *x == p)
    }

    fn eat(&mut self, p: &str) -> bool {
        let yes = self.is_p(p);
        if yes {
            self.pos += 1;
        }
        yes
    }

    fn expect(&mut self, p: &str) -> Result<(), String> {
        if self.eat(p) {
            Ok(())
        } else {
            Err(format!("expected '{p}'"))
        }
    }

    fn ident(&mut self) -> Result<String, String> {
        match self.toks.get(self.pos) {
            Some(Tok::Ident(n)) => {
                self.pos += 1;
                Ok(n.clone())
            }
            _ => Err("expected a name".into()),
        }
    }

    /// `logical-or ? expression : conditional` - right associative, below every binary operator.
    fn conditional(&mut self) -> Result<Expr, String> {
        let cond = self.binary(0)?;
        if !self.eat("?") {
            return Ok(cond);
        }
        let then = self.conditional()?;
        self.expect(":")?;
        let otherwise = self.conditional()?;
        Ok(Expr::Cond(Box::new(cond), Box::new(then), Box::new(otherwise)))
    }

    fn binary(&mut self, min_prec: u8) -> Result<Expr, String> {
        let mut lhs = self.unary()?;
        while let Some(Tok::P(op)) = self.toks.get(self.pos) {
            let op: &'static str = op;
            let Some(prec) = precedence(op) else { break };
            if prec < min_prec {
                break;
            }
            self.pos += 1;
            let rhs = self.binary(prec + 1)?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> Result<Expr, String> {
        if let Some(Tok::P(op)) = self.toks.get(self.pos) {
            let op: &'static str = op;
            if matches!(op, "-" | "+" | "~" | "!" | "*" | "&") {
                self.pos += 1;
                return Ok(Expr::Unary(op, Box::new(self.unary()?)));
            }
            if op == "(" {
                if let Some(ty) = self.try_type_in_parens() {
                    return Ok(Expr::Cast(ty, Box::new(self.unary()?)));
                }
            }
        }
        if matches!(self.toks.get(self.pos), Some(Tok::Ident(s)) if s == "sizeof") {
            self.pos += 1;
            if self.is_p("(") {
                if let Some(ty) = self.try_type_in_parens() {
                    return Ok(Expr::SizeofType(ty));
                }
            }
            return Ok(Expr::Sizeof(Box::new(self.unary()?)));
        }
        self.postfix()
    }

    /// At '(': parses "(type-name)", or restores the position if it is not one.
    fn try_type_in_parens(&mut self) -> Option<TypeId> {
        let save = self.pos;
        self.pos += 1;
        if let Some(ty) = self.type_name() {
            if self.eat(")") {
                return Some(ty);
            }
        }
        self.pos = save;
        None
    }

    fn type_name(&mut self) -> Option<TypeId> {
        let mut words: Vec<String> = Vec::new();
        let mut named: Option<TypeId> = None;
        while let Some(Tok::Ident(w)) = self.toks.get(self.pos).cloned() {
            match w.as_str() {
                "const" | "volatile" => self.pos += 1,
                "struct" | "union" | "enum" if named.is_none() && words.is_empty() => {
                    self.pos += 1;
                    let n = self.ident().ok()?;
                    named = Some((self.types)(&format!("{w} {n}"))?);
                }
                _ if TYPE_WORDS.contains(&w.as_str()) && named.is_none() => {
                    self.pos += 1;
                    words.push(w);
                }
                _ if named.is_none() && words.is_empty() => {
                    named = Some((self.types)(&w)?);
                    self.pos += 1;
                }
                _ => break,
            }
        }
        let mut ty = match named {
            Some(t) => t,
            None => builtin_type(&words, self.types)?,
        };
        while self.eat("*") {
            ty = TypeId::PointerTo(Box::new(ty));
        }
        Some(ty)
    }

    fn postfix(&mut self) -> Result<Expr, String> {
        let mut e = self.primary()?;
        loop {
            if self.eat("[") {
                let i = self.conditional()?;
                self.expect("]")?;
                e = Expr::Index(Box::new(e), Box::new(i));
            } else if self.eat(".") {
                e = Expr::Member(Box::new(e), self.ident()?);
            } else if self.eat("->") {
                e = Expr::Arrow(Box::new(e), self.ident()?);
            } else {
                return Ok(e);
            }
        }
    }

    fn primary(&mut self) -> Result<Expr, String> {
        let tok = self.toks.get(self.pos).cloned();
        self.pos += 1;
        match tok {
            Some(Tok::Int(v)) => Ok(Expr::Int(v)),
            Some(Tok::Float(v)) => Ok(Expr::Float(v)),
            Some(Tok::Ident(n)) => Ok(Expr::Ident(n)),
            Some(Tok::SymRef(id)) => Ok(Expr::SymRef(id)),
            Some(Tok::P("(")) => {
                let e = self.conditional()?;
                self.expect(")")?;
                Ok(e)
            }
            Some(Tok::P(p)) => Err(format!("unexpected '{p}'")),
            None => Err("unexpected end of expression".into()),
        }
    }
}

pub fn parse(text: &str, types: TypeResolver) -> Result<Expr, String> {
    let mut p = Parser { toks: lex(text)?, pos: 0, types };
    if p.toks.is_empty() {
        return Err("empty expression".into());
    }
    let e = p.conditional()?;
    if p.pos < p.toks.len() {
        return Err(format!("unexpected {:?}", p.toks[p.pos]));
    }
    Ok(e)
}

// --- Evaluation ---------------------------------------------------------------------------

/// Where a value is stored.
#[derive(Clone, Debug, PartialEq)]
pub enum Place {
    Memory(u32),
    Register(u16),
    Bits { addr: u32, storage: u32, shift: u32, bits: u32 },
    /// Computed value (see `Value::bytes`).
    None,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SymbolRef {
    Local(DieRef),
    Global(usize),
    Function(usize),
    /// An object of a type at an address.
    Object(TypeId, u32),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Value {
    pub ty: TypeId,
    pub place: Place,
    /// Contents of values that are not stored on the target.
    pub bytes: Option<Vec<u8>>,
    /// The named symbol this value is, if any.
    pub symbol: Option<SymbolRef>,
}

impl Value {
    pub fn can_assign(&self) -> bool {
        self.place != Place::None
    }
}

/// A write that performs an assignment.
#[derive(Clone, Debug, PartialEq)]
pub enum Write {
    Memory(u32, Vec<u8>),
    Register(u16, u32),
}

/// Target state for evaluation.
pub trait Target {
    /// PC of the frame whose scope applies (None: globals only).
    fn pc(&self) -> Option<u32>;
    /// Frame register by DWARF number ([`PC_REGISTER`] for the PC).
    fn reg(&self, dwarf: u16) -> Option<u32>;
    fn read(&mut self, addr: u32, len: usize) -> Result<Vec<u8>, String>;
    /// Resolves a `${ID}` symbol reference.
    fn symbol(&mut self, id: &str) -> Option<SymbolRef>;
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Scalar {
    I(i64),
    U(u64),
    F(f64),
}

impl Scalar {
    fn as_i64(self) -> i64 {
        match self {
            Scalar::I(v) => v,
            Scalar::U(v) => v as i64,
            Scalar::F(v) => v as i64,
        }
    }
    fn as_u64(self) -> u64 {
        match self {
            Scalar::I(v) => v as u64,
            Scalar::U(v) => v,
            Scalar::F(v) => (v as i64) as u64,
        }
    }
    fn as_f64(self) -> f64 {
        match self {
            Scalar::I(v) => v as f64,
            Scalar::U(v) => v as f64,
            Scalar::F(v) => v,
        }
    }
    fn truthy(self) -> bool {
        match self {
            Scalar::F(v) => v != 0.0,
            other => other.as_u64() != 0,
        }
    }
}

fn le_u64(b: &[u8]) -> u64 {
    let mut w = [0u8; 8];
    let n = b.len().min(8);
    w[..n].copy_from_slice(&b[..n]);
    u64::from_le_bytes(w)
}

fn mask(bits: u32) -> u64 {
    if bits >= 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    }
}

fn sign_extend(v: u64, bits: u32) -> i64 {
    if bits == 0 || bits >= 64 {
        return v as i64;
    }
    let sh = 64 - bits;
    ((v << sh) as i64) >> sh
}

pub fn register_number(name: &str) -> Option<u16> {
    match name {
        "sp" => return Some(26),
        "ra" => return Some(27),
        "pc" => return Some(PC_REGISTER),
        _ => {}
    }
    let kind = name.get(..1)?;
    let n: u16 = name.get(1..)?.parse().ok()?;
    if n > 15 {
        return None;
    }
    match kind {
        "d" => Some(n),
        "a" => Some(16 + n),
        _ => None,
    }
}

pub struct Eval<'a, T: Target> {
    pub syms: &'a Symbols,
    pub target: &'a mut T,
}

impl<'a, T: Target> Eval<'a, T> {
    pub fn new(syms: &'a Symbols, target: &'a mut T) -> Self {
        Eval { syms, target }
    }

    /// Resolves a type name for casts.
    pub fn resolve_type(syms: &Symbols, name: &str) -> Option<TypeId> {
        syms.types_named(name).first().map(|d| TypeId::Die(*d))
    }

    pub fn parse(&self, text: &str) -> Result<Expr, String> {
        let syms = self.syms;
        parse(text, &|n| Self::resolve_type(syms, n))
    }

    pub fn evaluate(&mut self, text: &str) -> Result<Value, String> {
        let e = self.parse(text)?;
        self.eval(&e)
    }

    fn info(&self, t: &TypeId) -> TypeInfo {
        self.syms.type_info(t)
    }

    pub fn value_of(&mut self, s: &SymbolRef) -> Result<Value, String> {
        let syms = self.syms;
        match s {
            SymbolRef::Local(d) => {
                let pc = self.target.pc().ok_or("no stack frame for local variable")?;
                let regs: Vec<Option<u32>> = (0..=PC_REGISTER).map(|r| self.target.reg(r)).collect();
                let reg = |r: u16| regs.get(r as usize).copied().flatten();
                let target = &mut *self.target;
                let mut mem = |a: u32, n: usize| target.read(a, n).ok();
                let loc = syms.var_location(*d, pc, &reg, &mut mem)?;
                let ty = syms.var_type(*d);
                let (place, bytes) = match loc {
                    Loc::Memory(a) => (Place::Memory(a), None),
                    Loc::Register(r) => {
                        if reg(r).is_none() {
                            return Err(format!("register r{r} not available in this frame"));
                        }
                        (Place::Register(r), None)
                    }
                    Loc::Value(mut b) => {
                        b.resize(self.info(&ty).size as usize, 0);
                        (Place::None, Some(b))
                    }
                    Loc::OptimizedOut => return Err("<optimized out>".into()),
                };
                Ok(Value { ty, place, bytes, symbol: Some(s.clone()) })
            }
            SymbolRef::Global(i) => {
                let g = syms.globals.get(*i).ok_or("invalid global")?;
                // Globals without debug information (MCS data labels) are one 32-bit word.
                let ty = if g.unit == usize::MAX { TypeId::Base(TypeClass::Cardinal, 4) } else { syms.var_type(g.die_ref()) };
                Ok(Value { ty, place: Place::Memory(g.addr), bytes: None, symbol: Some(s.clone()) })
            }
            SymbolRef::Function(i) => {
                let f = syms.functions.get(*i).ok_or("invalid function")?;
                Ok(Value { ty: TypeId::Die(f.die_ref()), place: Place::Memory(f.low), bytes: None, symbol: Some(s.clone()) })
            }
            SymbolRef::Object(ty, addr) => Ok(Value { ty: ty.clone(), place: Place::Memory(*addr), bytes: None, symbol: Some(s.clone()) }),
        }
    }

    fn lookup(&mut self, name: &str) -> Result<Value, String> {
        let syms = self.syms;
        if let Some(r) = name.strip_prefix('$') {
            let n = register_number(r).ok_or_else(|| format!("Unknown register ${r}"))?;
            if self.target.reg(n).is_none() {
                return Err(format!("register ${r} not available"));
            }
            return Ok(Value { ty: TypeId::Base(TypeClass::Cardinal, 4), place: Place::Register(n), bytes: None, symbol: None });
        }
        let pc = self.target.pc();
        if let Some(pc) = pc {
            if let Some(l) = syms.locals_at(pc).into_iter().rev().find(|l| l.name == name) {
                return self.value_of(&SymbolRef::Local(l.die));
            }
        }
        let unit = pc.and_then(|pc| syms.function_at(pc)).map(|f| f.unit);
        let gi = syms.global_indices(name);
        let pick = gi
            .iter()
            .find(|&&i| Some(syms.globals[i].unit) == unit)
            .or_else(|| gi.iter().find(|&&i| syms.globals[i].external))
            .or(gi.first());
        if let Some(&i) = pick {
            return self.value_of(&SymbolRef::Global(i));
        }
        let fi = syms.function_indices(name);
        if let Some(&i) = fi.iter().find(|&&i| syms.functions[i].external).or(fi.first()) {
            return self.value_of(&SymbolRef::Function(i));
        }
        if let Some((e, en)) = syms.enumerators_named(name).first() {
            let (_, v) = syms.enumerator(*e).ok_or("invalid enumerator")?;
            let ty = TypeId::Die(*en);
            return Ok(self.rvalue(ty, Scalar::I(v)));
        }
        Err(format!("Symbol not found: {name}"))
    }

    /// Contents of a value (`size` of its type).
    pub fn load(&mut self, v: &Value) -> Result<Vec<u8>, String> {
        let size = self.info(&v.ty).size as usize;
        if let Some(b) = &v.bytes {
            let mut b = b.clone();
            if size > 0 {
                b.resize(size, 0);
            }
            return Ok(b);
        }
        match v.place {
            Place::Memory(a) => self.target.read(a, size),
            Place::Register(r) => {
                let x = self.target.reg(r).ok_or_else(|| format!("register r{r} not available"))?;
                let mut b = (x as u64).to_le_bytes().to_vec();
                b.resize(size, 0);
                Ok(b)
            }
            Place::Bits { addr, storage, shift, bits } => {
                let raw = le_u64(&self.target.read(addr, storage as usize)?);
                let mut x = (raw >> shift) & mask(bits);
                if self.info(&v.ty).class == TypeClass::Integer {
                    x = sign_extend(x, bits) as u64;
                }
                Ok(x.to_le_bytes()[..size.min(8)].to_vec())
            }
            Place::None => Err("value not available".into()),
        }
    }

    fn scalar(&mut self, v: &Value) -> Result<(Scalar, TypeInfo), String> {
        let info = self.info(&v.ty);
        let s = match info.class {
            TypeClass::Array | TypeClass::Function => match v.place {
                Place::Memory(a) => Scalar::U(a as u64),
                _ => return Err("array or function is not in memory".into()),
            },
            TypeClass::Composite => return Err(format!("'{}' is not a scalar", info.name.clone().unwrap_or_default())),
            _ => {
                let b = self.load(v)?;
                let raw = le_u64(&b);
                let class = match info.class {
                    TypeClass::Enumeration => info.base.as_ref().map(|t| self.info(t).class).unwrap_or(TypeClass::Integer),
                    c => c,
                };
                match class {
                    TypeClass::Integer => Scalar::I(sign_extend(raw, (b.len().min(8) * 8) as u32)),
                    TypeClass::Real if b.len() == 4 => Scalar::F(f32::from_le_bytes(b[..4].try_into().unwrap()) as f64),
                    TypeClass::Real if b.len() == 8 => Scalar::F(f64::from_le_bytes(b[..8].try_into().unwrap())),
                    _ => Scalar::U(raw),
                }
            }
        };
        Ok((s, info))
    }

    fn rvalue(&self, ty: TypeId, s: Scalar) -> Value {
        let info = self.info(&ty);
        let size = if info.size == 0 { 4 } else { info.size.min(8) as usize };
        let bytes = match (info.class, size) {
            (TypeClass::Real, 4) => (s.as_f64() as f32).to_le_bytes().to_vec(),
            (TypeClass::Real, _) => s.as_f64().to_le_bytes().to_vec(),
            _ => s.as_u64().to_le_bytes()[..size].to_vec(),
        };
        Value { ty, place: Place::None, bytes: Some(bytes), symbol: None }
    }

    fn int_constant(&self, v: u64) -> Value {
        let ty = if v <= i32::MAX as u64 {
            TypeId::Base(TypeClass::Integer, 4)
        } else if v <= u32::MAX as u64 {
            TypeId::Base(TypeClass::Cardinal, 4)
        } else {
            TypeId::Base(TypeClass::Cardinal, 8)
        };
        self.rvalue(ty, Scalar::U(v))
    }

    fn sub_value(&mut self, v: &Value, offset: u32, ty: TypeId, size: u32, bits: Option<(u32, u32)>) -> Result<Value, String> {
        if let (Place::Memory(a), None) = (&v.place, &v.bytes) {
            let place = match bits {
                Some((shift, count)) => Place::Bits { addr: a + offset, storage: size, shift, bits: count },
                None => Place::Memory(a + offset),
            };
            return Ok(Value { ty, place, bytes: None, symbol: None });
        }
        let all = self.load(v)?;
        let (o, n) = (offset as usize, size as usize);
        let mut part = all.get(o..o + n).ok_or("member outside of value")?.to_vec();
        if let Some((shift, count)) = bits {
            let x = (le_u64(&part) >> shift) & mask(count);
            let sz = self.info(&ty).size.min(8) as usize;
            part = x.to_le_bytes()[..sz].to_vec();
        }
        Ok(Value { ty, place: Place::None, bytes: Some(part), symbol: None })
    }

    fn deref(&mut self, v: Value) -> Result<Value, String> {
        let info = self.info(&v.ty);
        match info.class {
            TypeClass::Pointer => {
                let (s, _) = self.scalar(&v)?;
                let base = info.base.unwrap_or(TypeId::Void);
                if self.info(&base).size == 0 {
                    return Err("cannot dereference a void pointer".into());
                }
                Ok(Value { ty: base, place: Place::Memory(s.as_u64() as u32), bytes: None, symbol: None })
            }
            TypeClass::Array => {
                let base = info.base.unwrap_or(TypeId::Void);
                let size = self.info(&base).size;
                self.sub_value(&v, 0, base, size, None)
            }
            _ => Err(format!("cannot dereference '{}'", info.name.unwrap_or_default())),
        }
    }

    fn member(&mut self, v: Value, name: &str) -> Result<Value, String> {
        let info = self.info(&v.ty);
        if info.class != TypeClass::Composite {
            return Err(format!("'{}' is not a struct or union", info.name.unwrap_or_default()));
        }
        let m = self.syms.find_member(&v.ty, name).ok_or_else(|| format!("no member named '{name}'"))?;
        self.sub_value(&v, m.offset, m.ty, m.size, m.bits)
    }

    pub fn eval(&mut self, e: &Expr) -> Result<Value, String> {
        match e {
            Expr::Int(v) => Ok(self.int_constant(*v)),
            Expr::Float(v) => Ok(self.rvalue(TypeId::Base(TypeClass::Real, 8), Scalar::F(*v))),
            Expr::Ident(n) => self.lookup(n),
            Expr::SymRef(id) => {
                let s = self.target.symbol(id).ok_or_else(|| format!("Invalid symbol ID: {id}"))?;
                self.value_of(&s)
            }
            Expr::Member(b, name) => {
                let v = self.eval(b)?;
                self.member(v, name)
            }
            Expr::Arrow(b, name) => {
                let v = self.eval(b)?;
                let d = self.deref(v)?;
                self.member(d, name)
            }
            Expr::Index(b, i) => {
                let v = self.eval(b)?;
                let iv = self.eval(i)?;
                let (idx, _) = self.scalar(&iv)?;
                let info = self.info(&v.ty);
                let base = info.base.clone().unwrap_or(TypeId::Void);
                let es = self.info(&base).size;
                match info.class {
                    TypeClass::Array => {
                        let off = (idx.as_i64() * es as i64) as u32;
                        self.sub_value(&v, off, base, es, None)
                    }
                    TypeClass::Pointer => {
                        let (p, _) = self.scalar(&v)?;
                        let addr = (p.as_u64() as i64 + idx.as_i64() * es as i64) as u32;
                        Ok(Value { ty: base, place: Place::Memory(addr), bytes: None, symbol: None })
                    }
                    _ => Err(format!("'{}' cannot be indexed", info.name.unwrap_or_default())),
                }
            }
            Expr::Unary(op, b) => {
                let v = self.eval(b)?;
                match *op {
                    "*" => self.deref(v),
                    "&" => match (&v.place, &v.bytes) {
                        (Place::Memory(a), None) => {
                            let a = *a;
                            Ok(self.rvalue(TypeId::PointerTo(Box::new(v.ty)), Scalar::U(a as u64)))
                        }
                        _ => Err("cannot take the address of this expression".into()),
                    },
                    _ => {
                        let (s, info) = self.scalar(&v)?;
                        let int_ty = TypeId::Base(TypeClass::Integer, 4);
                        match *op {
                            "!" => Ok(self.rvalue(int_ty, Scalar::I(!s.truthy() as i64))),
                            "+" => Ok(v),
                            "-" => Ok(match s {
                                Scalar::F(x) => self.rvalue(v.ty, Scalar::F(-x)),
                                other => self.rvalue(Self::promote(&info, &v.ty), Scalar::I(other.as_i64().wrapping_neg())),
                            }),
                            _ => match s {
                                Scalar::F(_) => Err("'~' needs an integer".into()),
                                other => Ok(self.rvalue(Self::promote(&info, &v.ty), Scalar::U(!other.as_u64()))),
                            },
                        }
                    }
                }
            }
            Expr::Binary(op, a, b) => {
                let va = self.eval(a)?;
                if *op == "&&" || *op == "||" {
                    let (sa, _) = self.scalar(&va)?;
                    let int_ty = TypeId::Base(TypeClass::Integer, 4);
                    if (*op == "&&" && !sa.truthy()) || (*op == "||" && sa.truthy()) {
                        return Ok(self.rvalue(int_ty, Scalar::I((*op == "||") as i64)));
                    }
                    let vb = self.eval(b)?;
                    let (sb, _) = self.scalar(&vb)?;
                    return Ok(self.rvalue(int_ty, Scalar::I(sb.truthy() as i64)));
                }
                let vb = self.eval(b)?;
                self.binary(op, va, vb)
            }
            // Only the taken branch is evaluated: the untaken one routinely faults on purpose,
            // e.g. an ORTI attribute that reads a task control block only once its core is up.
            Expr::Cond(c, then, otherwise) => {
                let vc = self.eval(c)?;
                let (sc, _) = self.scalar(&vc)?;
                self.eval(if sc.truthy() { then } else { otherwise })
            }
            Expr::Cast(ty, b) => {
                let v = self.eval(b)?;
                let ti = self.info(ty);
                if matches!(ti.class, TypeClass::Composite | TypeClass::Array) {
                    return match v.place {
                        Place::Memory(a) => Ok(Value { ty: ty.clone(), place: Place::Memory(a), bytes: None, symbol: None }),
                        _ => Err("cannot cast to an aggregate type".into()),
                    };
                }
                let (s, _) = self.scalar(&v)?;
                let conv = match ti.class {
                    TypeClass::Real => Scalar::F(s.as_f64()),
                    TypeClass::Integer => Scalar::I(sign_extend(s.as_u64(), ti.size.clamp(1, 8) * 8)),
                    _ => Scalar::U(s.as_u64() & mask(ti.size.clamp(1, 8) * 8)),
                };
                Ok(self.rvalue(ty.clone(), conv))
            }
            Expr::Sizeof(b) => {
                let v = self.eval(b)?;
                let n = self.info(&v.ty).size;
                Ok(self.rvalue(TypeId::Base(TypeClass::Cardinal, 4), Scalar::U(n as u64)))
            }
            Expr::SizeofType(t) => {
                let n = self.info(t).size;
                Ok(self.rvalue(TypeId::Base(TypeClass::Cardinal, 4), Scalar::U(n as u64)))
            }
        }
    }

    /// Integer promotion: at least int, keeping unsigned 32/64-bit types.
    fn promote(info: &TypeInfo, ty: &TypeId) -> TypeId {
        match info.class {
            TypeClass::Real => ty.clone(),
            TypeClass::Cardinal if info.size >= 4 => TypeId::Base(TypeClass::Cardinal, info.size.min(8) as u8),
            _ if info.size > 4 => TypeId::Base(TypeClass::Integer, 8),
            _ => TypeId::Base(TypeClass::Integer, 4),
        }
    }

    fn binary(&mut self, op: &str, va: Value, vb: Value) -> Result<Value, String> {
        let (sa, ia) = self.scalar(&va)?;
        let (sb, ib) = self.scalar(&vb)?;
        let is_ptr = |i: &TypeInfo| matches!(i.class, TypeClass::Pointer | TypeClass::Array);
        let elem_size = |s: &Self, i: &TypeInfo| i.base.as_ref().map(|b| s.info(b).size).unwrap_or(1).max(1) as i64;
        let ptr_type = |i: &TypeInfo, t: &TypeId| match i.class {
            TypeClass::Array => TypeId::PointerTo(Box::new(i.base.clone().unwrap_or(TypeId::Void))),
            _ => t.clone(),
        };

        if matches!(op, "+" | "-") && is_ptr(&ia) && !is_ptr(&ib) {
            let n = elem_size(self, &ia) * sb.as_i64();
            let a = sa.as_u64() as i64;
            let r = if op == "+" { a + n } else { a - n };
            return Ok(self.rvalue(ptr_type(&ia, &va.ty), Scalar::U(r as u32 as u64)));
        }
        if op == "+" && is_ptr(&ib) && !is_ptr(&ia) {
            let r = sb.as_u64() as i64 + elem_size(self, &ib) * sa.as_i64();
            return Ok(self.rvalue(ptr_type(&ib, &vb.ty), Scalar::U(r as u32 as u64)));
        }
        if op == "-" && is_ptr(&ia) && is_ptr(&ib) {
            let r = (sa.as_u64() as i64 - sb.as_u64() as i64) / elem_size(self, &ia);
            return Ok(self.rvalue(TypeId::Base(TypeClass::Integer, 4), Scalar::I(r)));
        }

        let float = matches!(sa, Scalar::F(_)) || matches!(sb, Scalar::F(_));
        let size = ia.size.max(ib.size).clamp(4, 8);
        let unsigned = !float
            && ((matches!(sa, Scalar::U(_)) && ia.size >= size && ia.class != TypeClass::Integer)
                || (matches!(sb, Scalar::U(_)) && ib.size >= size && ib.class != TypeClass::Integer));
        let res_ty = if float {
            TypeId::Base(TypeClass::Real, 8)
        } else {
            TypeId::Base(if unsigned { TypeClass::Cardinal } else { TypeClass::Integer }, size as u8)
        };
        let int_ty = TypeId::Base(TypeClass::Integer, 4);

        if let Some(ord) = match op {
            "==" | "!=" | "<" | ">" | "<=" | ">=" => Some(if float {
                sa.as_f64().partial_cmp(&sb.as_f64()).ok_or("comparison with NaN")?
            } else if unsigned {
                sa.as_u64().cmp(&sb.as_u64())
            } else {
                sa.as_i64().cmp(&sb.as_i64())
            }),
            _ => None,
        } {
            use std::cmp::Ordering::*;
            let r = match op {
                "==" => ord == Equal,
                "!=" => ord != Equal,
                "<" => ord == Less,
                ">" => ord == Greater,
                "<=" => ord != Greater,
                _ => ord != Less,
            };
            return Ok(self.rvalue(int_ty, Scalar::I(r as i64)));
        }

        if float {
            let (x, y) = (sa.as_f64(), sb.as_f64());
            let r = match op {
                "+" => x + y,
                "-" => x - y,
                "*" => x * y,
                "/" => x / y,
                _ => return Err(format!("'{op}' needs integer operands")),
            };
            return Ok(self.rvalue(res_ty, Scalar::F(r)));
        }
        if matches!(op, "/" | "%") && sb.as_u64() == 0 {
            return Err("division by zero".into());
        }
        let r = if unsigned {
            let (x, y) = (sa.as_u64(), sb.as_u64());
            Scalar::U(match op {
                "+" => x.wrapping_add(y),
                "-" => x.wrapping_sub(y),
                "*" => x.wrapping_mul(y),
                "/" => x / y,
                "%" => x % y,
                "&" => x & y,
                "|" => x | y,
                "^" => x ^ y,
                "<<" => x.wrapping_shl(y as u32),
                ">>" => (x & mask(size * 8)).wrapping_shr(y as u32),
                _ => return Err(format!("unsupported operator '{op}'")),
            })
        } else {
            let (x, y) = (sa.as_i64(), sb.as_i64());
            Scalar::I(match op {
                "+" => x.wrapping_add(y),
                "-" => x.wrapping_sub(y),
                "*" => x.wrapping_mul(y),
                "/" => x.wrapping_div(y),
                "%" => x.wrapping_rem(y),
                "&" => x & y,
                "|" => x | y,
                "^" => x ^ y,
                "<<" => x.wrapping_shl(y as u32),
                ">>" => x.wrapping_shr(y as u32),
                _ => return Err(format!("unsupported operator '{op}'")),
            })
        };
        Ok(self.rvalue(res_ty, r))
    }

    /// The write that stores `data` (the value's type size) into `v`.
    pub fn assign(&mut self, v: &Value, data: &[u8]) -> Result<Write, String> {
        let size = self.info(&v.ty).size as usize;
        if data.len() != size {
            return Err(format!("value has {} bytes, the type has {size}", data.len()));
        }
        match v.place {
            Place::Memory(a) if v.bytes.is_none() => Ok(Write::Memory(a, data.to_vec())),
            Place::Register(r) => {
                if size > 4 {
                    return Err("value does not fit into a register".into());
                }
                Ok(Write::Register(r, le_u64(data) as u32))
            }
            Place::Bits { addr, storage, shift, bits } => {
                let cur = le_u64(&self.target.read(addr, storage as usize)?);
                let m = mask(bits) << shift;
                let merged = (cur & !m) | ((le_u64(data) << shift) & m);
                Ok(Write::Memory(addr, merged.to_le_bytes()[..storage as usize].to_vec()))
            }
            _ => Err("Expression is not assignable".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn parses_c_expressions() {
        let no_types = |_: &str| None;
        let e = parse("a.b[3]->c + *p * 2", &no_types).unwrap();
        assert!(matches!(e, Expr::Binary("+", _, _)), "{e:?}");
        assert_eq!(parse("0x10 << 2", &no_types).unwrap(), Expr::Binary("<<", Box::new(Expr::Int(16)), Box::new(Expr::Int(2))));
        assert_eq!(parse("'A'", &no_types).unwrap(), Expr::Int(65));
        assert_eq!(parse("(x)", &no_types).unwrap(), Expr::Ident("x".into()));
        assert_eq!(parse("${GV:3}", &no_types).unwrap(), Expr::SymRef("GV:3".into()));
        let t = |n: &str| (n == "uint32").then_some(TypeId::Base(TypeClass::Cardinal, 4));
        assert_eq!(
            parse("*(uint32*)0x70000044", &t).unwrap(),
            Expr::Unary(
                "*",
                Box::new(Expr::Cast(TypeId::PointerTo(Box::new(TypeId::Base(TypeClass::Cardinal, 4))), Box::new(Expr::Int(0x7000_0044))))
            )
        );
        assert_eq!(
            parse("(unsigned char)300", &no_types).unwrap(),
            Expr::Cast(TypeId::Base(TypeClass::Cardinal, 1), Box::new(Expr::Int(300)))
        );
        // The conditional operator, which every dynamic ORTI attribute is built from.
        assert_eq!(
            parse("a ? b : c", &no_types).unwrap(),
            Expr::Cond(Box::new(Expr::Ident("a".into())), Box::new(Expr::Ident("b".into())), Box::new(Expr::Ident("c".into())))
        );
        // Binds looser than every binary operator, on all three operands.
        assert_eq!(
            parse("x == 2 ? 1 + 1 : 0", &no_types).unwrap(),
            Expr::Cond(
                Box::new(Expr::Binary("==", Box::new(Expr::Ident("x".into())), Box::new(Expr::Int(2)))),
                Box::new(Expr::Binary("+", Box::new(Expr::Int(1)), Box::new(Expr::Int(1)))),
                Box::new(Expr::Int(0))
            )
        );
        // Right associative: a ? b : (c ? d : e).
        assert!(matches!(parse("a ? b : c ? d : e", &no_types).unwrap(), Expr::Cond(_, _, o) if matches!(*o, Expr::Cond(..))));
        // The shape MICROSAR's ORTI generator emits for TASK.STATE.
        let orti = "Core_Dyn.OsState == 2 ? ( Trace_Dyn.CurrentTask == &Trace_Task ? 0 : Task_Dyn.State ) : 0xFF";
        assert!(matches!(parse(orti, &no_types).unwrap(), Expr::Cond(_, t, _) if matches!(*t, Expr::Cond(..))), "{orti}");
        // ...and for ALARM.vs_AlarmTime_ms, where the then-branch carries arithmetic.
        assert!(matches!(
            parse("s == 1 ? (a - b) * 1000000 / 1000000 : 0", &no_types).unwrap(),
            Expr::Cond(_, t, _) if matches!(*t, Expr::Binary("/", ..))
        ));
        assert!(parse("a ? b", &no_types).is_err());
        assert!(parse("a : b", &no_types).is_err());
        assert!(parse("a +", &no_types).is_err());
        assert!(parse("", &no_types).is_err());
    }

    struct Mem {
        pc: Option<u32>,
        regs: HashMap<u16, u32>,
        mem: HashMap<u32, u8>,
    }

    impl Target for Mem {
        fn pc(&self) -> Option<u32> {
            self.pc
        }
        fn reg(&self, r: u16) -> Option<u32> {
            self.regs.get(&r).copied()
        }
        fn read(&mut self, addr: u32, len: usize) -> Result<Vec<u8>, String> {
            (0..len as u32).map(|i| self.mem.get(&(addr + i)).copied().ok_or_else(|| format!("no memory at 0x{:x}", addr + i))).collect()
        }
        fn symbol(&mut self, _: &str) -> Option<SymbolRef> {
            None
        }
    }

    /// Any enum global of the test ELF (1, 2 or 4 bytes): name lookup, enumerators, arithmetic,
    /// address-of, casts and the conditional operator, evaluated against a fake memory.
    #[test]
    fn real_elf_expressions() {
        let Some(path) = crate::tests::test_elf() else { return };
        let s = Symbols::load(&path).unwrap();
        let mut t = Mem { pc: None, regs: HashMap::new(), mem: HashMap::new() };
        let (name, addr, size, enums) = s
            .globals
            .iter()
            .filter(|g| g.addr != 0)
            .find_map(|g| {
                let v = Eval::new(&s, &mut t).evaluate(&g.name).ok()?;
                let info = s.type_info(&v.ty);
                let enums = s.enumerators(&v.ty);
                let fits = enums.len() >= 2 && enums.iter().take(2).all(|e| (0..0x80).contains(&e.2));
                (v.place == Place::Memory(g.addr) && info.class == TypeClass::Enumeration && [1, 2, 4].contains(&info.size) && fits)
                    .then(|| (g.name.clone(), g.addr, info.size as usize, enums))
            })
            .expect("an enum global");
        let ((_, e0, v0), (_, e1, v1)) = (&enums[0], &enums[1]);
        let val = *v0 as i32;
        for (i, b) in val.to_le_bytes()[..size].iter().enumerate() {
            t.mem.insert(addr + i as u32, *b);
        }
        let mut ev = Eval::new(&s, &mut t);
        let v = ev.evaluate(&name).unwrap();
        assert_eq!(v.place, Place::Memory(addr));
        let r = ev.evaluate(&format!("{name} == {e0}")).unwrap();
        assert_eq!(ev.load(&r).unwrap(), 1i32.to_le_bytes());
        let r = ev.evaluate(&format!("{name} == {e1}")).unwrap();
        assert_eq!(ev.load(&r).unwrap(), ((v0 == v1) as i32).to_le_bytes());
        let r = ev.evaluate(&format!("({name} + 3) * 2")).unwrap();
        assert_eq!(ev.load(&r).unwrap(), val.wrapping_add(3).wrapping_mul(2).to_le_bytes());
        let r = ev.evaluate(&format!("&{name}")).unwrap();
        assert_eq!(s.type_info(&r.ty).class, TypeClass::Pointer);
        assert_eq!(ev.load(&r).unwrap(), addr.to_le_bytes());
        let r = ev.evaluate(&format!("*(unsigned char*){addr:#x}")).unwrap();
        assert_eq!(r.place, Place::Memory(addr));
        assert_eq!(ev.load(&r).unwrap(), vec![val.to_le_bytes()[0]]);
        assert_eq!(ev.assign(&r, &[7]).unwrap(), Write::Memory(addr, vec![7]));

        // Conditional operator. 0xdead0000 is unmapped in `Mem`, so an evaluated branch that
        // reads it fails - which is how the untaken branch is proven unread.
        let r = ev.evaluate(&format!("{name} == {val} ? 1 : *(unsigned int*)0xdead0000 + 1")).unwrap();
        assert_eq!(ev.load(&r).unwrap(), 1i32.to_le_bytes());
        let r = ev.evaluate(&format!("{name} != {val} ? *(unsigned int*)0xdead0000 + 1 : 7")).unwrap();
        assert_eq!(ev.load(&r).unwrap(), 7i32.to_le_bytes());
        assert!(ev.evaluate(&format!("{name} == {val} ? *(unsigned int*)0xdead0000 + 1 : 7")).is_err());
        // The taken branch keeps its place, so a nested ORTI-shaped attribute stays readable.
        let r = ev.evaluate(&format!("{name} == {val} ? ( {name} != {val} ? 0 : {name} ) : 0xFF")).unwrap();
        assert_eq!(r.place, Place::Memory(addr));
        assert_eq!(ev.load(&r).unwrap(), val.to_le_bytes()[..size]);

        // Locals: in the first statement of some function that has any, they resolve to a place.
        let pc = s
            .functions
            .iter()
            .filter_map(|f| s.lines_in_range(f.low, f.high).into_iter().map(|l| l.start).find(|&a| !s.locals_at(a).is_empty()))
            .next()
            .expect("a function with locals");
        let locals = s.locals_at(pc);
        let mut t = Mem { pc: Some(pc), regs: (0..64).map(|r| (r, 0x7000_1000)).collect(), mem: HashMap::new() };
        let mut ev = Eval::new(&s, &mut t);
        assert!(locals.iter().any(|l| ev.evaluate(&l.name).is_ok()), "no local of {pc:#x} evaluates: {:?}", locals.iter().map(|l| &l.name).collect::<Vec<_>>());
        let v = ev.evaluate("$sp + 4").unwrap();
        assert_eq!(ev.load(&v).unwrap(), 0x7000_1004u32.to_le_bytes());
    }
}
