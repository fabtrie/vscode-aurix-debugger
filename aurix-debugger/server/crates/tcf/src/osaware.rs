//! The `AurixOS` TCF service: AUTOSAR OS awareness for the debug adapter.
//!
//! The extension already holds a TCF connection, so the OS objects travel on it rather than over
//! the REST API on another port. That also keeps the awareness tied to the debug session: it goes
//! away when the session does.
//!
//! The service is small on purpose. Everything about *what* an ORTI file means lives in the
//! `orti` crate; this is the transport plus the bus access.
//!
//! ```text
//! AurixOS load      <file:string> <target:string|null>  -> <error> <info>
//! AurixOS getInfo                                       -> <error> <info>
//! AurixOS getObjects <kind:string> <name:string|null> <liveOnly:bool>
//!                                                       -> <error> {kind, objects, reads}
//! AurixOS getRunning                                    -> <error> [ per core ]
//! AurixOS getStacks  <names:string[]|null>            -> <error> {Stacks, Reads}
//! AurixOS startProfile <periodUs:number>              -> <error> {PeriodUs, Cores}
//! AurixOS stopProfile                                 -> <error> {Stopped}
//! AurixOS getProfile <sinceUs:number>                 -> <error> {Transitions, Totals, ...}
//! ```

use std::path::Path;
use std::sync::Arc;

use serde_json::{json, Map, Value};

use aurix_core::ops;
use orti::{Attribute, Awareness, PlannedObject};
use symbols::expr::{SymbolRef, Target};

use crate::contexts::{cores, owning_core};
use crate::Shared;

/// A `Target` that reads the system bus through the core thread, counting its accesses.
///
/// OS data is plain RAM, so the system-bus view is the right one; reading "as core N" only
/// matters for HSM-private memory and MCS program RAM, which no ORTI file describes.
pub struct OsTarget<'a> {
    pub sh: &'a Shared,
    pub reads: u32,
}

impl Target for OsTarget<'_> {
    fn pc(&self) -> Option<u32> {
        None
    }
    fn reg(&self, _: u16) -> Option<u32> {
        None
    }
    fn read(&mut self, addr: u32, len: usize) -> Result<Vec<u8>, String> {
        self.reads += 1;
        self.sh.handle.call(move |cx| ops::read_memory(cx, addr, len).map_err(|e| e.to_string())).unwrap_or_else(|| Err("server stopped".into()))
    }
    fn symbol(&mut self, _: &str) -> Option<SymbolRef> {
        None
    }
}

/// Parses an ORTI file and plans it against the symbols of `target` (the first core with symbols
/// when it is `None`), then stores it on the session.
pub fn load(sh: &Shared, file: &str, target: Option<&str>) -> Result<Value, String> {
    let path = Path::new(file);
    if !path.is_file() {
        return Err(format!("no such file: {file}"));
    }
    let all = cores(&sh.handle);
    let chosen: Vec<&crate::CoreInfo> = match target {
        Some(n) => vec![all.iter().find(|c| c.name == n || c.id == n).ok_or_else(|| format!("unknown target '{n}'"))?],
        None => all.iter().collect(),
    };
    let (name, syms) = chosen
        .iter()
        .find_map(|c| {
            let ctx = owning_core(&c.id).unwrap_or_else(|| c.id.clone());
            let set = sh.symbols_for(&ctx)?;
            let (_, s) = set.elfs.first()?;
            Some((c.name.clone(), s.clone()))
        })
        .ok_or("no symbols loaded for any core")?;

    let mut t = OsTarget { sh, reads: 0 };
    let aw = Awareness::load(path, syms, &mut t)?;
    let info = info_json(&aw, Some(&name));
    *sh.orti.lock().unwrap() = Some(Arc::new(aw));
    Ok(info)
}

/// What is loaded: object types, their attributes, and how many cores the file describes.
pub fn info(sh: &Shared) -> Value {
    match sh.orti.lock().unwrap().as_ref() {
        Some(aw) => info_json(aw, None),
        None => json!({ "Loaded": false }),
    }
}

fn info_json(aw: &Awareness, target: Option<&str>) -> Value {
    let types: Vec<Value> = aw
        .orti
        .decls
        .iter()
        .map(|d| {
            let attrs: Vec<Value> = d
                .attrs
                .iter()
                .map(|a| {
                    json!({
                        "Name": a.name,
                        "Description": a.desc,
                        "Kind": match a.kind {
                            orti::AttrKind::CType => "ctype",
                            orti::AttrKind::Enum => "enum",
                            orti::AttrKind::String => "string",
                        },
                        "PerCore": a.indexed,
                        // Whether a value can change is a property of the planned instances, not
                        // of the declaration: `STACK` is an address constant in every task.
                        "Live": aw.objects_of(&d.name).flat_map(|o| o.attrs.iter()).any(|p| p.name == a.name && p.plan.is_live()),
                    })
                })
                .collect();
            json!({ "Name": d.name, "Description": d.desc, "Count": aw.objects_of(&d.name).count(), "Attributes": attrs })
        })
        .collect();
    json!({
        "Loaded": true,
        "File": aw.file.to_string_lossy(),
        "Implementation": aw.orti.implementation,
        "Version": aw.orti.version.semantics_version,
        "Cores": aw.orti.core_count(),
        "Target": target,
        "Types": types,
        "Errors": aw.errors,
    })
}

fn attribute_json(aw: &Awareness, kind: &str, a: &Attribute, t: &mut OsTarget) -> Value {
    let mut m = Map::new();
    m.insert("Name".into(), json!(a.name));
    if let Some(c) = a.index {
        m.insert("Core".into(), json!(c));
    }
    match aw.read(kind, a, t) {
        Ok(bytes) => {
            let s = a.show(&bytes);
            m.insert("Value".into(), json!(s.text));
            if let Some(n) = s.number {
                m.insert("Number".into(), json!(n));
            }
            if let Some(l) = s.link {
                m.insert("Link".into(), json!(l));
            }
        }
        Err(e) => {
            m.insert("Value".into(), json!(""));
            m.insert("Error".into(), json!(e));
        }
    }
    Value::Object(m)
}

fn object_json(aw: &Awareness, kind: &str, o: &PlannedObject, live_only: bool, t: &mut OsTarget) -> Value {
    let attrs: Vec<Value> =
        o.attrs.iter().filter(|a| !live_only || a.plan.is_live()).map(|a| attribute_json(aw, kind, a, t)).collect();
    json!({ "Name": o.name, "Attributes": attrs })
}

/// The objects of one ORTI type, read now.
pub fn objects(sh: &Shared, kind: &str, name: Option<&str>, live_only: bool) -> Result<Value, String> {
    let aw = sh.orti.lock().unwrap().clone().ok_or("no ORTI file loaded")?;
    if aw.orti.decl(kind).is_none() {
        let known: Vec<&str> = aw.orti.decls.iter().map(|d| d.name.as_str()).collect();
        return Err(format!("unknown object type '{kind}' (have: {})", known.join(", ")));
    }
    let mut t = OsTarget { sh, reads: 0 };
    let objects: Vec<Value> = aw
        .objects_of(kind)
        .filter(|o| name.is_none_or(|n| n == o.name))
        .map(|o| object_json(&aw, kind, o, live_only, &mut t))
        .collect();
    Ok(json!({ "Kind": kind, "Objects": objects, "Reads": t.reads }))
}

/// What is running on each core right now: the smallest read the debug adapter needs to name a
/// thread after its task, so it is its own command rather than a filter over `getObjects`.
pub fn running(sh: &Shared) -> Result<Value, String> {
    let aw = sh.orti.lock().unwrap().clone().ok_or("no ORTI file loaded")?;
    let os = aw.objects_of("OS").next().ok_or("the ORTI file has no OS object")?;
    let mut t = OsTarget { sh, reads: 0 };
    let mut out = Vec::new();
    for core in 0..aw.orti.core_count() {
        let mut m = Map::new();
        m.insert("Core".into(), json!(core));
        // The AUTOSAR core id, which is what a frontend has to match its debug target against:
        // the ORTI index is just a position in the file.
        for want in ["vs_COREID", "RUNNINGTASK", "RUNNINGTASKPRIORITY", "RUNNINGISR2", "LASTERROR"] {
            // A single-core file has these without an index; an SMP one has one per core.
            let Some(a) = os.attrs.iter().find(|a| a.name == want && (a.index == Some(core) || (a.index.is_none() && core == 0))) else {
                continue;
            };
            let key = match want {
                "vs_COREID" => "CoreId",
                "RUNNINGTASK" => "Task",
                "RUNNINGTASKPRIORITY" => "Priority",
                "RUNNINGISR2" => "Isr",
                _ => "LastError",
            };
            match aw.read("OS", a, &mut t) {
                Ok(b) => {
                    let s = a.show(&b);
                    // The core id is for matching a debug target, so it goes out as the number;
                    // everything else is for a person to read.
                    m.insert(key.into(), if key == "CoreId" { json!(s.number) } else { json!(s.text) });
                }
                Err(e) => {
                    m.insert(format!("{key}Error"), json!(e));
                }
            }
        }
        out.push(Value::Object(m));
    }
    Ok(json!({ "Cores": out, "Reads": t.reads }))
}

/// Stack coverage for every stack the ORTI file describes, or only the named ones.
///
/// Expensive by nature - finding a high-water mark means reading the untouched part of a stack -
/// so this is only ever called on demand, and the caller is told what it cost.
pub fn stacks(sh: &Shared, only: Option<&[String]>) -> Result<Value, String> {
    let aw = sh.orti.lock().unwrap().clone().ok_or("no ORTI file loaded")?;
    let mut t = OsTarget { sh, reads: 0 };
    let measured = orti::stack::measure(&aw, only, &mut t);
    let out: Vec<Value> = measured
        .iter()
        .map(|s| {
            let mut m = Map::new();
            m.insert("Name".into(), json!(s.name));
            m.insert("Base".into(), json!(s.base));
            m.insert("Size".into(), json!(s.size));
            m.insert("Direction".into(), json!(if s.direction == orti::Direction::Up { "UP" } else { "DOWN" }));
            m.insert("Pattern".into(), json!(s.pattern));
            if let Some(u) = s.used {
                m.insert("Used".into(), json!(u));
                m.insert("Free".into(), json!(s.size.saturating_sub(u)));
            }
            if let Some(p) = s.percent() {
                m.insert("Percent".into(), json!(p));
            }
            if let Some(e) = &s.error {
                m.insert("Error".into(), json!(e));
            }
            Value::Object(m)
        })
        .collect();
    Ok(json!({ "Stacks": out, "Reads": t.reads }))
}
