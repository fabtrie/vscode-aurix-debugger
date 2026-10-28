//! TCF Breakpoints service state: breakpoint properties from clients,
//! resolved per-core instances and the hardware breakpoints and watchpoints in the core.

use std::collections::{BTreeMap, HashMap};

use serde_json::{json, Map, Value};

use aurix_core::target::{BpKind, DebugReason, WatchAccess};
use aurix_core::{ops, ServerHandle};
use crate::contexts::{cores, CoreInfo, CONTAINER};
use crate::symstore::SymbolSet;

/// Hardware triggers are scarce (8 per core): a source line that maps to many
/// places (e.g. an inline header function) is limited to this many addresses.
const MAX_ADDRESSES: usize = 4;

/// `AccessMode` bits of a breakpoint.
const ACCESS_READ: u64 = 0x01;
const ACCESS_WRITE: u64 = 0x02;
const ACCESS_EXECUTE: u64 = 0x04;
const ACCESS_CHANGE: u64 = 0x08;

/// What an instance plants in its core.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// A hardware breakpoint.
    Code,
    /// A watchpoint on `len` bytes.
    Data { len: u32, access: WatchAccess },
}

#[derive(Clone, Debug)]
pub struct Instance {
    pub target: usize,
    pub ctx: String,
    pub addr: u32,
    pub kind: Kind,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Bp {
    pub props: Map<String, Value>,
    pub owner: u64,
    pub instances: Vec<Instance>,
    pub hits: u32,
    pub error: Option<String>,
    /// Actual source line for File/Line breakpoints.
    pub line: Option<u32>,
}

impl Bp {
    pub fn planted(&self) -> bool {
        self.instances.iter().any(|i| i.error.is_none())
    }

    pub fn enabled(&self) -> bool {
        self.props.get("Enabled").and_then(Value::as_bool).unwrap_or(false)
    }

    /// The access a data breakpoint (watchpoint) halts on; `None` for a code breakpoint.
    pub fn data(&self) -> Option<WatchAccess> {
        access_mode(&self.props)
    }

    /// The condition that must hold for a hit to halt, if it has one.
    pub fn condition(&self) -> Option<&str> {
        self.props.get("Condition").and_then(Value::as_str).map(str::trim).filter(|c| !c.is_empty())
    }
}

/// The access a breakpoint with an `AccessMode` of read and/or write (and not execute) watches.
/// A value change can only be caught as the write that makes it.
pub fn access_mode(props: &Map<String, Value>) -> Option<WatchAccess> {
    let m = props.get("AccessMode").and_then(Value::as_u64)?;
    if m & ACCESS_EXECUTE != 0 {
        return None;
    }
    match (m & ACCESS_READ != 0, m & (ACCESS_WRITE | ACCESS_CHANGE) != 0) {
        (true, true) => Some(WatchAccess::Access),
        (true, false) => Some(WatchAccess::Read),
        (false, true) => Some(WatchAccess::Write),
        (false, false) => None,
    }
}

/// The `AccessMode` bits of a watched access.
pub fn access_bits(a: WatchAccess) -> u64 {
    match a {
        WatchAccess::Read => ACCESS_READ,
        WatchAccess::Write => ACCESS_WRITE,
        WatchAccess::Access => ACCESS_READ | ACCESS_WRITE,
    }
}

#[derive(Default)]
pub struct BpTable {
    pub bps: BTreeMap<String, Bp>,
    /// (target, address, kind) -> (ID of the core breakpoint or watchpoint, number of TCF
    /// breakpoints using it).
    planted: HashMap<(usize, u32, Kind), (u32, u32)>,
}

fn parse_addr(v: &Value) -> Option<u32> {
    match v {
        Value::Number(n) => n.as_u64().map(|x| x as u32),
        Value::String(s) => {
            let t = s.trim();
            match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                Some(h) => u32::from_str_radix(h, 16).ok(),
                None => t.parse().ok(),
            }
        }
        _ => None,
    }
}

/// Addresses of a breakpoint on `core` with what to plant there (and the actual line for
/// File/Line), or why it cannot be resolved.
fn resolve(props: &Map<String, Value>, core: &CoreInfo, r: &Resolver) -> Result<(Vec<u32>, Kind, Option<u32>), String> {
    let Some(access) = access_mode(props) else {
        let set = (r.syms)(&core.id);
        return resolve_code(props, set.as_deref()).map(|(addrs, line)| (addrs, Kind::Code, line));
    };
    let size = props.get("Size").and_then(Value::as_u64).map(|s| s as u32);
    let (addr, len) = match (props.get("Address"), props.get("Location")) {
        (Some(a), _) => (parse_addr(a).ok_or_else(|| format!("Invalid address: {a}"))?, size.unwrap_or(4)),
        (None, Some(l)) => match parse_addr(l) {
            Some(a) => (a, size.unwrap_or(4)),
            None => {
                let (a, s) = (r.data)(core, l.as_str().unwrap_or_default())?;
                (a, size.unwrap_or(s))
            }
        },
        _ => return Err("A data breakpoint needs an address or an expression".into()),
    };
    if len == 0 {
        return Err("A data breakpoint needs a size of at least one byte".into());
    }
    Ok((vec![addr], Kind::Data { len, access }, None))
}

/// Addresses of a code breakpoint (and the actual line for File/Line), or why it
/// cannot be resolved.
fn resolve_code(props: &Map<String, Value>, syms: Option<&SymbolSet>) -> Result<(Vec<u32>, Option<u32>), String> {
    if let Some(a) = props.get("Address") {
        return parse_addr(a).map(|x| (vec![x], None)).ok_or_else(|| format!("Invalid address: {a}"));
    }
    if let Some(l) = props.get("Location") {
        if let Some(x) = parse_addr(l) {
            return Ok((vec![x], None));
        }
        let name = l.as_str().unwrap_or_default();
        let s = syms.ok_or_else(|| format!("Cannot evaluate location '{name}': symbols not loaded"))?;
        return s.resolve_location(name).map(|x| (vec![x], None)).ok_or_else(|| format!("Unknown symbol '{name}'"));
    }
    if let (Some(file), Some(line)) = (props.get("File").and_then(Value::as_str), props.get("Line").and_then(Value::as_u64)) {
        let s = syms.ok_or("Line number information not available (no symbols loaded)")?;
        let (actual, mut addrs) = s.resolve_line(file, line as u32)?;
        if addrs.is_empty() {
            return Err(format!("No code for {file}:{line}"));
        }
        addrs.truncate(MAX_ADDRESSES);
        return Ok((addrs, Some(actual)));
    }
    Err("Breakpoint has no location".into())
}

/// Symbol set of a core, by context ID.
pub type SymbolsFor<'a> = &'a dyn Fn(&str) -> Option<std::sync::Arc<SymbolSet>>;

/// Address and size at which a core watches the object an expression names (see
/// [`crate::expressions::watch_range`]).
pub type DataFor<'a> = &'a dyn Fn(&CoreInfo, &str) -> Result<(u32, u32), String>;

/// How breakpoint locations are resolved on a core.
pub struct Resolver<'a> {
    pub syms: SymbolsFor<'a>,
    pub data: DataFor<'a>,
}

/// Whether the breakpoint's addresses depend on symbols.
fn symbolic(props: &Map<String, Value>) -> bool {
    !props.contains_key("Address") && props.get("Location").is_none_or(|l| parse_addr(l).is_none())
}

/// The cores a breakpoint is planted on: those of its `ContextIds`, else every examined core; a
/// data breakpoint every TriCore core, since the other cores have their own address spaces (the
/// HSM) or no watchpoints at all (MCS channels).
fn target_cores(props: &Map<String, Value>, all: &[CoreInfo]) -> Vec<CoreInfo> {
    match props.get("ContextIds").and_then(Value::as_array) {
        Some(ids) if !ids.is_empty() => all
            .iter()
            .filter(|c| ids.iter().any(|i| i.as_str() == Some(c.id.as_str()) || i.as_str() == Some(CONTAINER)))
            .cloned()
            .collect(),
        _ if access_mode(props).is_some() => all.iter().filter(|c| c.examined && c.tricore()).cloned().collect(),
        _ => all.iter().filter(|c| c.examined).cloned().collect(),
    }
}

impl BpTable {
    fn plant_one(&mut self, h: &ServerHandle, target: usize, addr: u32, kind: Kind) -> Result<(), String> {
        if let Some((_, n)) = self.planted.get_mut(&(target, addr, kind)) {
            *n += 1;
            return Ok(());
        }
        let id = h
            .call(move |cx| {
                match kind {
                    Kind::Code => ops::add_breakpoint(cx, target, addr, 4, BpKind::Hard),
                    Kind::Data { len, access } => ops::add_watchpoint(cx, target, addr, len, access),
                }
                .map_err(|e| e.to_string())
            })
            .unwrap_or_else(|| Err("server stopped".into()))?;
        self.planted.insert((target, addr, kind), (id, 1));
        Ok(())
    }

    fn unplant_one(&mut self, h: &ServerHandle, target: usize, addr: u32, kind: Kind) {
        let Some((id, n)) = self.planted.get_mut(&(target, addr, kind)) else { return };
        *n -= 1;
        if *n == 0 {
            let id = *id;
            self.planted.remove(&(target, addr, kind));
            let _ = h.call(move |cx| match kind {
                Kind::Code => ops::remove_breakpoint(cx, target, addr),
                Kind::Data { .. } => ops::remove_watchpoint_id(cx, target, id),
            });
        }
    }

    /// Resolves the breakpoint per core with that core's symbols and plants it where it
    /// resolves: a source line of the HSM ELF only lands on the HSM.
    fn plant(&mut self, h: &ServerHandle, props: Map<String, Value>, owner: u64, hits: u32, r: &Resolver) -> Bp {
        let mut bp = Bp { props, owner, instances: Vec::new(), hits, error: None, line: None };
        if !bp.enabled() {
            return bp;
        }
        let all = cores(h);
        let targets = target_cores(&bp.props, &all);
        if targets.is_empty() {
            bp.error = Some("No matching context for breakpoint".into());
            return bp;
        }
        let mut first_err = None;
        for c in targets {
            match resolve(&bp.props, &c, r) {
                Err(e) => {
                    first_err.get_or_insert(e);
                }
                Ok((addrs, kind, line)) => {
                    bp.line = bp.line.or(line);
                    for &addr in &addrs {
                        let error = self.plant_one(h, c.idx, addr, kind).err();
                        bp.instances.push(Instance { target: c.idx, ctx: c.id.clone(), addr, kind, error });
                    }
                }
            }
        }
        if bp.instances.is_empty() {
            bp.error = first_err;
        }
        bp
    }

    fn unplant(&mut self, h: &ServerHandle, bp: &Bp) {
        for i in &bp.instances {
            if i.error.is_none() {
                self.unplant_one(h, i.target, i.addr, i.kind);
            }
        }
    }

    /// Adds or replaces a breakpoint. Returns (id, existed before).
    pub fn insert(&mut self, h: &ServerHandle, props: Map<String, Value>, owner: u64, r: &Resolver) -> Result<(String, bool), String> {
        let id = props.get("ID").and_then(Value::as_str).ok_or("Breakpoint ID missing")?.to_string();
        let (existed, hits) = match self.bps.remove(&id) {
            Some(old) => {
                self.unplant(h, &old);
                (true, old.hits)
            }
            None => (false, 0),
        };
        let bp = self.plant(h, props, owner, hits, r);
        self.bps.insert(id.clone(), bp);
        Ok((id, existed))
    }

    /// Re-plants breakpoints after symbol files changed: those that could not be resolved
    /// and those resolved from symbols (another ELF may add or move addresses). Returns
    /// their IDs.
    pub fn reresolve(&mut self, h: &ServerHandle, r: &Resolver) -> Vec<String> {
        let pending: Vec<String> = self
            .bps
            .iter()
            .filter(|(_, b)| b.enabled() && (symbolic(&b.props) || b.error.is_some() || b.instances.iter().any(|i| i.error.is_some())))
            .map(|(id, _)| id.clone())
            .collect();
        for id in &pending {
            if let Some(old) = self.bps.remove(id) {
                self.unplant(h, &old);
                let bp = self.plant(h, old.props, old.owner, old.hits, r);
                self.bps.insert(id.clone(), bp);
            }
        }
        pending
    }

    pub fn remove(&mut self, h: &ServerHandle, id: &str) -> bool {
        match self.bps.remove(id) {
            Some(bp) => {
                self.unplant(h, &bp);
                true
            }
            None => false,
        }
    }

    pub fn status(&self, id: &str) -> Value {
        let Some(bp) = self.bps.get(id) else { return json!({}) };
        if let Some(e) = &bp.error {
            return json!({ "Error": e });
        }
        let instances: Vec<Value> = bp
            .instances
            .iter()
            .map(|i| match (&i.error, i.kind) {
                (Some(e), _) => json!({ "LocationContext": i.ctx, "Error": e }),
                (None, Kind::Code) => json!({
                    "LocationContext": i.ctx,
                    "MemoryContext": i.ctx,
                    "BreakpointType": "Hardware",
                    "Address": i.addr,
                    "Size": 1,
                    "HitCount": bp.hits,
                }),
                (None, Kind::Data { len, access }) => json!({
                    "LocationContext": i.ctx,
                    "MemoryContext": i.ctx,
                    "BreakpointType": "Hardware",
                    "Address": i.addr,
                    "Size": len,
                    "AccessMode": access_bits(access),
                    "HitCount": bp.hits,
                }),
            })
            .collect();
        let mut st = json!({ "Instances": instances });
        if let Some(line) = bp.line {
            st["Line"] = json!(line);
            if let Some(f) = bp.props.get("File") {
                st["File"] = f.clone();
            }
        }
        st
    }

    /// The breakpoints a halt of `target` is for: those with a code instance at `pc` for a
    /// breakpoint stop; for a watchpoint stop those with the data instance of the core
    /// watchpoint `wp` that hit, or every data breakpoint of the target when the core cannot
    /// tell which one did (the HSM).
    pub fn hits(&self, target: usize, reason: DebugReason, pc: Option<u32>, wp: Option<(u32, Kind)>) -> Vec<String> {
        let matches = |i: &Instance| match reason {
            DebugReason::Breakpoint => i.kind == Kind::Code && Some(i.addr) == pc,
            DebugReason::Watchpoint => i.kind != Kind::Code && wp.is_none_or(|(addr, kind)| i.addr == addr && i.kind == kind),
            _ => false,
        };
        self.bps
            .iter()
            .filter(|(_, b)| b.instances.iter().any(|i| i.error.is_none() && i.target == target && matches(i)))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Counts a reported hit of each of `ids`.
    pub fn count_hits(&mut self, ids: &[String]) {
        for id in ids {
            if let Some(bp) = self.bps.get_mut(id) {
                bp.hits += 1;
            }
        }
    }

    /// The core watchpoints planted for breakpoints of this table, as (target, watchpoint ID).
    pub fn core_watchpoints(&self) -> Vec<(usize, u32)> {
        self.planted.iter().filter(|(k, _)| k.2 != Kind::Code).map(|(k, v)| (k.0, v.0)).collect()
    }

    pub fn owned_by(&self, owner: u64) -> Vec<String> {
        self.bps.iter().filter(|(_, b)| b.owner == owner).map(|(id, _)| id.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn addresses() {
        let mut m = Map::new();
        m.insert("Address".into(), json!("0x8000abcd"));
        assert_eq!(resolve_code(&m, None).unwrap().0, vec![0x8000_abcd]);
        let mut m = Map::new();
        m.insert("Location".into(), json!("2147483649"));
        assert_eq!(resolve_code(&m, None).unwrap().0, vec![0x8000_0001]);
        let mut m = Map::new();
        m.insert("File".into(), json!("main.c"));
        m.insert("Line".into(), json!(10));
        assert!(resolve_code(&m, None).is_err());
    }

    #[test]
    fn access_modes() {
        let mode = |m: u64| access_mode(json!({ "AccessMode": m }).as_object().unwrap());
        assert_eq!(mode(ACCESS_WRITE), Some(WatchAccess::Write));
        assert_eq!(mode(ACCESS_CHANGE), Some(WatchAccess::Write));
        assert_eq!(mode(ACCESS_READ), Some(WatchAccess::Read));
        assert_eq!(mode(ACCESS_READ | ACCESS_WRITE), Some(WatchAccess::Access));
        assert_eq!(mode(ACCESS_EXECUTE), None);
        assert_eq!(mode(ACCESS_EXECUTE | ACCESS_WRITE), None);
        assert_eq!(access_mode(&Map::new()), None);
        for a in [WatchAccess::Read, WatchAccess::Write, WatchAccess::Access] {
            assert_eq!(mode(access_bits(a)), Some(a));
        }
    }
}
