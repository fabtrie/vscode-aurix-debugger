//! TCF Expressions service and the variable/type part of the Symbols service.
//!
//! Symbol IDs:
//! - `FN:<i>` function, `GV:<i>` global variable
//! - `LV:<unit>.<die>@<frame>` local variable/parameter in a stack frame
//! - `OB:<addr>:<type>` object of a type at an address (e.g. `s.member`)
//! - `MB:<unit>.<die>` struct/union member, `EN:<unit>.<die>:<size>` enumerator
//! - types: see [`TypeId::to_id`]
//!
//! Expressions are evaluated in the scope of a stack frame (locals, then
//! globals) or of a core (globals only when it is running, which is what
//! live watch uses).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::{json, Value as Json};

use aurix_core::arch::{REGS, REG_PC};
use aurix_core::ops;
use aurix_core::unwind::Frame;
use symbols::expr::{Eval, Place, SymbolRef, Target, Value, Write};
use symbols::types::{TypeClass, TypeId};
use symbols::{DieRef, Symbols, PC_REGISTER};

use crate::contexts::{cores, find_core, CoreInfo, CONTAINER};
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
    let f = crate::stack::unwind_core(&sh.handle, c.idx).map_err(|e| (ERR_OTHER, e))?;
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

fn read_mem(sh: &Shared, addr: u32, len: usize) -> Result<Vec<u8>, String> {
    if len > MAX_VALUE_SIZE * 16 {
        return Err(format!("{len} bytes are too many to read"));
    }
    sh.handle
        .call(move |cx| ops::read_memory(cx, addr, len).map_err(|e| e.to_string()))
        .unwrap_or_else(|| Err("server stopped".into()))
}

struct TcfTarget<'a> {
    sh: &'a Shared,
    pc: Option<u32>,
    regs: [Option<u32>; PC_REGISTER as usize + 1],
}

impl Target for TcfTarget<'_> {
    fn pc(&self) -> Option<u32> {
        self.pc
    }
    fn reg(&self, r: u16) -> Option<u32> {
        self.regs.get(r as usize).copied().flatten()
    }
    fn read(&mut self, addr: u32, len: usize) -> Result<Vec<u8>, String> {
        read_mem(self.sh, addr, len)
    }
    fn symbol(&mut self, id: &str) -> Option<SymbolRef> {
        parse_symbol(id)
    }
}

fn target<'a>(sh: &'a Shared, s: &Scope) -> TcfTarget<'a> {
    TcfTarget { sh, pc: s.pc, regs: s.regs }
}

fn syms(sh: &Shared) -> Res<Arc<Symbols>> {
    sh.active_symbols().ok_or((ERR_SYM_NOT_FOUND, "No symbols loaded".into()))
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

fn symbol_id(v: &Value, s: &Scope) -> Option<String> {
    match &v.symbol {
        Some(SymbolRef::Local(d)) => s.frame_id.as_ref().map(|f| format!("LV:{}@{f}", die_id(*d))),
        Some(SymbolRef::Global(i)) => Some(format!("GV:{i}")),
        Some(SymbolRef::Function(i)) => Some(format!("FN:{i}")),
        Some(SymbolRef::Object(t, a)) => Some(format!("OB:{a:x}:{}", t.to_id())),
        None => match (&v.place, &v.bytes) {
            (Place::Memory(a), None) => Some(format!("OB:{a:x}:{}", v.ty.to_id())),
            _ => None,
        },
    }
}

/// The type an ID stands for, or of the symbol it names.
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

fn type_fields(syms: &Symbols, t: &TypeId, ctx: &mut Json) {
    let info = syms.type_info(t);
    ctx["TypeID"] = json!(t.to_id());
    ctx["TypeClass"] = json!(info.class as u8);
    ctx["Size"] = json!(info.size);
}

pub fn symbol_context(sh: &Shared, id: &str) -> Res<Json> {
    let syms = syms(sh)?;
    let not_found = || (ERR_SYM_NOT_FOUND, format!("Invalid symbol ID: {id}"));

    if let Some(t) = TypeId::parse(id) {
        let info = syms.type_info(&t);
        let mut ctx = json!({ "ID": id, "OwnerID": CONTAINER, "Class": 4, "TypeClass": info.class as u8,
                              "Size": info.size, "TypeID": id, "Flags": 0 });
        if let Some(n) = &info.name {
            ctx["Name"] = json!(n);
        }
        if let Some(b) = &info.base {
            ctx["BaseTypeID"] = json!(b.to_id());
        }
        if let Some(l) = info.length {
            ctx["Length"] = json!(l);
        }
        return Ok(ctx);
    }
    if let Some(m) = id.strip_prefix("MB:").and_then(parse_die) {
        let mi = syms.member_info(m).ok_or_else(not_found)?;
        let mut ctx = json!({ "ID": id, "OwnerID": CONTAINER, "Class": 2, "Offset": mi.offset, "Flags": 0 });
        type_fields(&syms, &mi.ty, &mut ctx);
        ctx["Size"] = json!(mi.size);
        if let Some(n) = mi.name {
            ctx["Name"] = json!(n);
        }
        return Ok(ctx);
    }
    if let Some(rest) = id.strip_prefix("EN:") {
        let (d, size) = rest.rsplit_once(':').ok_or_else(not_found)?;
        let d = parse_die(d).ok_or_else(not_found)?;
        let size: usize = size.parse().map_err(|_| not_found())?;
        let (name, value) = syms.enumerator(d).ok_or_else(not_found)?;
        let bytes = &value.to_le_bytes()[..size.clamp(1, 8)];
        return Ok(json!({ "ID": id, "OwnerID": CONTAINER, "Class": 1, "Name": name, "Size": size,
                          "TypeClass": TypeClass::Enumeration as u8, "Value": B64.encode(bytes), "Flags": 0 }));
    }

    let sym = parse_symbol(id).ok_or_else(not_found)?;
    match &sym {
        SymbolRef::Function(i) => {
            let f = syms.functions.get(*i).ok_or_else(not_found)?;
            Ok(json!({ "ID": id, "OwnerID": CONTAINER, "Name": f.name, "Class": 3, "TypeClass": TypeClass::Function as u8,
                       "TypeID": TypeId::Die(f.die_ref()).to_id(), "Address": f.low, "Size": f.high - f.low, "Flags": 0 }))
        }
        SymbolRef::Global(i) => {
            let g = syms.globals.get(*i).ok_or_else(not_found)?;
            let mut ctx = json!({ "ID": id, "OwnerID": CONTAINER, "Name": g.name, "Class": 2, "Address": g.addr, "Flags": 0 });
            type_fields(&syms, &syms.var_type(g.die_ref()), &mut ctx);
            Ok(ctx)
        }
        SymbolRef::Object(t, a) => {
            let mut ctx = json!({ "ID": id, "OwnerID": CONTAINER, "Class": 2, "Address": a, "Flags": 0 });
            type_fields(&syms, t, &mut ctx);
            Ok(ctx)
        }
        SymbolRef::Local(d) => {
            let frame = id.split_once('@').map(|x| x.1).unwrap_or_default();
            let scope = scope_for(sh, frame)?;
            let mut ctx = json!({ "ID": id, "OwnerID": frame, "Class": 1, "Flags": 0 });
            if let Some(n) = syms.die_name(*d) {
                ctx["Name"] = json!(n);
            }
            type_fields(&syms, &syms.var_type(*d), &mut ctx);
            let mut t = target(sh, &scope);
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

pub fn symbol_children(sh: &Shared, id: &str) -> Res<Vec<String>> {
    let syms = syms(sh)?;
    let Some(t) = type_of_id(&syms, id) else { return fail(ERR_SYM_NOT_FOUND, format!("Invalid symbol ID: {id}")) };
    let info = syms.type_info(&t);
    Ok(match info.class {
        TypeClass::Composite => syms.members(&t).iter().map(|m| format!("MB:{}", die_id(m.die))).collect(),
        TypeClass::Enumeration => syms.enumerators(&t).iter().map(|(d, _, _)| format!("EN:{}:{}", die_id(*d), info.size)).collect(),
        _ => Vec::new(),
    })
}

// --- Expressions -----------------------------------------------------------------------------

fn entry(sh: &Shared, id: &str) -> Res<(String, String, Option<String>)> {
    let t = sh.exprs.lock().unwrap();
    let e = t.map.get(id).ok_or_else(|| (ERR_INV_CONTEXT, format!("Invalid expression ID: {id}")))?;
    Ok((e.parent.clone(), e.text.clone(), e.symbol.clone()))
}

fn evaluate_entry(sh: &Shared, syms: &Symbols, parent: &str, text: &str, symbol: &Option<String>) -> Res<(Scope, Value)> {
    let scope = scope_for(sh, parent)?;
    let mut t = target(sh, &scope);
    let mut ev = Eval::new(syms, &mut t);
    let v = match symbol {
        Some(sid) => {
            let s = parse_symbol(sid).ok_or_else(|| (ERR_SYM_NOT_FOUND, format!("Invalid symbol ID: {sid}")))?;
            ev.value_of(&s)
        }
        None => ev.evaluate(text),
    }
    .map_err(|e| (ERR_INV_EXPRESSION, e))?;
    Ok((scope, v))
}

fn expression_context(sh: &Shared, syms: &Symbols, id: &str, parent: &str, text: &str, symbol: &Option<String>) -> Json {
    let mut ctx = json!({ "ID": id, "ParentID": parent, "Expression": text, "CanAssign": false, "HasFuncCall": false });
    if let Ok((scope, v)) = evaluate_entry(sh, syms, parent, text, symbol) {
        let info = syms.type_info(&v.ty);
        ctx["CanAssign"] = json!(v.can_assign());
        ctx["Class"] = json!(if v.can_assign() { 2 } else { 1 });
        ctx["Type"] = json!(v.ty.to_id());
        ctx["Size"] = json!(info.size);
        if let Some(sid) = symbol_id(&v, &scope) {
            ctx["SymbolID"] = json!(sid);
        }
    }
    ctx
}

fn create(sh: &Shared, parent: String, text: String) -> Res<Json> {
    let syms = syms(sh)?;
    let text = text.trim().to_string();
    symbols::expr::parse(&text, &|n| Eval::<TcfTarget>::resolve_type(&syms, n)).map_err(|e| (ERR_INV_EXPRESSION, e))?;
    let id = sh.exprs.lock().unwrap().add(ExprEntry { parent: parent.clone(), text: text.clone(), symbol: None });
    Ok(expression_context(sh, &syms, &id, &parent, &text, &None))
}

/// Local variables of a frame (or of a suspended core's top frame).
fn children(sh: &Shared, parent: &str) -> Res<Vec<String>> {
    let syms = syms(sh)?;
    let scope = scope_for(sh, parent)?;
    let (Some(pc), Some(frame)) = (scope.pc, scope.frame_id.clone()) else { return Ok(Vec::new()) };
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
            symbol: Some(format!("LV:{}@{frame}", die_id(l.die))),
        }));
    }
    Ok(ids)
}

fn evaluate(sh: &Shared, id: &str) -> Res<(Vec<u8>, Json)> {
    let syms = syms(sh)?;
    let (parent, text, symbol) = entry(sh, id)?;
    let (scope, v) = evaluate_entry(sh, &syms, &parent, &text, &symbol)?;
    let info = syms.type_info(&v.ty);
    if info.size as usize > MAX_VALUE_SIZE {
        return fail(ERR_INV_DATA_SIZE, format!("value of {} bytes is too large", info.size));
    }
    let mut props = json!({ "Class": if v.can_assign() { 2 } else { 1 }, "Type": v.ty.to_id(), "BigEndian": false });
    if let Some(sid) = symbol_id(&v, &scope) {
        props["Symbol"] = json!(sid);
    }
    if let (Place::Memory(a), None) = (&v.place, &v.bytes) {
        props["Address"] = json!(a);
    }
    let bytes = if info.size == 0 {
        Vec::new()
    } else {
        let mut t = target(sh, &scope);
        Eval::new(&syms, &mut t).load(&v).map_err(|e| (ERR_INV_EXPRESSION, e))?
    };
    Ok((bytes, props))
}

fn assign(sh: &Shared, id: &str, data: &[u8]) -> Res<()> {
    let syms = syms(sh)?;
    let (parent, text, symbol) = entry(sh, id)?;
    let (scope, v) = evaluate_entry(sh, &syms, &parent, &text, &symbol)?;
    let write = {
        let mut t = target(sh, &scope);
        Eval::new(&syms, &mut t).assign(&v, data).map_err(|e| (ERR_INV_EXPRESSION, e))?
    };
    let idx = scope.core.idx;
    match write {
        Write::Memory(addr, bytes) => {
            let expect = bytes.clone();
            sh.handle
                .call(move |cx| ops::write_memory(cx, addr, &bytes).map_err(|e| e.to_string()))
                .unwrap_or_else(|| Err("server stopped".into()))
                .map_err(|e| (ERR_OTHER, e))?;
            // The debug interface reports no error for writes the chip drops (e.g. memory
            // write-protected for the debugger by the application's SMPU regions): verify RAM
            // writes while halted. Peripheral registers and running cores may legitimately differ.
            if scope.core.suspended() && addr < PERIPHERAL_BASE {
                let back = read_mem(sh, addr, expect.len()).map_err(|e| (ERR_OTHER, e))?;
                if back != expect {
                    return fail(
                        ERR_OTHER,
                        format!("The target ignored the write to 0x{addr:08x}: the memory is write-protected for the debugger (e.g. by the application's SMPU configuration)"),
                    );
                }
            }
        }
        Write::Register(r, value) => {
            if scope.frame_index != Some(0) {
                return fail(ERR_OTHER, "Register variables can only be modified in the top stack frame");
            }
            let reg = if r == PC_REGISTER {
                REG_PC
            } else {
                REGS.iter().position(|x| x.dwarf.map(|d| d as u16) == Some(r)).ok_or_else(|| (ERR_OTHER, format!("No register for DWARF number {r}")))?
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

/// Handles Expressions.* and the variable part of Symbols.*; returns the reply fields.
pub fn dispatch(sh: &Shared, service: &str, command: &str, args: &[String]) -> Res<Vec<String>> {
    match (service, command) {
        ("Symbols", "getContext") => Ok(vec![String::new(), symbol_context(sh, &arg_str(args, 0)?)?.to_string()]),
        ("Symbols", "getChildren") => Ok(vec![String::new(), json!(symbol_children(sh, &arg_str(args, 0)?)?).to_string()]),
        ("Expressions", "create") => Ok(vec![String::new(), create(sh, arg_str(args, 0)?, arg_str(args, 2)?)?.to_string()]),
        ("Expressions", "getContext") => {
            let id = arg_str(args, 0)?;
            let syms = syms(sh)?;
            let (parent, text, symbol) = entry(sh, &id)?;
            Ok(vec![String::new(), expression_context(sh, &syms, &id, &parent, &text, &symbol).to_string()])
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
