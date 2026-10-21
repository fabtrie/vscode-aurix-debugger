//! TCF Expressions service and the variable/type part of the Symbols service.
//!
//! Symbol IDs name the ELF file they belong to: `E<n>/<id>` with `<id>`:
//! - `FN:<i>` function, `GV:<i>` global variable
//! - `LV:<unit>.<die>@<frame>` local variable/parameter in a stack frame
//! - `OB:<addr>:<type>` object of a type at an address (e.g. `s.member`)
//! - `MB:<unit>.<die>` struct/union member, `EN:<unit>.<die>:<size>` enumerator
//! - types: see [`TypeId::to_id`]
//!
//! Expressions are evaluated in the scope of a stack frame (locals, then
//! globals) or of a core (globals only when it is running, which is what
//! live watch uses). With several ELF files (application, bootmanager, FBL),
//! names are looked up in the ELF of the frame's PC first, then in the others.

use std::collections::{HashMap, VecDeque};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::{json, Value as Json};

use aurix_core::ops;
use aurix_core::unwind::Frame;
use symbols::expr::{Eval, Place, SymbolRef, Target, Value, Write};
use symbols::types::{TypeClass, TypeId};
use symbols::{DieRef, Symbols, PC_REGISTER};

use aurix_core::tricore::local_address;

use crate::contexts::{core_id, cores, find_core, CoreInfo, CONTAINER};
use crate::symstore::{elf_id, split_elf_id, Elf};
use crate::Shared;

const ERR_OTHER: i64 = 1;
const ERR_JSON_SYNTAX: i64 = 2;
const ERR_IS_RUNNING: i64 = 14;
const ERR_INV_DATA_SIZE: i64 = 15;
const ERR_INV_CONTEXT: i64 = 16;
const ERR_INV_EXPRESSION: i64 = 18;
const ERR_SYM_NOT_FOUND: i64 = 22;
const ERR_INV_COMMAND: i64 = 25;

/// Expression IDs kept (the client never disposes them).
const MAX_EXPRESSIONS: usize = 8192;
/// Largest value returned by Expressions.evaluate.
const MAX_VALUE_SIZE: usize = 0x10000;
/// Start of the peripheral (SFR) address space.
const PERIPHERAL_BASE: u32 = 0xF000_0000;

pub type Res<T> = Result<T, (i64, String)>;

fn fail<T>(code: i64, msg: impl Into<String>) -> Res<T> {
    Err((code, msg.into()))
}

struct ExprEntry {
    parent: String,
    /// Shown expression.
    text: String,
    /// Symbol the expression stands for (frame locals).
    symbol: Option<String>,
}

#[derive(Default)]
pub struct ExprTable {
    map: HashMap<String, ExprEntry>,
    order: VecDeque<String>,
    next: u64,
}

impl ExprTable {
    fn add(&mut self, e: ExprEntry) -> String {
        self.next += 1;
        let id = format!("EXPR{}", self.next);
        self.map.insert(id.clone(), e);
        self.order.push_back(id.clone());
        while self.order.len() > MAX_EXPRESSIONS {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
        id
    }
}

// --- Arguments ---------------------------------------------------------------------------

fn arg(args: &[String], i: usize) -> Res<Json> {
    let s = args.get(i).ok_or((ERR_JSON_SYNTAX, format!("missing argument {}", i + 1)))?;
    serde_json::from_str(s).map_err(|e| (ERR_JSON_SYNTAX, format!("argument {}: {e}", i + 1)))
}

fn arg_str(args: &[String], i: usize) -> Res<String> {
    match arg(args, i)? {
        Json::String(s) => Ok(s),
        Json::Null => Ok(String::new()),
        v => fail(ERR_JSON_SYNTAX, format!("argument {}: expected a string, got {v}", i + 1)),
    }
}

// --- Scope -------------------------------------------------------------------------------

struct Scope {
    core: CoreInfo,
    frame_id: Option<String>,
    frame_index: Option<usize>,
    pc: Option<u32>,
    regs: [Option<u32>; PC_REGISTER as usize + 1],
}

fn frames(sh: &Shared, c: &CoreInfo) -> Res<Vec<Frame>> {
    if !c.suspended() || sh.is_stepping(c.idx) {
        return fail(ERR_IS_RUNNING, "Execution context is running");
    }
    if let Some(f) = sh.stacks.lock().unwrap().get(&c.idx) {
        return Ok(f.clone());
    }
    let f = crate::stack::unwind_core(sh, c.idx).map_err(|e| (ERR_OTHER, e))?;
    sh.stacks.lock().unwrap().insert(c.idx, f.clone());
    Ok(f)
}

/// Evaluation scope of a context: a stack frame (`...C0.F2`), a core (its
/// top frame while suspended) or the chip (globals).
fn scope_for(sh: &Shared, ctx: &str) -> Res<Scope> {
    let core = match find_core(&sh.handle, ctx) {
        Some(c) => c,
        None if ctx.is_empty() || ctx == CONTAINER => match cores(&sh.handle).into_iter().find(|c| c.examined) {
            Some(c) => c,
            None => return fail(ERR_INV_CONTEXT, "No examined core"),
        },
        None => return fail(ERR_INV_CONTEXT, format!("Invalid context: {ctx}")),
    };
    let index = match ctx.rsplit_once(".F") {
        Some((head, n)) if head == core.id => Some(n.parse::<usize>().map_err(|_| (ERR_INV_CONTEXT, format!("Invalid stack frame: {ctx}")))?),
        _ if ctx == core.id && core.suspended() && !sh.is_stepping(core.idx) => Some(0),
        _ => None,
    };
    let mut s = Scope { core, frame_id: None, frame_index: None, pc: None, regs: [None; PC_REGISTER as usize + 1] };
    if let Some(i) = index {
        let fr = frames(sh, &s.core)?;
        let f = fr.get(i).ok_or_else(|| (ERR_INV_CONTEXT, format!("Invalid stack frame: {ctx}")))?;
        s.regs[..32].copy_from_slice(&f.regs);
        s.regs[PC_REGISTER as usize] = Some(f.pc);
        // Caller frames hold return addresses: use the call instruction's scope.
        s.pc = Some(if i == 0 { f.pc } else { f.pc.wrapping_sub(1) });
        s.frame_index = Some(i);
        s.frame_id = Some(format!("{}.F{i}", s.core.id));
    }
    Ok(s)
}

/// Reads memory as core `t` sees it (HSM-private addresses through its debug window).
fn read_mem(sh: &Shared, t: usize, addr: u32, len: usize) -> Result<Vec<u8>, String> {
    if len > MAX_VALUE_SIZE * 16 {
        return Err(format!("{len} bytes are too many to read"));
    }
    sh.handle
        .call(move |cx| ops::read_memory_as(cx, t, addr, len).map_err(|e| e.to_string()))
        .unwrap_or_else(|| Err("server stopped".into()))
}

struct TcfTarget<'a> {
    sh: &'a Shared,
    /// Target index of the scope's core.
    core: usize,
    /// Target index for the core-local memory (see [`local_core`]).
    local: usize,
    /// ELF the expression is evaluated with.
    elf: usize,
    pc: Option<u32>,
    regs: [Option<u32>; PC_REGISTER as usize + 1],
    /// The last read that failed, so a caller can tell a memory fault from a bad name.
    read_error: Option<String>,
}

impl Target for TcfTarget<'_> {
    fn pc(&self) -> Option<u32> {
        self.pc
    }
    fn reg(&self, r: u16) -> Option<u32> {
        self.regs.get(r as usize).copied().flatten()
    }
    fn read(&mut self, addr: u32, len: usize) -> Result<Vec<u8>, String> {
        let r = read_mem(self.sh, if local_address(addr) { self.local } else { self.core }, addr, len);
        if let Err(e) = &r {
            self.read_error = Some(e.clone());
        }
        r
    }
    fn symbol(&mut self, id: &str) -> Option<SymbolRef> {
        symbol_ref(self.elf, id)
    }
}

/// A `${ID}` symbol reference of an expression evaluated with ELF `elf`. Symbols of another ELF
/// do not exist in this one, so an `E<n>/` ID only resolves in its own.
pub fn symbol_ref(elf: usize, id: &str) -> Option<SymbolRef> {
    match split_elf_id(id) {
        Some((n, rest)) if n == elf => parse_symbol(rest),
        Some(_) => None,
        None => parse_symbol(id),
    }
}

fn target<'a>(sh: &'a Shared, elf: usize, s: &Scope) -> TcfTarget<'a> {
    TcfTarget { sh, core: s.core.idx, local: local_core(sh, elf, s), elf, pc: s.pc, regs: s.regs, read_error: None }
}

/// The target whose core-local memory (`0xD000_0000` DSPR / `0xC000_0000` PSPR alias) an
/// expression of ELF `elf` means: the scope's core when it runs the ELF (it is one of the cores
/// configured for it, or executes its code), else the first configured one.
fn local_core(sh: &Shared, elf: usize, s: &Scope) -> usize {
    let runs = sh.symbols.lock().unwrap().run_cores(elf).to_vec();
    let Some(&home) = runs.first() else { return s.core.idx };
    if runs.iter().any(|&c| core_id(c) == s.core.id) || s.pc.is_some_and(|pc| sh.elf(elf).is_some_and(|e| e.function_at(pc).is_some())) {
        return s.core.idx;
    }
    cores(&sh.handle).into_iter().find(|c| c.id == core_id(home)).map_or(s.core.idx, |c| c.idx)
}

/// ELF files to evaluate an expression of `ctx` with, in lookup order: an ELF named by a
/// `${E<n>/...}` reference, then the ELF of the scope's PC, then the others.
fn elfs_for(sh: &Shared, ctx: &str, pc: Option<u32>, text: &str) -> Res<Vec<Elf>> {
    let set = sh.symbols_for(ctx).ok_or((ERR_SYM_NOT_FOUND, "No symbols loaded".to_string()))?;
    let mut elfs = set.ordered(pc);
    let named = text.find("${").and_then(|p| split_elf_id(&text[p + 2..])).map(|(n, _)| n);
    if let Some(i) = named.and_then(|n| elfs.iter().position(|e| e.0 == n)) {
        let e = elfs.remove(i);
        elfs.insert(0, e);
    }
    Ok(elfs)
}

/// The ELF a symbol ID belongs to and the ID within it.
fn elf_of<'i>(sh: &Shared, id: &'i str) -> Res<(Elf, &'i str)> {
    let (n, rest) = split_elf_id(id).ok_or_else(|| (ERR_SYM_NOT_FOUND, format!("Invalid symbol ID: {id}")))?;
    let syms = sh.elf(n).ok_or_else(|| (ERR_SYM_NOT_FOUND, format!("Invalid symbol ID: {id}")))?;
    Ok(((n, syms), rest))
}

// --- Symbol IDs ----------------------------------------------------------------------------

fn die_id(d: DieRef) -> String {
    format!("{}.{:x}", d.unit, d.off)
}

fn parse_die(s: &str) -> Option<DieRef> {
    let (u, o) = s.split_once('.')?;
    Some(DieRef { unit: u.parse().ok()?, off: usize::from_str_radix(o, 16).ok()? })
}

fn parse_symbol(id: &str) -> Option<SymbolRef> {
    if let Some(n) = id.strip_prefix("GV:") {
        return n.parse().ok().map(SymbolRef::Global);
    }
    if let Some(n) = id.strip_prefix("FN:") {
        return n.parse().ok().map(SymbolRef::Function);
    }
    if let Some(rest) = id.strip_prefix("LV:") {
        return parse_die(rest.split_once('@')?.0).map(SymbolRef::Local);
    }
    if let Some(rest) = id.strip_prefix("OB:") {
        let (a, t) = rest.split_once(':')?;
        return Some(SymbolRef::Object(TypeId::parse(t)?, u32::from_str_radix(a, 16).ok()?));
    }
    None
}

fn symbol_id(elf: usize, v: &Value, s: &Scope) -> Option<String> {
    let id = match &v.symbol {
        Some(SymbolRef::Local(d)) => s.frame_id.as_ref().map(|f| format!("LV:{}@{f}", die_id(*d))),
        Some(SymbolRef::Global(i)) => Some(format!("GV:{i}")),
        Some(SymbolRef::Function(i)) => Some(format!("FN:{i}")),
        Some(SymbolRef::Object(t, a)) => Some(format!("OB:{a:x}:{}", t.to_id())),
        None => match (&v.place, &v.bytes) {
            (Place::Memory(a), None) => Some(format!("OB:{a:x}:{}", v.ty.to_id())),
            _ => None,
        },
    };
    id.map(|id| elf_id(elf, &id))
}

/// The type an ID (within its ELF) stands for, or of the symbol it names.
fn type_of_id(syms: &Symbols, id: &str) -> Option<TypeId> {
    if let Some(t) = TypeId::parse(id) {
        return Some(t);
    }
    if let Some(m) = id.strip_prefix("MB:").and_then(parse_die) {
        return syms.member_info(m).map(|m| m.ty);
    }
    match parse_symbol(id)? {
        SymbolRef::Local(d) => Some(syms.var_type(d)),
        SymbolRef::Global(i) => syms.globals.get(i).map(|g| syms.var_type(g.die_ref())),
        SymbolRef::Function(i) => syms.functions.get(i).map(|f| TypeId::Die(f.die_ref())),
        SymbolRef::Object(t, _) => Some(t),
    }
}

fn type_fields(elf: usize, syms: &Symbols, t: &TypeId, ctx: &mut Json) {
    let info = syms.type_info(t);
    ctx["TypeID"] = json!(elf_id(elf, &t.to_id()));
    ctx["TypeClass"] = json!(info.class as u8);
    ctx["Size"] = json!(info.size);
}

pub fn symbol_context(sh: &Shared, full_id: &str) -> Res<Json> {
    let ((n, syms), id) = elf_of(sh, full_id)?;
    let not_found = || (ERR_SYM_NOT_FOUND, format!("Invalid symbol ID: {full_id}"));

    if let Some(t) = TypeId::parse(id) {
        let info = syms.type_info(&t);
        let mut ctx = json!({ "ID": full_id, "OwnerID": CONTAINER, "Class": 4, "TypeClass": info.class as u8,
                              "Size": info.size, "TypeID": full_id, "Flags": 0 });
        if let Some(name) = &info.name {
            ctx["Name"] = json!(name);
        }
        if let Some(b) = &info.base {
            ctx["BaseTypeID"] = json!(elf_id(n, &b.to_id()));
        }
        if let Some(l) = info.length {
            ctx["Length"] = json!(l);
        }
        return Ok(ctx);
    }
    if let Some(m) = id.strip_prefix("MB:").and_then(parse_die) {
        let mi = syms.member_info(m).ok_or_else(not_found)?;
        let mut ctx = json!({ "ID": full_id, "OwnerID": CONTAINER, "Class": 2, "Offset": mi.offset, "Flags": 0 });
        type_fields(n, &syms, &mi.ty, &mut ctx);
        ctx["Size"] = json!(mi.size);
        if let Some(name) = mi.name {
            ctx["Name"] = json!(name);
        }
        return Ok(ctx);
    }
    if let Some(rest) = id.strip_prefix("EN:") {
        let (d, size) = rest.rsplit_once(':').ok_or_else(not_found)?;
        let d = parse_die(d).ok_or_else(not_found)?;
        let size: usize = size.parse().map_err(|_| not_found())?;
        let (name, value) = syms.enumerator(d).ok_or_else(not_found)?;
        let bytes = &value.to_le_bytes()[..size.clamp(1, 8)];
        return Ok(json!({ "ID": full_id, "OwnerID": CONTAINER, "Class": 1, "Name": name, "Size": size,
                          "TypeClass": TypeClass::Enumeration as u8, "Value": B64.encode(bytes), "Flags": 0 }));
    }

    let sym = parse_symbol(id).ok_or_else(not_found)?;
    match &sym {
        SymbolRef::Function(i) => {
            let f = syms.functions.get(*i).ok_or_else(not_found)?;
            Ok(json!({ "ID": full_id, "OwnerID": CONTAINER, "Name": f.name, "Class": 3, "TypeClass": TypeClass::Function as u8,
                       "TypeID": elf_id(n, &TypeId::Die(f.die_ref()).to_id()), "Address": f.low, "Size": f.high - f.low, "Flags": 0 }))
        }
        SymbolRef::Global(i) => {
            let g = syms.globals.get(*i).ok_or_else(not_found)?;
            let mut ctx = json!({ "ID": full_id, "OwnerID": owner_of(sh, n, g.addr), "Name": g.name, "Class": 2, "Address": g.addr, "Flags": 0 });
            type_fields(n, &syms, &syms.var_type(g.die_ref()), &mut ctx);
            Ok(ctx)
        }
        SymbolRef::Object(t, a) => {
            let mut ctx = json!({ "ID": full_id, "OwnerID": owner_of(sh, n, *a), "Class": 2, "Address": a, "Flags": 0 });
            type_fields(n, &syms, t, &mut ctx);
            Ok(ctx)
        }
        SymbolRef::Local(d) => {
            let frame = id.split_once('@').map(|x| x.1).unwrap_or_default();
            let scope = scope_for(sh, frame)?;
            let mut ctx = json!({ "ID": full_id, "OwnerID": frame, "Class": 1, "Flags": 0 });
            if let Some(name) = syms.die_name(*d) {
                ctx["Name"] = json!(name);
            }
            type_fields(n, &syms, &syms.var_type(*d), &mut ctx);
            let mut t = target(sh, n, &scope);
            let mut ev = Eval::new(&syms, &mut t);
            // An unavailable location (optimized out) leaves the value out.
            if let Ok(v) = ev.value_of(&sym) {
                match (&v.place, &v.bytes) {
                    (Place::Memory(a), None) => {
                        ctx["Class"] = json!(2);
                        ctx["Address"] = json!(a);
                    }
                    _ => {
                        if let Ok(b) = ev.load(&v) {
                            ctx["Value"] = json!(B64.encode(b));
                        }
                    }
                }
            }
            Ok(ctx)
        }
    }
}

pub fn symbol_children(sh: &Shared, full_id: &str) -> Res<Vec<String>> {
    let ((n, syms), id) = elf_of(sh, full_id)?;
    let Some(t) = type_of_id(&syms, id) else { return fail(ERR_SYM_NOT_FOUND, format!("Invalid symbol ID: {full_id}")) };
    let info = syms.type_info(&t);
    Ok(match info.class {
        TypeClass::Composite => syms.members(&t).iter().map(|m| elf_id(n, &format!("MB:{}", die_id(m.die)))).collect(),
        TypeClass::Enumeration => syms.enumerators(&t).iter().map(|(d, _, _)| elf_id(n, &format!("EN:{}:{}", die_id(*d), info.size))).collect(),
        _ => Vec::new(),
    })
}

// --- Expressions -----------------------------------------------------------------------------

fn entry(sh: &Shared, id: &str) -> Res<(String, String, Option<String>)> {
    let t = sh.exprs.lock().unwrap();
    let e = t.map.get(id).ok_or_else(|| (ERR_INV_CONTEXT, format!("Invalid expression ID: {id}")))?;
    Ok((e.parent.clone(), e.text.clone(), e.symbol.clone()))
}

/// Evaluates an expression entry. Returns the scope, the ELF that evaluated it and the value.
fn evaluate_entry(sh: &Shared, parent: &str, text: &str, symbol: &Option<String>) -> Res<(Scope, Elf, Value)> {
    let scope = scope_for(sh, parent)?;
    if let Some(sid) = symbol {
        let (elf, id) = elf_of(sh, sid)?;
        let s = parse_symbol(id).ok_or_else(|| (ERR_SYM_NOT_FOUND, format!("Invalid symbol ID: {sid}")))?;
        let mut t = target(sh, elf.0, &scope);
        let v = Eval::new(&elf.1, &mut t).value_of(&s).map_err(|e| (ERR_INV_EXPRESSION, e))?;
        return Ok((scope, elf, v));
    }
    evaluate_text(sh, parent, scope, text).map_err(|(e, _)| e)
}

/// Evaluates `text` in `scope` with the ELF files of `ctx`, then with the globals of the other
/// cores' files. A failure also carries the read that failed while the first file evaluated it,
/// if one did.
fn evaluate_text(sh: &Shared, ctx: &str, scope: Scope, text: &str) -> Result<(Scope, Elf, Value), ((i64, String), Option<String>)> {
    let mut first_err = None;
    let own = elfs_for(sh, ctx, scope.pc, text);
    for elf in own.iter().flatten() {
        let mut t = target(sh, elf.0, &scope);
        let r = Eval::new(&elf.1, &mut t).evaluate(text);
        match r {
            Ok(v) => return Ok((scope, elf.clone(), v)),
            Err(e) => {
                first_err.get_or_insert((e, t.read_error));
            }
        }
    }
    // Globals of another core's ELF files (e.g. an HSM variable in a live watch evaluated for a
    // TriCore core): evaluated in that core's global scope, so its memory view applies.
    let own_elfs: Vec<usize> = own.iter().flatten().map(|e| e.0).collect();
    for core in cores(&sh.handle).into_iter().filter(|c| c.idx != scope.core.idx) {
        let Some(set) = sh.symbols_for(&core.id) else { continue };
        let other = Scope { core, frame_id: None, frame_index: None, pc: None, regs: [None; PC_REGISTER as usize + 1] };
        for elf in set.ordered(None).into_iter().filter(|e| !own_elfs.contains(&e.0)) {
            let mut t = target(sh, elf.0, &other);
            if let Ok(v) = Eval::new(&elf.1, &mut t).evaluate(text) {
                return Ok((other, elf, v));
            }
        }
    }
    match (own, first_err) {
        (Err(e), None) => Err((e, None)),
        (_, first) => {
            let (e, read) = first.unwrap_or_default();
            Err(((ERR_INV_EXPRESSION, e), read))
        }
    }
}

/// Owner of a symbol of ELF `n`: the core context if the ELF belongs to exactly one core (its
/// memory view applies, e.g. HSM RAM), otherwise the chip.
/// The context a variable of ELF `n` at `addr` belongs to: for core-local memory the first core
/// configured to run the ELF, else the only core with the ELF loaded, else the chip.
fn owner_of(sh: &Shared, n: usize, addr: u32) -> String {
    if local_address(addr) {
        if let Some(&home) = sh.symbols.lock().unwrap().run_cores(n).first() {
            return core_id(home);
        }
    }
    let owners: Vec<String> = cores(&sh.handle)
        .into_iter()
        .filter(|c| sh.symbols_for(&c.id).is_some_and(|s| s.elfs.iter().any(|e| e.0 == n)))
        .map(|c| c.id)
        .collect();
    match owners.as_slice() {
        [one] => one.clone(),
        _ => CONTAINER.to_string(),
    }
}

fn expression_context(sh: &Shared, id: &str, parent: &str, text: &str, symbol: &Option<String>) -> Json {
    let mut ctx = json!({ "ID": id, "ParentID": parent, "Expression": text, "CanAssign": false, "HasFuncCall": false });
    if let Ok((scope, (n, syms), v)) = evaluate_entry(sh, parent, text, symbol) {
        let info = syms.type_info(&v.ty);
        ctx["CanAssign"] = json!(v.can_assign());
        ctx["Class"] = json!(if v.can_assign() { 2 } else { 1 });
        ctx["Type"] = json!(elf_id(n, &v.ty.to_id()));
        ctx["Size"] = json!(info.size);
        if let Some(sid) = symbol_id(n, &v, &scope) {
            ctx["SymbolID"] = json!(sid);
        }
    }
    ctx
}

fn create(sh: &Shared, parent: String, text: String) -> Res<Json> {
    let text = text.trim().to_string();
    // Syntax check: the expression must parse with the types of one of the ELF files.
    let pc = scope_for(sh, &parent).ok().and_then(|s| s.pc);
    let mut parsed = Err(String::new());
    for (_, syms) in elfs_for(sh, &parent, pc, &text)? {
        parsed = symbols::expr::parse(&text, &|name| Eval::<TcfTarget>::resolve_type(&syms, name)).map(|_| ());
        if parsed.is_ok() {
            break;
        }
    }
    parsed.map_err(|e| (ERR_INV_EXPRESSION, e))?;
    let id = sh.exprs.lock().unwrap().add(ExprEntry { parent: parent.clone(), text: text.clone(), symbol: None });
    Ok(expression_context(sh, &id, &parent, &text, &None))
}

/// Local variables of a frame (or of a suspended core's top frame).
fn children(sh: &Shared, parent: &str) -> Res<Vec<String>> {
    let scope = scope_for(sh, parent)?;
    let (Some(pc), Some(frame)) = (scope.pc, scope.frame_id.clone()) else { return Ok(Vec::new()) };
    let Some((n, syms)) = sh.symbols_for(parent).and_then(|s| s.for_addr(pc).cloned()) else { return Ok(Vec::new()) };
    let locals = syms.locals_at(pc);
    let mut table = sh.exprs.lock().unwrap();
    let mut ids = Vec::new();
    for (i, l) in locals.iter().enumerate() {
        // Shadowed by a later (inner) declaration.
        if locals[i + 1..].iter().any(|x| x.name == l.name) {
            continue;
        }
        ids.push(table.add(ExprEntry {
            parent: parent.to_string(),
            text: l.name.clone(),
            symbol: Some(elf_id(n, &format!("LV:{}@{frame}", die_id(l.die)))),
        }));
    }
    Ok(ids)
}

fn evaluate(sh: &Shared, id: &str) -> Res<(Vec<u8>, Json)> {
    let (parent, text, symbol) = entry(sh, id)?;
    let (scope, (n, syms), v) = evaluate_entry(sh, &parent, &text, &symbol)?;
    let info = syms.type_info(&v.ty);
    if info.size as usize > MAX_VALUE_SIZE {
        return fail(ERR_INV_DATA_SIZE, format!("value of {} bytes is too large", info.size));
    }
    let mut props = json!({ "Class": if v.can_assign() { 2 } else { 1 }, "Type": elf_id(n, &v.ty.to_id()), "BigEndian": false });
    if let Some(sid) = symbol_id(n, &v, &scope) {
        props["Symbol"] = json!(sid);
    }
    if let (Place::Memory(a), None) = (&v.place, &v.bytes) {
        props["Address"] = json!(a);
    }
    let bytes = if info.size == 0 {
        Vec::new()
    } else {
        let mut t = target(sh, n, &scope);
        Eval::new(&syms, &mut t).load(&v).map_err(|e| (ERR_INV_EXPRESSION, e))?
    };
    Ok((bytes, props))
}

fn assign(sh: &Shared, id: &str, data: &[u8]) -> Res<()> {
    let (parent, text, symbol) = entry(sh, id)?;
    let (scope, (n, syms), v) = evaluate_entry(sh, &parent, &text, &symbol)?;
    let write = {
        let mut t = target(sh, n, &scope);
        Eval::new(&syms, &mut t).assign(&v, data).map_err(|e| (ERR_INV_EXPRESSION, e))?
    };
    let idx = scope.core.idx;
    match write {
        Write::Memory(addr, bytes) => {
            write_checked(sh, &scope, n, addr, bytes).map_err(|e| (ERR_OTHER, e))?;
        }
        Write::Register(r, value) => {
            if scope.frame_index != Some(0) {
                return fail(ERR_OTHER, "Register variables can only be modified in the top stack frame");
            }
            let arch = scope.core.arch;
            let reg = if r == PC_REGISTER {
                arch.pc
            } else {
                arch.regs.iter().position(|x| x.dwarf.map(|d| d as u16) == Some(r)).ok_or_else(|| (ERR_OTHER, format!("No register for DWARF number {r}")))?
            };
            sh.handle
                .call(move |cx| ops::write_reg(cx, idx, reg, value).map_err(|e| e.to_string()))
                .unwrap_or_else(|| Err("server stopped".into()))
                .map_err(|e| (ERR_OTHER, e))?;
        }
    }
    sh.invalidate_stack(idx);
    Ok(())
}

/// Writes `bytes` at `addr` through the memory view of ELF `n` in `scope`. Returns the target
/// whose view was used.
fn write_checked(sh: &Shared, scope: &Scope, n: usize, addr: u32, bytes: Vec<u8>) -> Result<usize, String> {
    let idx = if local_address(addr) { local_core(sh, n, scope) } else { scope.core.idx };
    let expect = bytes.clone();
    sh.handle
        .call(move |cx| ops::write_memory_as(cx, idx, addr, &bytes).map_err(|e| e.to_string()))
        .unwrap_or_else(|| Err("server stopped".into()))?;
    // The debug interface reports no error for writes the chip drops (e.g. memory
    // write-protected for the debugger by the application's SMPU regions): verify RAM
    // writes while halted. Peripheral registers and running cores may legitimately differ.
    if scope.core.suspended() && addr < PERIPHERAL_BASE {
        let back = read_mem(sh, idx, addr, expect.len())?;
        if back != expect {
            return Err(format!(
                "The target ignored the write to 0x{addr:08x}: the memory is write-protected for the debugger (e.g. by the application's SMPU configuration)"
            ));
        }
    }
    Ok(idx)
}

// --- Global expressions for the REST/MCP frontend --------------------------------------------

/// Why [`evaluate_global`] failed.
#[derive(Clone, Debug, PartialEq)]
pub enum GlobalError {
    /// No symbols are loaded for the context.
    NoSymbols,
    /// The context names no core, or there are no cores.
    NoTarget(String),
    /// Reading target memory failed while evaluating (e.g. following a pointer).
    Memory(String),
    /// The expression does not parse or names nothing.
    Expression(String),
}

/// An expression evaluated without a stack frame, as a live watch evaluates it while the core
/// runs: globals, statics, constants and what they lead to.
///
/// The REST/MCP variable operations use this rather than an evaluator of their own, so a name
/// means the same variable there as in VS Code, read through the same memory view (core-local
/// RAM through the core that runs the ELF).
pub struct GlobalValue {
    pub elf: Elf,
    pub value: Value,
    scope: Scope,
}

/// The core a global expression of `ctx` is evaluated for: the one `ctx` names, else the first
/// core with symbols (the set `symbols_for("")` picks), else the first examined one.
fn global_scope(sh: &Shared, ctx: &str) -> Result<Scope, GlobalError> {
    let all = cores(&sh.handle);
    let pos = match crate::contexts::owning_core(ctx) {
        Some(id) => all.iter().position(|c| c.id == id).ok_or_else(|| GlobalError::NoTarget(format!("unknown context {ctx}")))?,
        None => {
            let store = sh.symbols.lock().unwrap();
            let mut with_syms: Vec<usize> = (0..all.len()).filter(|&i| store.set(&all[i].id).is_some()).collect();
            with_syms.sort_by_key(|&i| all[i].id.rsplit_once(".C").and_then(|(_, n)| n.parse::<u32>().ok()));
            with_syms.first().copied().or_else(|| all.iter().position(|c| c.examined)).unwrap_or(0)
        }
    };
    let core = all.into_iter().nth(pos).ok_or_else(|| GlobalError::NoTarget("no targets (run `init` first)".into()))?;
    Ok(Scope { core, frame_id: None, frame_index: None, pc: None, regs: [None; PC_REGISTER as usize + 1] })
}

/// Evaluates `text` with the symbols of `ctx` (a core context, or empty for the first core that
/// has symbols), without a stack frame.
pub fn evaluate_global(sh: &Shared, ctx: &str, text: &str) -> Result<GlobalValue, GlobalError> {
    let scope = global_scope(sh, ctx)?;
    let ctx = scope.core.id.clone();
    match evaluate_text(sh, &ctx, scope, text) {
        Ok((scope, elf, value)) => Ok(GlobalValue { elf, value, scope }),
        Err(((ERR_SYM_NOT_FOUND, _), _)) => Err(GlobalError::NoSymbols),
        Err((_, Some(read))) => Err(GlobalError::Memory(read)),
        Err(((_, e), None)) => Err(GlobalError::Expression(e)),
    }
}

impl GlobalValue {
    /// Target index of the core the value was evaluated for.
    pub fn core(&self) -> usize {
        self.scope.core.idx
    }

    /// Reads `len` bytes at `addr` through the value's memory view.
    pub fn read(&self, sh: &Shared, addr: u32, len: usize) -> Result<Vec<u8>, String> {
        target(sh, self.elf.0, &self.scope).read(addr, len)
    }

    /// Stores `data` (the size of the value's type) into the variable, merging a bit field into
    /// its storage unit. Returns the target whose memory view was written and the range.
    pub fn assign(&self, sh: &Shared, data: &[u8]) -> Result<(usize, u32, u32), GlobalError> {
        let mut t = target(sh, self.elf.0, &self.scope);
        let write = Eval::new(&self.elf.1, &mut t).assign(&self.value, data);
        let write = match (write, t.read_error) {
            (Ok(w), _) => w,
            (Err(_), Some(read)) => return Err(GlobalError::Memory(read)),
            (Err(e), None) => return Err(GlobalError::Expression(e)),
        };
        match write {
            Write::Memory(addr, bytes) => {
                let len = bytes.len() as u32;
                let idx = write_checked(sh, &self.scope, self.elf.0, addr, bytes).map_err(GlobalError::Memory)?;
                Ok((idx, addr, len))
            }
            // Without a stack frame nothing lives in a register.
            Write::Register(..) => Err(GlobalError::Expression("a register variable needs a stack frame".into())),
        }
    }
}

/// Handles Expressions.* and the variable part of Symbols.*; returns the reply fields.
pub fn dispatch(sh: &Shared, service: &str, command: &str, args: &[String]) -> Res<Vec<String>> {
    match (service, command) {
        ("Symbols", "getContext") => Ok(vec![String::new(), symbol_context(sh, &arg_str(args, 0)?)?.to_string()]),
        ("Symbols", "getChildren") => Ok(vec![String::new(), json!(symbol_children(sh, &arg_str(args, 0)?)?).to_string()]),
        ("Expressions", "create") => Ok(vec![String::new(), create(sh, arg_str(args, 0)?, arg_str(args, 2)?)?.to_string()]),
        ("Expressions", "getContext") => {
            let id = arg_str(args, 0)?;
            let (parent, text, symbol) = entry(sh, &id)?;
            Ok(vec![String::new(), expression_context(sh, &id, &parent, &text, &symbol).to_string()])
        }
        ("Expressions", "getChildren") => Ok(vec![String::new(), json!(children(sh, &arg_str(args, 0)?)?).to_string()]),
        ("Expressions", "evaluate") => {
            let (bytes, props) = evaluate(sh, &arg_str(args, 0)?)?;
            Ok(vec![json!(B64.encode(bytes)).to_string(), String::new(), props.to_string()])
        }
        ("Expressions", "assign") => {
            let id = arg_str(args, 0)?;
            let data = B64.decode(arg_str(args, 1)?).map_err(|e| (ERR_JSON_SYNTAX, format!("base64: {e}")))?;
            assign(sh, &id, &data)?;
            Ok(vec![String::new()])
        }
        ("Expressions", "dispose") => {
            let id = arg_str(args, 0)?;
            let mut t = sh.exprs.lock().unwrap();
            t.map.remove(&id);
            t.order.retain(|x| *x != id);
            Ok(vec![String::new()])
        }
        _ => fail(ERR_INV_COMMAND, format!("Unsupported command: {service}.{command}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_ids() {
        assert_eq!(parse_symbol("GV:12"), Some(SymbolRef::Global(12)));
        assert_eq!(parse_symbol("FN:3"), Some(SymbolRef::Function(3)));
        assert_eq!(parse_symbol("LV:21.3803@S0.D0.C0.F2"), Some(SymbolRef::Local(DieRef { unit: 21, off: 0x3803 })));
        assert_eq!(
            parse_symbol("OB:70013954:T0.24d"),
            Some(SymbolRef::Object(TypeId::Die(DieRef { unit: 0, off: 0x24d }), 0x7001_3954))
        );
        assert_eq!(parse_symbol("X"), None);
    }
}
