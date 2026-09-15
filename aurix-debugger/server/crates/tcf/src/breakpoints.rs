//! TCF Breakpoints service state: breakpoint properties from clients,
//! resolved per-core instances and the hardware breakpoints in the core.

use std::collections::{BTreeMap, HashMap};

use serde_json::{json, Map, Value};

use aurix_core::target::BpKind;
use aurix_core::{ops, ServerHandle};
use symbols::Symbols;

use crate::contexts::{cores, CoreInfo, CONTAINER};

/// Hardware triggers are scarce (8 per core): a source line that maps to many
/// places (e.g. an inline header function) is limited to this many addresses.
const MAX_ADDRESSES: usize = 4;

#[derive(Clone, Debug)]
pub struct Instance {
    pub target: usize,
    pub ctx: String,
    pub addr: u32,
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
}

#[derive(Default)]
pub struct BpTable {
    pub bps: BTreeMap<String, Bp>,
    /// (target, address) -> number of TCF breakpoints using the core breakpoint.
    planted: HashMap<(usize, u32), u32>,
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

/// Addresses of a breakpoint (and the actual line for File/Line), or why it
/// cannot be resolved.
fn resolve(props: &Map<String, Value>, syms: Option<&Symbols>) -> Result<(Vec<u32>, Option<u32>), String> {
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

fn target_cores(props: &Map<String, Value>, all: &[CoreInfo]) -> Vec<CoreInfo> {
    match props.get("ContextIds").and_then(Value::as_array) {
        Some(ids) if !ids.is_empty() => all
            .iter()
            .filter(|c| ids.iter().any(|i| i.as_str() == Some(c.id.as_str()) || i.as_str() == Some(CONTAINER)))
            .cloned()
            .collect(),
        _ => all.iter().filter(|c| c.examined).cloned().collect(),
    }
}

impl BpTable {
    fn plant_one(&mut self, h: &ServerHandle, target: usize, addr: u32) -> Result<(), String> {
        if let Some(n) = self.planted.get_mut(&(target, addr)) {
            *n += 1;
            return Ok(());
        }
        h.call(move |cx| ops::add_breakpoint(cx, target, addr, 4, BpKind::Hard).map(|_| ()).map_err(|e| e.to_string()))
            .unwrap_or_else(|| Err("server stopped".into()))?;
        self.planted.insert((target, addr), 1);
        Ok(())
    }

    fn unplant_one(&mut self, h: &ServerHandle, target: usize, addr: u32) {
        let Some(n) = self.planted.get_mut(&(target, addr)) else { return };
        *n -= 1;
        if *n == 0 {
            self.planted.remove(&(target, addr));
            let _ = h.call(move |cx| ops::remove_breakpoint(cx, target, addr));
        }
    }

    fn plant(&mut self, h: &ServerHandle, props: Map<String, Value>, owner: u64, hits: u32, syms: Option<&Symbols>) -> Bp {
        let mut bp = Bp { props, owner, instances: Vec::new(), hits, error: None, line: None };
        if !bp.enabled() {
            return bp;
        }
        match resolve(&bp.props, syms) {
            Err(e) => bp.error = Some(e),
            Ok((addrs, line)) => {
                bp.line = line;
                let all = cores(h);
                let targets = target_cores(&bp.props, &all);
                if targets.is_empty() {
                    bp.error = Some("No matching context for breakpoint".into());
                }
                for c in targets {
                    for &addr in &addrs {
                        let error = self.plant_one(h, c.idx, addr).err();
                        bp.instances.push(Instance { target: c.idx, ctx: c.id.clone(), addr, error });
                    }
                }
            }
        }
        bp
    }

    fn unplant(&mut self, h: &ServerHandle, bp: &Bp) {
        for i in &bp.instances {
            if i.error.is_none() {
                self.unplant_one(h, i.target, i.addr);
            }
        }
    }

    /// Adds or replaces a breakpoint. Returns (id, existed before).
    pub fn insert(&mut self, h: &ServerHandle, props: Map<String, Value>, owner: u64, syms: Option<&Symbols>) -> Result<(String, bool), String> {
        let id = props.get("ID").and_then(Value::as_str).ok_or("Breakpoint ID missing")?.to_string();
        let (existed, hits) = match self.bps.remove(&id) {
            Some(old) => {
                self.unplant(h, &old);
                (true, old.hits)
            }
            None => (false, 0),
        };
        let bp = self.plant(h, props, owner, hits, syms);
        self.bps.insert(id.clone(), bp);
        Ok((id, existed))
    }

    /// Re-plants breakpoints that could not be resolved (e.g. before symbols
    /// were loaded). Returns their IDs.
    pub fn reresolve(&mut self, h: &ServerHandle, syms: &Symbols) -> Vec<String> {
        let pending: Vec<String> = self
            .bps
            .iter()
            .filter(|(_, b)| b.enabled() && (b.error.is_some() || b.instances.iter().any(|i| i.error.is_some())))
            .map(|(id, _)| id.clone())
            .collect();
        for id in &pending {
            if let Some(old) = self.bps.remove(id) {
                self.unplant(h, &old);
                let bp = self.plant(h, old.props, old.owner, old.hits, Some(syms));
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
            .map(|i| match &i.error {
                Some(e) => json!({ "LocationContext": i.ctx, "Error": e }),
                None => json!({
                    "LocationContext": i.ctx,
                    "MemoryContext": i.ctx,
                    "BreakpointType": "Hardware",
                    "Address": i.addr,
                    "Size": 1,
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

    /// Breakpoint IDs with an instance at `addr` on `target`; counts the hit.
    pub fn record_hit(&mut self, target: usize, addr: u32) -> Vec<String> {
        let mut ids = Vec::new();
        for (id, bp) in self.bps.iter_mut() {
            if bp.instances.iter().any(|i| i.error.is_none() && i.target == target && i.addr == addr) {
                bp.hits += 1;
                ids.push(id.clone());
            }
        }
        ids
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
        assert_eq!(resolve(&m, None).unwrap().0, vec![0x8000_abcd]);
        let mut m = Map::new();
        m.insert("Location".into(), json!("2147483649"));
        assert_eq!(resolve(&m, None).unwrap().0, vec![0x8000_0001]);
        let mut m = Map::new();
        m.insert("File".into(), json!("main.c"));
        m.insert("Line".into(), json!(10));
        assert!(resolve(&m, None).is_err());
    }
}
