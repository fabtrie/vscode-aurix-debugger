//! Variables by name: the globals of the loaded symbols, the children of an expression, and an
//! expression's value read and written.
//!
//! Names are resolved by the evaluator the TCF frontend uses ([`tcf::evaluate_global`]), so an
//! expression means the same variable here as in a VS Code live watch, through the same memory
//! view. Values are decoded by the DAQ's [`Decoder`], so `evaluate` shows exactly what a DAQ point
//! on the same expression streams.
//!
//! Core-local RAM (the `0xD000_0000` DSPR / `0xC000_0000` PSPR alias) exists once per TriCore
//! core. A variable there means the copy of the core an `@N` names, or of the only core its ELF
//! is assigned to ([`tcf::home_core`]); otherwise there is no single variable, so it is listed as
//! a group of one child per core, and reading or writing it by its bare name is refused.

use std::cell::OnceCell;
use std::sync::{Arc, Mutex};

use serde_json::Value as Json;

use api_spec::types::*;
use api_spec::{ApiError, ErrorCode, MAX_TRANSFER};
use aurix_core::events::emit;
use aurix_core::Event;
use symbols::expr::{Eval, Place, SymbolRef, Target};
use symbols::types::{MemberInfo, TypeClass, TypeId};
use symbols::Symbols;

use crate::core_impl::CoreApi;
use crate::daq_resolve::{decoder_for, Decoder};

const DEFAULT_LIMIT: u32 = 100;
const MAX_LIMIT: u32 = 10_000;

fn no_symbols() -> ApiError {
    ApiError::not_found("no symbols loaded (use `load_symbols`, or start a VS Code debug session)")
}

fn eval_error(e: tcf::GlobalError) -> ApiError {
    match e {
        tcf::GlobalError::NoSymbols => no_symbols(),
        tcf::GlobalError::NoTarget(m) => ApiError::not_found(m),
        tcf::GlobalError::Memory(m) => ApiError::new(ErrorCode::Memory, m),
        tcf::GlobalError::Expression(m) if m.starts_with("Symbol not found") => ApiError::not_found(m),
        tcf::GlobalError::Expression(m) => ApiError::bad_request(m),
        tcf::GlobalError::Core(m) => ApiError::bad_request(m),
    }
}

/// The context and symbol set a request means: the named target's, else the first core's that has
/// symbols. Fails fast, without a round trip to the core, when nothing is loaded.
fn symbol_set(api: &CoreApi, target: Option<&str>) -> Result<(String, Arc<tcf::SymbolSet>), ApiError> {
    let ctx = match target {
        None => String::new(),
        Some(n) => tcf::cores(&api.handle)
            .into_iter()
            .find(|c| c.name == n || c.idx.to_string() == n)
            .map(|c| c.id)
            .ok_or_else(|| ApiError::not_found(format!("unknown target '{n}' (see `targets`)")))?,
    };
    let set = api.session.symbols_for(&ctx).filter(|s| !s.elfs.is_empty()).ok_or_else(no_symbols)?;
    Ok((ctx, set))
}

fn evaluate_in(api: &CoreApi, target: Option<&str>, expression: &str) -> Result<tcf::GlobalValue, ApiError> {
    let (ctx, _) = symbol_set(api, target)?;
    tcf::evaluate_global(&api.session, &ctx, expression.trim()).map_err(eval_error)
}

fn page(offset: Option<u32>, limit: Option<u32>) -> (usize, usize) {
    (offset.unwrap_or(0) as usize, limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT) as usize)
}

// --- Types -----------------------------------------------------------------------------------

/// How a type is presented: name, class, and whether it can be expanded or sampled.
struct Shape {
    type_name: String,
    class: VariableClass,
    has_children: bool,
    sampleable: bool,
}

fn shape(syms: &Symbols, ty: &TypeId, bits: Option<(u32, u32)>) -> Shape {
    let info = syms.type_info(ty);
    let (decoder, _, _) = decoder_for(syms, ty, bits);
    let class = match info.class {
        TypeClass::Cardinal if matches!(decoder, Decoder::Bool { .. }) => VariableClass::Bool,
        TypeClass::Cardinal => VariableClass::Uint,
        TypeClass::Integer => VariableClass::Int,
        TypeClass::Real => VariableClass::Float,
        TypeClass::Pointer => VariableClass::Pointer,
        TypeClass::Enumeration => VariableClass::Enum,
        TypeClass::Array => VariableClass::Array,
        // A typedef keeps its own name, so ask the type it stands for whether it is a union.
        TypeClass::Composite if syms.type_info(&info.resolved).name.is_some_and(|n| n.starts_with("union ")) => VariableClass::Union,
        TypeClass::Composite => VariableClass::Struct,
        TypeClass::Function => VariableClass::Function,
        TypeClass::Unknown => VariableClass::Unknown,
    };
    let has_children = match info.class {
        TypeClass::Composite => !syms.members(ty).is_empty(),
        TypeClass::Array => info.length.is_some_and(|n| n > 0),
        _ => false,
    };
    Shape {
        type_name: info.name.unwrap_or_else(|| "<unknown>".into()),
        class,
        has_children,
        // Exactly the points DAQ streams as a typed value; it reads the rest as hex.
        sampleable: decoder != Decoder::Hex && info.size <= 8,
    }
}

fn variable(name: String, expression: String, syms: &Symbols, ty: &TypeId, size: u32, address: Option<u32>, bits: Option<(u32, u32)>) -> VariableInfo {
    let s = shape(syms, ty, bits);
    VariableInfo {
        per_core: false,
        name,
        expression,
        type_name: s.type_name,
        class: s.class,
        size,
        address,
        bit_offset: bits.map(|b| b.0),
        bit_size: bits.map(|b| b.1),
        has_children: s.has_children,
        sampleable: s.sampleable,
        unit: None,
    }
}

// --- list_globals ----------------------------------------------------------------------------

/// The globals of a symbol set sorted by name: (position of the ELF in the set, global index).
struct GlobalIndex {
    elfs: Vec<usize>,
    order: Vec<(u16, u32)>,
}

/// Sorted global lists of the symbol sets asked for recently. Sorting 10^5 names costs tens of
/// milliseconds, which a search-as-you-type client would otherwise pay on every keystroke.
#[derive(Default)]
pub struct GlobalsCache(Mutex<Vec<Arc<GlobalIndex>>>);

/// Sets kept: usually every core has the same files, so one or two are in use.
const CACHED_SETS: usize = 4;

fn cmp_ignore_case(a: &str, b: &str) -> std::cmp::Ordering {
    a.bytes().map(|c| c.to_ascii_lowercase()).cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
}

impl GlobalsCache {
    fn index(&self, set: &tcf::SymbolSet) -> Arc<GlobalIndex> {
        // ELF numbers identify the loaded contents: a reloaded file gets a new number.
        let elfs: Vec<usize> = set.elfs.iter().map(|e| e.0).collect();
        let mut cache = self.0.lock().unwrap();
        if let Some(i) = cache.iter().position(|x| x.elfs == elfs) {
            let hit = cache.remove(i);
            cache.push(hit.clone());
            return hit;
        }
        let mut order: Vec<(u16, u32)> = Vec::new();
        for (p, (_, syms)) in set.elfs.iter().enumerate() {
            // A DWARF global at address 0 is a definition the linker dropped.
            let kept = syms.globals.iter().enumerate().filter(|(_, g)| g.addr != 0 || g.unit == usize::MAX);
            order.extend(kept.map(|(i, _)| (p as u16, i as u32)));
        }
        let name = |&(p, i): &(u16, u32)| set.elfs[p as usize].1.globals[i as usize].name.as_str();
        order.sort_unstable_by(|a, b| cmp_ignore_case(name(a), name(b)).then_with(|| name(a).cmp(name(b))).then(a.cmp(b)));
        let index = Arc::new(GlobalIndex { elfs, order });
        if cache.len() >= CACHED_SETS {
            cache.remove(0);
        }
        cache.push(index.clone());
        index
    }
}

/// Case-insensitive glob match: `*` any run of characters, `?` exactly one.
fn glob(pattern: &[u8], name: &[u8]) -> bool {
    let (mut p, mut n) = (0, 0);
    // Where the last `*` was, and the name position it currently swallows up to.
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p].eq_ignore_ascii_case(&name[n])) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some((p, n));
            p += 1;
        } else if let Some((sp, sn)) = star {
            p = sp + 1;
            n = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == b'*')
}

fn is_identifier(s: &str) -> bool {
    let mut b = s.bytes();
    b.next().is_some_and(|c| c.is_ascii_alphabetic() || c == b'_') && b.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

/// The expression that names global `i` of the ELF at position `p`: its plain name if a lookup
/// of that name finds it, else a symbol reference.
///
/// A lookup tries the ELF files in order and takes the first that knows the name (as a global,
/// a function or an enumerator); within one file it prefers an external definition to a static.
fn global_expression(set: &tcf::SymbolSet, p: usize, i: usize) -> String {
    let (n, syms) = &set.elfs[p];
    let name = &syms.globals[i].name;
    let earlier = set.elfs[..p].iter().any(|(_, s)| {
        !s.global_indices(name).is_empty() || !s.function_indices(name).is_empty() || !s.enumerators_named(name).is_empty()
    });
    let gi = syms.global_indices(name);
    let found = gi.iter().copied().find(|&j| syms.globals[j].external).or(gi.first().copied());
    if is_identifier(name) && !earlier && found == Some(i) {
        name.clone()
    } else {
        format!("${{{}}}", tcf::elf_id(*n, &format!("GV:{i}")))
    }
}

/// Evaluation needs a target, but the type of a global reads nothing.
struct NoMemory;

impl Target for NoMemory {
    fn pc(&self) -> Option<u32> {
        None
    }
    fn reg(&self, _: u16) -> Option<u32> {
        None
    }
    fn read(&mut self, _: u32, _: usize) -> Result<Vec<u8>, String> {
        Err("no memory access while listing".into())
    }
    fn symbol(&mut self, _: &str) -> Option<SymbolRef> {
        None
    }
}

/// The TriCore cores by core ID, for [`tcf::home_core`]. Asked for only once a page holds a
/// core-local global: the question waits for the core thread, which a plain search never does.
fn tricores<'a>(api: &CoreApi, cache: &'a OnceCell<Vec<u32>>) -> &'a [u32] {
    cache.get_or_init(|| tcf::cores(&api.handle).into_iter().filter(|c| c.tricore()).map(|c| c.coreid).collect())
}

fn global_info(api: &CoreApi, cores: &OnceCell<Vec<u32>>, set: &tcf::SymbolSet, p: usize, i: usize) -> VariableInfo {
    let (n, syms) = &set.elfs[p];
    let g = &syms.globals[i];
    // The evaluator's own idea of the type: a label without debug information is one word.
    let ty = Eval::new(syms, &mut NoMemory).value_of(&SymbolRef::Global(i)).map(|v| v.ty).unwrap_or(TypeId::Void);
    let size = syms.type_info(&ty).size;
    // Core-local RAM: the global address in the one core the ELF is meant for, else a group.
    let address = if aurix_core::tricore::local_address(g.addr) {
        tcf::home_core(&api.session, tricores(api, cores), *n, None).ok().map(|core| aurix_core::tricore::global_address(core, g.addr))
    } else {
        Some(g.addr)
    };
    let mut v = variable(g.name.clone(), global_expression(set, p, i), syms, &ty, size, address, None);
    if address.is_none() {
        v.per_core = true;
        v.has_children = true;
        v.sampleable = false;
    }
    v.unit = syms.units.get(g.unit).map(|u| u.name.clone()).filter(|n| !n.is_empty());
    v
}

pub fn list_globals(api: &CoreApi, req: ListGlobalsRequest) -> Result<GlobalList, ApiError> {
    let (_, set) = symbol_set(api, req.target.as_deref())?;
    let cores = OnceCell::new();
    let index = api.globals.index(&set);
    let (offset, limit) = page(req.offset, req.limit);
    let pattern = req.query.as_deref().map(str::trim).filter(|q| !q.is_empty() && *q != "*").map(str::as_bytes);
    let mut total = 0usize;
    let mut items = Vec::new();
    for &(p, i) in &index.order {
        let (p, i) = (p as usize, i as usize);
        if let Some(q) = pattern {
            if !glob(q, set.elfs[p].1.globals[i].name.as_bytes()) {
                continue;
            }
        }
        // Types are looked up for this page only: that is the slow part.
        if total >= offset && items.len() < limit {
            items.push(global_info(api, &cores, &set, p, i));
        }
        total += 1;
    }
    Ok(GlobalList { total: total as u32, offset: offset as u32, items })
}

// --- symbol_children -------------------------------------------------------------------------

/// `expr` ready to take `.member` or `[i]`: postfix operators bind tighter than everything but a
/// primary expression, so anything else needs parentheses (`(*p).x`, `(a + 1)[2]`).
fn postfix_base(syms: &Symbols, expr: &str) -> String {
    use symbols::expr::Expr;
    let parsed = symbols::expr::parse(expr, &|n| Eval::<NoMemory>::resolve_type(syms, n));
    match parsed {
        Ok(Expr::Ident(_) | Expr::SymRef(_) | Expr::Member(..) | Expr::Arrow(..) | Expr::Index(..)) => expr.to_string(),
        _ => format!("({expr})"),
    }
}

/// Members of a struct or union, with the members of anonymous ones lifted to where C finds them.
fn named_members(syms: &Symbols, ty: &TypeId, base: u32, out: &mut Vec<MemberInfo>) {
    for m in syms.members(ty) {
        if m.name.is_some() {
            out.push(MemberInfo { offset: base + m.offset, ..m });
        } else {
            named_members(syms, &m.ty, base + m.offset, out);
        }
    }
}

/// The children of a variable in core-local RAM that no core is determined for: one per core it
/// may mean, each the variable `expr@N` at that core's global address.
fn per_core_children(gv: &tcf::GlobalValue, cores: &[u32], offset: usize, limit: usize) -> Vec<VariableInfo> {
    let syms = &gv.elf.1;
    let v = &gv.value;
    let (addr, size, bits) = match v.place {
        Place::Bits { addr, storage, shift, bits } => (addr, storage, Some((shift, bits))),
        _ => (gv.stored_at().unwrap_or(0), syms.type_info(&v.ty).size, None),
    };
    let expr = gv.expression();
    let copy = |c: u32| variable(format!("core {c}"), format!("{expr}@{c}"), syms, &v.ty, size, Some(aurix_core::tricore::global_address(c, addr)), bits);
    cores.iter().skip(offset).take(limit).map(|&c| copy(c)).collect()
}

pub fn symbol_children(api: &CoreApi, req: SymbolChildrenRequest) -> Result<SymbolChildren, ApiError> {
    let expression = req.expression.trim().to_string();
    let gv = evaluate_in(api, req.target.as_deref(), &expression)?;
    let syms = &gv.elf.1;
    let v = &gv.value;
    let info = syms.type_info(&v.ty);
    let (offset, limit) = page(req.offset, req.limit);
    if let Some(cores) = gv.per_core(&api.session) {
        return Ok(SymbolChildren {
            expression,
            type_name: info.name.unwrap_or_else(|| "<unknown>".into()),
            total: cores.len() as u32,
            offset: offset as u32,
            items: per_core_children(&gv, &cores, offset, limit),
        });
    }
    // Children of something stored at a fixed place have an address; of a computed value, none.
    let base = match (&v.place, &v.bytes) {
        (Place::Memory(a), None) => gv.global_address(&api.session, *a).ok(),
        _ => None,
    };
    // A child names the same core as its parent: `g_s@2` -> `g_s.a@2`.
    let parent = postfix_base(syms, gv.expression());
    let suffix = gv.suffix().map(|s| s.to_string()).unwrap_or_default();
    let (total, items) = match info.class {
        TypeClass::Composite => {
            let mut members = Vec::new();
            named_members(syms, &v.ty, 0, &mut members);
            let items = members
                .iter()
                .skip(offset)
                .take(limit)
                .map(|m| {
                    let name = m.name.clone().unwrap_or_default();
                    variable(name.clone(), format!("{parent}.{name}{suffix}"), syms, &m.ty, m.size, base.map(|a| a.wrapping_add(m.offset)), m.bits)
                })
                .collect();
            (members.len(), items)
        }
        TypeClass::Array => {
            let len = info.length.unwrap_or(0) as usize;
            let elem = info.base.clone().unwrap_or(TypeId::Void);
            let es = syms.type_info(&elem).size;
            let items = (offset..len.min(offset.saturating_add(limit)))
                .map(|i| {
                    let addr = base.map(|a| a.wrapping_add(es.wrapping_mul(i as u32)));
                    variable(format!("[{i}]"), format!("{parent}[{i}]{suffix}"), syms, &elem, es, addr, None)
                })
                .collect();
            (len, items)
        }
        _ => (0, Vec::new()),
    };
    Ok(SymbolChildren {
        expression,
        type_name: info.name.unwrap_or_else(|| "<unknown>".into()),
        total: total as u32,
        offset: offset as u32,
        items,
    })
}

// --- evaluate / write_variable ---------------------------------------------------------------

/// Reads and decodes an evaluated expression.
fn read_value(api: &CoreApi, expression: String, gv: &tcf::GlobalValue) -> Result<VariableValue, ApiError> {
    let syms = &gv.elf.1;
    let v = &gv.value;
    let size = syms.type_info(&v.ty).size;
    // Refuses a variable in core-local RAM that no core is named for, before reading anything.
    let address = gv.stored_at().map(|a| gv.global_address(&api.session, a)).transpose().map_err(eval_error)?;
    let (bits, raw) = match (&v.place, &v.bytes) {
        (_, Some(b)) => (None, b.clone()),
        (Place::Memory(a), None) => {
            if size > MAX_TRANSFER {
                return Err(ApiError::bad_request(format!("`{expression}` has {size} bytes, more than {MAX_TRANSFER}: use `read_memory`")));
            }
            let raw = if size == 0 { Vec::new() } else { gv.read(&api.session, *a, size as usize).map_err(|e| ApiError::new(ErrorCode::Memory, e))? };
            (None, raw)
        }
        (Place::Bits { addr, storage, shift, bits }, None) => {
            let raw = gv.read(&api.session, *addr, *storage as usize).map_err(|e| ApiError::new(ErrorCode::Memory, e))?;
            (Some((*shift, *bits)), raw)
        }
        _ => return Err(ApiError::bad_request(format!("`{expression}` has no value without a stack frame"))),
    };
    let shape = shape(syms, &v.ty, bits);
    // The DAQ's decoding: a bit field is extracted from the storage unit as read.
    let (mut decoder, _, _) = decoder_for(syms, &v.ty, bits);
    if raw.len() > 8 {
        decoder = Decoder::Hex;
    }
    Ok(VariableValue {
        expression,
        type_name: shape.type_name,
        class: shape.class,
        size: if bits.is_some() { raw.len() as u32 } else { size },
        address,
        bit_offset: bits.map(|b| b.0),
        bit_size: bits.map(|b| b.1),
        value: if raw.is_empty() { Json::String(String::new()) } else { decoder.decode(&raw) },
        raw_hex: Hex(raw),
        has_children: shape.has_children,
        sampleable: shape.sampleable,
    })
}

pub fn evaluate(api: &CoreApi, req: EvaluateRequest) -> Result<VariableValue, ApiError> {
    let expression = req.expression.trim().to_string();
    let gv = evaluate_in(api, req.target.as_deref(), &expression)?;
    read_value(api, expression, &gv)
}

/// An integer given as a JSON number, bool or string; `hex` when it was written as `0x...`.
fn integer(v: &Json) -> Option<(i128, bool)> {
    match v {
        Json::Bool(b) => Some((*b as i128, false)),
        Json::Number(n) => n
            .as_i64()
            .map(i128::from)
            .or_else(|| n.as_u64().map(i128::from))
            .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0 && f.abs() < 1.8e19).map(|f| f as i128))
            .map(|x| (x, false)),
        Json::String(s) => {
            let t = s.trim();
            let (neg, t) = match t.strip_prefix('-') {
                Some(r) => (true, r.trim_start()),
                None => (false, t),
            };
            let (x, hex) = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                Some(h) => (u64::from_str_radix(h, 16).ok()?, true),
                None => (t.parse::<u64>().ok()?, false),
            };
            Some((if neg { -(x as i128) } else { x as i128 }, hex && !neg))
        }
        _ => None,
    }
}

fn float(v: &Json) -> Option<f64> {
    match v {
        Json::Number(n) => n.as_f64(),
        Json::String(s) => match s.trim() {
            "nan" => Some(f64::NAN),
            "inf" => Some(f64::INFINITY),
            "-inf" => Some(f64::NEG_INFINITY),
            t => t.parse().ok(),
        },
        _ => None,
    }
}

/// The bytes (the size of the value's type) that store `value` into `v`.
fn encode(syms: &Symbols, v: &symbols::expr::Value, bits: Option<(u32, u32)>, value: &Json) -> Result<Vec<u8>, String> {
    let info = syms.type_info(&v.ty);
    let size = info.size as usize;
    let s = shape(syms, &v.ty, bits);
    let (decoder, _, _) = decoder_for(syms, &v.ty, bits);
    let scalar = s.sampleable && size <= 8;
    if s.class == VariableClass::Float && scalar {
        let x = float(value).ok_or_else(|| format!("{value} is not a number"))?;
        return Ok(if size == 4 { (x as f32).to_le_bytes().to_vec() } else { x.to_le_bytes().to_vec() });
    }
    if scalar {
        let named = match (value, &decoder) {
            (Json::String(name), Decoder::Enum { names, .. }) => names.iter().find(|(_, n)| n == name.trim()).map(|(x, _)| (*x as i128, false)),
            _ => None,
        };
        let (x, hex) = named.or_else(|| integer(value)).ok_or_else(|| match &decoder {
            Decoder::Enum { names, .. } => {
                let known: Vec<&str> = names.iter().map(|(_, n)| n.as_str()).take(16).collect();
                format!("{value} is neither a number nor an enumerator of {} (have: {})", s.type_name, known.join(", "))
            }
            _ => format!("{value} is not an integer"),
        })?;
        let width = bits.map_or(size as u32 * 8, |b| b.1).min(64);
        let umax = (1i128 << width) - 1;
        let signed = matches!(decoder, Decoder::Int { .. } | Decoder::Enum { signed: true, .. });
        let fits = if s.class == VariableClass::Bool {
            (0..=1).contains(&x)
        } else if signed {
            let half = 1i128 << (width - 1);
            (-half..half).contains(&x) || (hex && (0..=umax).contains(&x))
        } else {
            (0..=umax).contains(&x)
        };
        if !fits {
            return Err(format!("{value} does not fit {} ({width} bits)", s.type_name));
        }
        return Ok((x as u64 & umax as u64).to_le_bytes()[..size].to_vec());
    }
    // Aggregates and anything else: the bytes, exactly as `evaluate` shows them in `raw_hex`.
    let text = value.as_str().ok_or_else(|| format!("{} takes its {size} bytes as a hex string", s.type_name))?;
    let data: Hex = serde_json::from_value(Json::String(text.to_string())).map_err(|e| e.to_string())?;
    if data.0.len() != size {
        return Err(format!("{} has {size} bytes, the value has {}", s.type_name, data.0.len()));
    }
    Ok(data.0)
}

pub fn write_variable(api: &CoreApi, req: WriteVariableRequest) -> Result<VariableValue, ApiError> {
    let expression = req.expression.trim().to_string();
    let gv = evaluate_in(api, req.target.as_deref(), &expression)?;
    if let Some(a) = gv.stored_at() {
        gv.global_address(&api.session, a).map_err(eval_error)?;
    }
    let bits = match (&gv.value.place, &gv.value.bytes) {
        (Place::Memory(_), None) => None,
        (Place::Bits { shift, bits, .. }, None) => Some((*shift, *bits)),
        _ => return Err(ApiError::bad_request(format!("`{expression}` is not stored on the target, so it cannot be written"))),
    };
    let info = gv.elf.1.type_info(&gv.value.ty);
    if info.size == 0 || info.size > MAX_TRANSFER || info.class == TypeClass::Function {
        return Err(ApiError::bad_request(format!("`{expression}` ({}) cannot be written", info.name.unwrap_or_default())));
    }
    let data = encode(&gv.elf.1, &gv.value, bits, &req.value).map_err(ApiError::bad_request)?;
    let (t, addr, len) = gv.assign(&api.session, &data).map_err(eval_error)?;
    // Core-local RAM was written through that core's view; anything else is the same for all.
    let target = aurix_core::tricore::local_address(addr).then_some(t);
    emit(Event::MemoryChanged { target, addr, len });
    read_value(api, expression, &gv)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_patterns() {
        let m = |p: &str, n: &str| glob(p.as_bytes(), n.as_bytes());
        assert!(m("*speed*", "g_Motor_SPEED_rpm"));
        assert!(m("g_?otor", "g_motor"));
        assert!(m("*", ""));
        assert!(m("a*b*c", "aXXbYYc"));
        assert!(m("a*b*c", "abcbc"));
        assert!(!m("a*b*c", "aXXbYY"));
        assert!(!m("motor", "g_motor"), "a pattern matches the whole name");
        assert!(!m("g_?otor", "g_otor"));
        assert!(m("**x", "abx"));
    }

    #[test]
    fn integers_from_json() {
        use serde_json::json;
        assert_eq!(integer(&json!(42)), Some((42, false)));
        assert_eq!(integer(&json!(-1)), Some((-1, false)));
        assert_eq!(integer(&json!(true)), Some((1, false)));
        assert_eq!(integer(&json!(3.0)), Some((3, false)));
        assert_eq!(integer(&json!(3.5)), None);
        assert_eq!(integer(&json!("0xFFFFFFFFFFFFFFFF")), Some((u64::MAX as i128, true)));
        assert_eq!(integer(&json!("18446744073709551615")), Some((u64::MAX as i128, false)));
        assert_eq!(integer(&json!("-7")), Some((-7, false)));
        assert_eq!(integer(&json!("RUN")), None);
        assert_eq!(float(&json!("-inf")), Some(f64::NEG_INFINITY));
    }

    #[test]
    fn identifiers() {
        assert!(is_identifier("g_motor2"));
        assert!(!is_identifier("2x"));
        assert!(!is_identifier("a.b"));
        assert!(!is_identifier(""));
    }
}
