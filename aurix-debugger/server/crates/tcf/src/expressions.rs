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
    /// Whose core-local memory the scope's reads of the local alias go to.
    rule: LocalRule,
}

/// How the core whose core-local memory (`0xD000_0000` DSPR / `0xC000_0000` PSPR alias) an
/// expression means is chosen.
#[derive(Clone)]
enum LocalRule {
    /// A debugger view (TCF): the scope's core, or the one that runs the ELF (see
    /// [`local_core`]). The scope is what the user picked, so it is a choice, not a guess.
    Scope,
    /// The REST/MCP/DAQ frontends, which have no scope: the core an `@N` names, else the only
    /// core the ELF is assigned to, else none (see [`home_core`]). The TriCore cores as (core ID,
    /// target index).
    Home { explicit: Option<u32>, tricores: Vec<(u32, usize)> },
}

impl Scope {
    /// A core's scope without a stack frame (globals only).
    fn global(core: CoreInfo, rule: LocalRule) -> Scope {
        Scope { core, frame_id: None, frame_index: None, pc: None, regs: [None; PC_REGISTER as usize + 1], rule }
    }
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
    let mut s = Scope::global(core, LocalRule::Scope);
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
    /// Target index for the core-local memory (see [`LocalRule`]); `None` when no core is
    /// determined, so such a read fails rather than picking one.
    local: Option<usize>,
    /// ELF the expression is evaluated with.
    elf: usize,
    pc: Option<u32>,
    regs: [Option<u32>; PC_REGISTER as usize + 1],
    /// The last read that failed, so a caller can tell a memory fault from a bad name.
    read_error: Option<ReadFail>,
}

/// A read that failed while evaluating.
#[derive(Clone, Debug, Default)]
struct ReadFail {
    message: String,
    /// The read was of core-local memory that no core is determined for: (ELF, address).
    per_core: Option<(usize, u32)>,
}

impl Target for TcfTarget<'_> {
    fn pc(&self) -> Option<u32> {
        self.pc
    }
    fn reg(&self, r: u16) -> Option<u32> {
        self.regs.get(r as usize).copied().flatten()
    }
    fn read(&mut self, addr: u32, len: usize) -> Result<Vec<u8>, String> {
        let t = if local_address(addr) { self.local } else { Some(self.core) };
        let r = match t {
            Some(t) => read_mem(self.sh, t, addr, len),
            None => Err(format!("0x{addr:08x} is core-local RAM and no core is named for it")),
        };
        if let Err(e) = &r {
            self.read_error = Some(ReadFail { message: e.clone(), per_core: t.is_none().then_some((self.elf, addr)) });
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
    let local = match &s.rule {
        LocalRule::Scope => Some(local_core(sh, elf, s)),
        LocalRule::Home { explicit, tricores } => {
            let ids: Vec<u32> = tricores.iter().map(|t| t.0).collect();
            let home = home_core(sh, &ids, elf, *explicit).ok();
            home.and_then(|c| tricores.iter().find(|t| t.0 == c)).map(|t| t.1)
        }
    };
    TcfTarget { sh, core: s.core.idx, local, elf, pc: s.pc, regs: s.regs, read_error: None }
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
/// if one did, or a later file's read of core-local memory that no core is named for.
fn evaluate_text(sh: &Shared, ctx: &str, scope: Scope, text: &str) -> Result<(Scope, Elf, Value), ((i64, String), Option<ReadFail>)> {
    let mut first_err: Option<(String, Option<ReadFail>)> = None;
    let own = elfs_for(sh, ctx, scope.pc, text);
    for elf in own.iter().flatten() {
        let mut t = target(sh, elf.0, &scope);
        let r = Eval::new(&elf.1, &mut t).evaluate(text);
        match r {
            Ok(v) => return Ok((scope, elf.clone(), v)),
            Err(e) => {
                // Core-local memory that no core is named for says more than "not found" in
                // another file would.
                let per_core = |r: &Option<ReadFail>| r.as_ref().is_some_and(|r| r.per_core.is_some());
                if first_err.as_ref().is_none_or(|f| per_core(&t.read_error) && !per_core(&f.1)) {
                    first_err = Some((e, t.read_error));
                }
            }
        }
    }
    // Globals of another core's ELF files (e.g. an HSM variable in a live watch evaluated for a
    // TriCore core): evaluated in that core's global scope, so its memory view applies.
    let own_elfs: Vec<usize> = own.iter().flatten().map(|e| e.0).collect();
    for core in cores(&sh.handle).into_iter().filter(|c| c.idx != scope.core.idx) {
        let Some(set) = sh.symbols_for(&core.id) else { continue };
        let other = Scope::global(core, scope.rule.clone());
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
    let idx = if local_address(addr) { target(sh, n, scope).local } else { Some(scope.core.idx) };
    let idx = idx.ok_or_else(|| format!("0x{addr:08x} is core-local RAM and no core is named for it"))?;
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
    /// The context or an `@N` names no core, or there are no cores.
    NoTarget(String),
    /// Reading target memory failed while evaluating (e.g. following a pointer).
    Memory(String),
    /// The expression does not parse or names nothing.
    Expression(String),
    /// The expression needs core-local RAM and names no core while several could be meant, or
    /// it names a core other than the request's target.
    Core(String),
}

/// The core a trailing `@N` or `@hsm` of an expression names (see [`split_core`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreSel {
    /// The TriCore core with core ID `N`: whose core-local RAM the expression means.
    Tricore(u32),
    /// The HSM: its symbols and memory view.
    Hsm,
}

impl std::fmt::Display for CoreSel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CoreSel::Tricore(n) => write!(f, "@{n}"),
            CoreSel::Hsm => f.write_str("@hsm"),
        }
    }
}

/// Splits the core suffix off an expression: `CRASH_state@2`, `${E0/GV:12}@1`, `g_key@hsm`, as
/// a VS Code live watch writes them. `@` cannot occur in a C expression, so a trailing `@N` or
/// `@hsm` is never part of one (the `@` inside a `${...}` frame-local ID is followed by `S`).
pub fn split_core(text: &str) -> (&str, Option<CoreSel>) {
    let text = text.trim();
    let Some((head, tail)) = text.rsplit_once('@') else { return (text, None) };
    let (head, tail) = (head.trim_end(), tail.trim());
    let sel = if tail.eq_ignore_ascii_case("hsm") {
        CoreSel::Hsm
    } else if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) {
        // Too many digits for a core ID is still a core that does not exist.
        CoreSel::Tricore(tail.parse().unwrap_or(u32::MAX))
    } else {
        return (text, None);
    };
    if head.is_empty() {
        return (text, None);
    }
    (head, Some(sel))
}

/// The TriCore core (core ID) whose core-local RAM a REST/MCP/DAQ expression of ELF `elf` means:
/// the one an `@N` names (`explicit`), else the only core the ELF is assigned to (`symbolFiles`
/// `path@N`, `load_symbols` `path@N`). Otherwise the cores to name one of: the ELF's if it is
/// assigned to several, else every TriCore core of `tricores` (core IDs), not the HSM.
///
/// Unlike a live watch there is no scope whose core could stand in: picking one would read
/// another core's RAM without a word, which is worse than refusing.
pub fn home_core(sh: &Shared, tricores: &[u32], elf: usize, explicit: Option<u32>) -> Result<u32, Vec<u32>> {
    if let Some(n) = explicit {
        return Ok(n);
    }
    let mut runs = sh.symbols.lock().unwrap().run_cores(elf).to_vec();
    match runs.as_slice() {
        [one] if tricores.contains(one) => Ok(*one),
        [_, _, ..] => {
            runs.sort_unstable();
            runs.dedup();
            Err(runs)
        }
        // Not assigned, or to a core this chip does not have.
        _ => {
            let mut all = tricores.to_vec();
            all.sort_unstable();
            Err(all)
        }
    }
}

/// The refusal of an expression that needs core-local RAM at `addr` of ELF `elf` without a core
/// named, listing what to write instead. `reads`: the RAM is read on the way (`*p`), rather than
/// being where the value is.
fn per_core_error(sh: &Shared, elf: usize, text: &str, addr: u32, candidates: &[u32], reads: bool) -> GlobalError {
    let file = sh.elf(elf).and_then(|s| s.path.file_name().map(|f| f.to_string_lossy().into_owned())).unwrap_or_else(|| "its symbol file".into());
    let runs = sh.symbols.lock().unwrap().run_cores(elf).len() > 1;
    let list = |v: &[String], last: &str| match v {
        [] => String::new(),
        [one] => one.clone(),
        [head @ .., tail] => format!("{} {last} {tail}", head.join(", ")),
    };
    let cores: Vec<String> = candidates.iter().map(u32::to_string).collect();
    let options: Vec<String> = candidates.iter().map(|c| format!("`{text}@{c}`")).collect();
    let how = if runs { format!(" (it runs on cores {})", list(&cores, "and")) } else { String::new() };
    let what = if reads { "reads" } else { "is in" };
    let fix = if options.is_empty() { "; there is no TriCore core to read it through".to_string() } else { format!("; use {}", list(&options, "or")) };
    GlobalError::Core(format!("`{text}` {what} core-local RAM (0x{addr:08X}) and {file} is not assigned to a single core{how}{fix}"))
}

/// An expression evaluated without a stack frame, as a live watch evaluates it while the core
/// runs: globals, statics, constants and what they lead to.
///
/// The REST/MCP variable operations and DAQ points use this rather than an evaluator of their
/// own, so a name means the same variable there as in VS Code. Core-local RAM is the one of the
/// core [`home_core`] picks; a value stored there for no determined core still evaluates (its
/// type and address are known), and [`Self::per_core`] tells the caller to pick one.
pub struct GlobalValue {
    pub elf: Elf,
    pub value: Value,
    scope: Scope,
    /// The expression without its core suffix, and the suffix.
    text: String,
    sel: Option<CoreSel>,
}

/// The core a global expression of `ctx` is evaluated for: the one `ctx` names, else the first
/// core with symbols (the set `symbols_for("")` picks), else the first examined one.
fn global_scope(sh: &Shared, all: &[CoreInfo], ctx: &str, rule: LocalRule) -> Result<Scope, GlobalError> {
    let pos = match crate::contexts::owning_core(ctx) {
        Some(id) => all.iter().position(|c| c.id == id).ok_or_else(|| GlobalError::NoTarget(format!("unknown context {ctx}")))?,
        None => {
            let store = sh.symbols.lock().unwrap();
            let mut with_syms: Vec<usize> = (0..all.len()).filter(|&i| store.set(&all[i].id).is_some()).collect();
            with_syms.sort_by_key(|&i| all[i].id.rsplit_once(".C").and_then(|(_, n)| n.parse::<u32>().ok()));
            with_syms.first().copied().or_else(|| all.iter().position(|c| c.examined)).unwrap_or(0)
        }
    };
    let core = all.get(pos).cloned().ok_or_else(|| GlobalError::NoTarget("no targets (run `init` first)".into()))?;
    Ok(Scope::global(core, rule))
}

/// Evaluates `text` with the symbols of `ctx` (a core context, or empty for the first core that
/// has symbols), without a stack frame.
///
/// A trailing `@N` (see [`split_core`]) names the TriCore core whose core-local RAM is meant and
/// evaluates with that core's symbols, as `expr@N` does in a live watch; `@hsm` evaluates for the
/// HSM. A `ctx` naming another core than the suffix is an error rather than a silent choice.
pub fn evaluate_global(sh: &Shared, ctx: &str, text: &str) -> Result<GlobalValue, GlobalError> {
    let (expr, sel) = split_core(text);
    let all = cores(&sh.handle);
    let tricores: Vec<(u32, usize)> = all.iter().filter(|c| c.tricore()).map(|c| (c.coreid, c.idx)).collect();
    let named = match sel {
        None => None,
        Some(CoreSel::Tricore(n)) => Some(all.iter().find(|c| c.tricore() && c.coreid == n).ok_or_else(|| {
            let have: Vec<String> = tricores.iter().map(|t| t.0.to_string()).collect();
            GlobalError::NoTarget(format!("`{text}`: there is no TriCore core {n} (the cores are {})", have.join(", ")))
        })?),
        Some(CoreSel::Hsm) => Some(all.iter().find(|c| c.arch.name == "cortex_m").ok_or_else(|| GlobalError::NoTarget(format!("`{text}`: there is no HSM")))?),
    };
    let ctx = match (named, crate::contexts::owning_core(ctx)) {
        (Some(c), Some(t)) if c.id != t => {
            let target = all.iter().find(|x| x.id == t).map_or(t.as_str(), |x| x.name.as_str());
            return Err(GlobalError::Core(format!("`{text}` names {}, but the request's target is {target}", c.name)));
        }
        (Some(c), _) => c.id.clone(),
        (None, _) => ctx.to_string(),
    };
    let explicit = match sel {
        Some(CoreSel::Tricore(n)) => Some(n),
        _ => None,
    };
    let ids: Vec<u32> = tricores.iter().map(|t| t.0).collect();
    let scope = global_scope(sh, &all, &ctx, LocalRule::Home { explicit, tricores })?;
    let ctx = scope.core.id.clone();
    match evaluate_text(sh, &ctx, scope, expr) {
        Ok((scope, elf, value)) => Ok(GlobalValue { elf, value, scope, text: expr.to_string(), sel }),
        Err(((ERR_SYM_NOT_FOUND, _), _)) => Err(GlobalError::NoSymbols),
        Err((_, Some(ReadFail { per_core: Some((elf, addr)), .. }))) => {
            let candidates = home_core(sh, &ids, elf, None).err().unwrap_or_default();
            Err(per_core_error(sh, elf, expr, addr, &candidates, true))
        }
        Err((_, Some(read))) => Err(GlobalError::Memory(read.message)),
        Err(((_, e), None)) => Err(GlobalError::Expression(e)),
    }
}

impl GlobalValue {
    /// Target index of the core the value was evaluated for.
    pub fn core(&self) -> usize {
        self.scope.core.idx
    }

    /// The core the value was evaluated for: whose symbols, and whose view of memory other than
    /// core-local RAM (HSM-private memory, MCS program RAM).
    pub fn scope_core(&self) -> &CoreInfo {
        &self.scope.core
    }

    /// The expression without its core suffix.
    pub fn expression(&self) -> &str {
        &self.text
    }

    /// The expression's core suffix (`@2`, `@hsm`), if it has one.
    pub fn suffix(&self) -> Option<CoreSel> {
        self.sel
    }

    /// Where the value is stored, if at a fixed address: the object, or a bit field's storage
    /// unit.
    pub fn stored_at(&self) -> Option<u32> {
        match (&self.value.place, &self.value.bytes) {
            (Place::Memory(a), None) | (Place::Bits { addr: a, .. }, None) => Some(*a),
            _ => None,
        }
    }

    /// The TriCore core whose core-local RAM the value's ELF means (see [`home_core`]), or the
    /// cores to name one of.
    fn home(&self, sh: &Shared) -> Result<u32, Vec<u32>> {
        match &self.scope.rule {
            LocalRule::Home { explicit, tricores } => {
                let ids: Vec<u32> = tricores.iter().map(|t| t.0).collect();
                home_core(sh, &ids, self.elf.0, *explicit)
            }
            // Only REST values are made; a scope always has a core.
            LocalRule::Scope => Ok(self.scope.core.coreid),
        }
    }

    /// The cores to choose from when the value is stored in core-local RAM and no core is
    /// determined for it (an ELF on several cores, or on none); `None` when its memory is.
    pub fn per_core(&self, sh: &Shared) -> Option<Vec<u32>> {
        self.stored_at().filter(|&a| local_address(a)).and_then(|_| self.home(sh).err())
    }

    /// The system-bus address of `addr` as the value means it: core-local RAM translated to the
    /// global address of its core's DSPR/PSPR, anything else unchanged. Fails with the cores to
    /// choose from when no core is determined.
    pub fn global_address(&self, sh: &Shared, addr: u32) -> Result<u32, GlobalError> {
        if !local_address(addr) {
            return Ok(addr);
        }
        match self.home(sh) {
            Ok(core) => Ok(aurix_core::tricore::global_address(core, addr)),
            Err(candidates) => Err(per_core_error(sh, self.elf.0, &self.text, addr, &candidates, Some(addr) != self.stored_at())),
        }
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
            (Err(_), Some(read)) => return Err(GlobalError::Memory(read.message)),
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

    #[test]
    fn core_suffixes() {
        assert_eq!(split_core("CRASH_state@2"), ("CRASH_state", Some(CoreSel::Tricore(2))));
        assert_eq!(split_core(" g_s.a @ 1 "), ("g_s.a", Some(CoreSel::Tricore(1))));
        assert_eq!(split_core("${E0/GV:12}@3"), ("${E0/GV:12}", Some(CoreSel::Tricore(3))));
        assert_eq!(split_core("g_key@HSM"), ("g_key", Some(CoreSel::Hsm)));
        assert_eq!(split_core("x@99999999999"), ("x", Some(CoreSel::Tricore(u32::MAX))), "a core that cannot exist");
        // A frame local's ID has an `@` of its own, which is no core.
        assert_eq!(split_core("${E0/LV:21.3803@S0.D0.C0.F2}"), ("${E0/LV:21.3803@S0.D0.C0.F2}", None));
        assert_eq!(split_core("${E0/LV:21.3803@S0.D0.C0.F2}@1"), ("${E0/LV:21.3803@S0.D0.C0.F2}", Some(CoreSel::Tricore(1))));
        for plain in ["g_x", "a@b", "@2", "x@", "x@-1"] {
            assert_eq!(split_core(plain), (plain, None), "{plain}");
        }
        assert_eq!(CoreSel::Tricore(2).to_string(), "@2");
        assert_eq!(CoreSel::Hsm.to_string(), "@hsm");
    }
}
