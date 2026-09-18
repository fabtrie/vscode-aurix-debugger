//! One TCF connection: command dispatch and replies.

use std::io::Write;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use aurix_core::unwind::Frame;
use symbols::LineInfo;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::{json, Map, Value};

use aurix_core::arch::RegGroup;
use aurix_core::target::TargetState;
use aurix_core::{log_debug, log_warn, ops};

use crate::contexts::{chip_name, cores, find_core, owning_core, read_pc, reason_text, CoreInfo, CONTAINER};
use crate::symstore::elf_id;
use crate::wire::{self, Reader};
use crate::{Shared, SERVICES};

// TCF standard error codes
const ERR_OTHER: i64 = 1;
const ERR_JSON_SYNTAX: i64 = 2;
const ERR_ALREADY_STOPPED: i64 = 10;
const ERR_ALREADY_RUNNING: i64 = 12;
const ERR_IS_RUNNING: i64 = 14;
const ERR_INV_CONTEXT: i64 = 16;
const ERR_INV_ADDRESS: i64 = 17;
const ERR_SYM_NOT_FOUND: i64 = 22;
const ERR_INV_COMMAND: i64 = 25;

/// Shared, thread-safe socket writer.
#[derive(Clone)]
pub struct Writer(Arc<Mutex<TcpStream>>);

impl Writer {
    pub fn send(&self, msg: &[u8]) {
        let _ = self.0.lock().unwrap().write_all(msg);
    }
}

struct TcfError {
    code: i64,
    msg: String,
}

type Reply = Result<Vec<String>, TcfError>;

fn err(code: i64, msg: impl Into<String>) -> TcfError {
    TcfError { code, msg: msg.into() }
}

fn error_json(e: &TcfError) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    json!({ "Code": e.code, "Time": now, "Format": e.msg }).to_string()
}

fn j<T: serde::Serialize + ?Sized>(v: &T) -> String {
    serde_json::to_string(v).unwrap()
}

struct Args<'a>(&'a [String]);

impl<'a> Args<'a> {
    fn value(&self, i: usize) -> Result<Value, TcfError> {
        let s = self.0.get(i).ok_or_else(|| err(ERR_JSON_SYNTAX, format!("missing argument {}", i + 1)))?;
        serde_json::from_str(s).map_err(|e| err(ERR_JSON_SYNTAX, format!("argument {}: {e}", i + 1)))
    }
    fn string(&self, i: usize) -> Result<String, TcfError> {
        self.value(i)?.as_str().map(str::to_string).ok_or_else(|| err(ERR_JSON_SYNTAX, format!("argument {} must be a string", i + 1)))
    }
    fn opt_string(&self, i: usize) -> Result<Option<String>, TcfError> {
        match self.value(i)? {
            Value::Null => Ok(None),
            Value::String(s) => Ok(Some(s)),
            v => Err(err(ERR_JSON_SYNTAX, format!("argument {}: expected string or null, got {v}", i + 1))),
        }
    }
    fn u64(&self, i: usize) -> Result<u64, TcfError> {
        self.value(i)?.as_u64().ok_or_else(|| err(ERR_JSON_SYNTAX, format!("argument {} must be a number", i + 1)))
    }
}

pub fn run(shared: Arc<Shared>, id: u64, stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let Ok(write_half) = stream.try_clone() else { return };
    let writer = Writer(Arc::new(Mutex::new(write_half)));
    writer.send(&wire::event("Locator", "Hello", &[j(SERVICES)]));
    shared.channels.lock().unwrap().insert(id, writer.clone());

    let ch = Channel { shared: shared.clone(), id, writer: writer.clone() };
    let mut reader = Reader::new(stream);
    loop {
        match reader.next_message() {
            Ok(Some(fields)) => ch.message(&fields),
            Ok(None) => break,
            Err(e) => {
                log_debug!("TCF: read error: {e}");
                break;
            }
        }
    }

    shared.channels.lock().unwrap().remove(&id);
    shared.remove_owned(id);
}

struct Channel {
    shared: Arc<Shared>,
    id: u64,
    writer: Writer,
}

impl Channel {
    fn message(&self, f: &[String]) {
        match f.first().map(String::as_str) {
            Some("C") if f.len() >= 4 => {
                let token = &f[1];
                let (service, command) = (f[2].as_str(), f[3].as_str());
                let args = Args(&f[4..]);
                log_debug!("TCF <- {service}.{command} {:?}", &f[4..]);
                let reply = self.dispatch(service, command, &args);
                self.reply(token, service, command, reply);
            }
            Some("E") | Some("F") | Some("R") | Some("N") | Some("P") => {}
            _ => log_warn!("TCF: ignoring malformed message {f:?}"),
        }
    }

    fn reply(&self, token: &str, service: &str, command: &str, reply: Reply) {
        let mut fields = vec!["R".to_string(), token.to_string()];
        match reply {
            Ok(v) => fields.extend(v),
            Err(e) => {
                log_debug!("TCF -> {service}.{command} error {}: {}", e.code, e.msg);
                let ej = error_json(&e);
                // Error position differs per command; see the reply layouts below.
                match (service, command) {
                    ("Memory", "get") | ("Memory", "set") => fields.extend(["null".into(), ej, "null".into()]),
                    ("StackTrace", "getContext") => fields.extend(["null".into(), ej]),
                    ("Expressions", "evaluate") => fields.extend(["null".into(), ej, "null".into()]),
                    ("Expressions", "assign") | ("Expressions", "dispose") => fields.push(ej),
                    ("RunControl", "getState") => fields.extend([ej, "false".into(), "0".into(), "null".into(), "null".into()]),
                    _ => fields.extend([ej, "null".into()]),
                }
            }
        }
        self.writer.send(&wire::encode(&fields));
    }

    fn h(&self) -> &aurix_core::ServerHandle {
        &self.shared.handle
    }

    fn core(&self, id: &str) -> Result<CoreInfo, TcfError> {
        find_core(self.h(), id).ok_or_else(|| err(ERR_INV_CONTEXT, format!("Invalid context: {id}")))
    }

    fn dispatch(&self, service: &str, command: &str, a: &Args) -> Reply {
        match (service, command) {
            ("Locator", "sync") => Ok(vec![]),
            ("Locator", "getPeers") => Ok(vec![String::new(), "[]".into()]),
            ("ContextQuery", "query") => {
                let mut ids = vec![CONTAINER.to_string()];
                ids.extend(cores(self.h()).into_iter().filter(|c| c.visible).map(|c| c.id));
                Ok(vec![String::new(), j(&ids)])
            }
            ("ContextQuery", "getAttrNames") => Ok(vec![String::new(), j(&["ID", "ParentID", "Name"])]),

            ("RunControl", "getChildren") => self.rc_children(a),
            ("RunControl", "getContext") => self.rc_context(a),
            ("RunControl", "getState") => self.rc_state(a),
            ("RunControl", "suspend") => self.rc_suspend(a),
            ("RunControl", "resume") => self.rc_resume(a),

            ("Memory", "getChildren") => {
                let parent = a.opt_string(0)?;
                let ids: Vec<String> = if parent.is_none() { vec![CONTAINER.into()] } else { vec![] };
                Ok(vec![String::new(), j(&ids)])
            }
            ("Memory", "getContext") => Ok(vec![
                String::new(),
                json!({ "ID": a.string(0)?, "BigEndian": false, "AddressSize": 4, "Name": "memory" }).to_string(),
            ]),
            ("Memory", "get") => self.mem_get(a),
            ("Memory", "set") => self.mem_set(a),

            ("Registers", "getChildren") => self.reg_children(a),
            ("Registers", "getContext") => self.reg_context(a),
            ("Registers", "get") => self.reg_get(a),
            ("Registers", "set") => self.reg_set(a),

            ("Breakpoints", "set") => self.bp_set(a),
            ("Breakpoints", "add") | ("Breakpoints", "change") => self.bp_add(a),
            ("Breakpoints", "remove") => self.bp_remove(a),
            ("Breakpoints", "enable") | ("Breakpoints", "disable") => self.bp_enable(a, command == "enable"),
            ("Breakpoints", "getIDs") => {
                let ids: Vec<String> = self.shared.bps.lock().unwrap().bps.keys().cloned().collect();
                Ok(vec![String::new(), j(&ids)])
            }
            ("Breakpoints", "getProperties") => {
                let id = a.string(0)?;
                let t = self.shared.bps.lock().unwrap();
                let bp = t.bps.get(&id).ok_or_else(|| err(ERR_OTHER, format!("Invalid breakpoint ID: {id}")))?;
                Ok(vec![String::new(), Value::Object(bp.props.clone()).to_string()])
            }
            ("Breakpoints", "getStatus") => {
                let id = a.string(0)?;
                Ok(vec![String::new(), self.shared.bps.lock().unwrap().status(&id).to_string()])
            }
            ("Breakpoints", "getCapabilities") => Ok(vec![
                String::new(),
                json!({ "ID": CONTAINER, "Location": true, "FileLine": true, "ContextIds": true,
                        "BreakpointType": true, "IgnoreCount": false, "Condition": false,
                        "Temporary": false, "AccessMode": false }).to_string(),
            ]),

            ("StackTrace", "getChildren") => self.st_children(a, None),
            ("StackTrace", "getChildrenRange") => {
                let (s, e) = (a.u64(1)? as i64, a.u64(2)? as i64);
                self.st_children(a, Some((s, e)))
            }
            ("StackTrace", "getContext") => self.st_context(a),

            ("LineNumbers", "mapToSource") => {
                let ctx = a.opt_string(0)?.unwrap_or_default();
                let (start, end) = (a.u64(1)? as u32, a.u64(2)? as u32);
                let areas: Vec<Value> = match self.shared.symbols_for(&ctx) {
                    Some(s) => s.lines_in_range(start, end).iter().map(code_area).collect(),
                    None => vec![],
                };
                Ok(vec![String::new(), j(&areas)])
            }
            ("LineNumbers", "mapToMemory") => {
                let ctx = a.opt_string(0)?.unwrap_or_default();
                let file = a.string(1)?;
                let line = a.u64(2)? as u32;
                let s = self.shared.symbols_for(&ctx).ok_or_else(|| err(ERR_SYM_NOT_FOUND, "No symbols loaded"))?;
                let (actual, addrs) = s.resolve_line(&file, line).map_err(|e| err(ERR_OTHER, e))?;
                let areas: Vec<Value> = addrs
                    .iter()
                    .map(|&addr| {
                        let end = s.line_at(addr).map(|li| li.end).unwrap_or(addr + 2);
                        json!({ "File": file, "SLine": actual, "SAddr": addr, "EAddr": end, "IsStmt": true })
                    })
                    .collect();
                Ok(vec![String::new(), j(&areas)])
            }

            ("Symbols", "findByAddr") => {
                let ctx = a.opt_string(0)?.unwrap_or_default();
                let addr = a.u64(1)? as u32;
                let s = self.shared.symbols_for(&ctx).ok_or_else(|| err(ERR_SYM_NOT_FOUND, "No symbols loaded"))?;
                let (e, i) = s.function_index_at(addr).ok_or_else(|| err(ERR_SYM_NOT_FOUND, format!("No symbol at 0x{addr:08x}")))?;
                Ok(vec![String::new(), j(&elf_id(e, &format!("FN:{i}")))])
            }
            ("Symbols", "findByName") => {
                let ctx = a.opt_string(0)?.unwrap_or_default();
                let name = a.string(1)?;
                let s = self.shared.symbols_for(&ctx).ok_or_else(|| err(ERR_SYM_NOT_FOUND, "No symbols loaded"))?;
                for (e, s) in &s.elfs {
                    if let Some(i) = s.functions.iter().position(|f| f.name == name) {
                        return Ok(vec![String::new(), j(&elf_id(*e, &format!("FN:{i}")))]);
                    }
                    if let Some(i) = s.globals.iter().position(|g| g.name == name) {
                        return Ok(vec![String::new(), j(&elf_id(*e, &format!("GV:{i}")))]);
                    }
                }
                Err(err(ERR_SYM_NOT_FOUND, format!("Symbol not found: {name}")))
            }
            ("Disassembly", _) => crate::disassembly::dispatch(&self.shared, command, a.0).map_err(|(c, m)| err(c, m)),
            ("Symbols", "getContext") | ("Symbols", "getChildren") | ("Expressions", _) => {
                crate::expressions::dispatch(&self.shared, service, command, a.0).map_err(|(c, m)| err(c, m))
            }

            ("MemoryMap", "set") => {
                let ctx = a.string(0)?;
                let map = a.value(1)?;
                self.shared.memory_maps.lock().unwrap().insert(ctx.clone(), map.clone());
                // All files of the map: the first is the primary ELF, the others add symbols
                // (bootmanager, FBL, ...).
                let mut files: Vec<PathBuf> = Vec::new();
                for f in map.as_array().into_iter().flatten().filter_map(|m| m.get("FileName").and_then(Value::as_str)) {
                    if !files.iter().any(|x| x.as_path() == Path::new(f)) {
                        files.push(PathBuf::from(f));
                    }
                }
                if !files.is_empty() {
                    let ctx = owning_core(&ctx).unwrap_or(ctx);
                    self.shared.symbols.lock().unwrap().assign(&ctx, &files).map_err(|e| err(ERR_OTHER, e))?;
                    self.shared.stacks.lock().unwrap().clear();
                    self.shared.reresolve_breakpoints();
                }
                Ok(vec![String::new()])
            }
            ("MemoryMap", "get") => {
                let ctx = a.string(0)?;
                let m = self.shared.memory_maps.lock().unwrap().get(&ctx).cloned().unwrap_or(json!([]));
                Ok(vec![String::new(), m.to_string()])
            }

            ("ContextReset", "getCapabilities") => Ok(vec![
                String::new(),
                json!([{ "Type": "reset", "Description": "Target reset through the debug adapter (OpenOCD 'reset run', or 'reset halt' with Suspend)" }]).to_string(),
            ]),
            ("ContextReset", "reset") => {
                let params = a.value(2).unwrap_or(json!({}));
                let halt = params.get("Suspend").and_then(Value::as_bool).unwrap_or(false);
                let mode = if halt { "halt" } else { "run" };
                self.h()
                    .call(move |cx| ops::reset(cx, mode).map_err(|e| e.to_string()))
                    .unwrap_or_else(|| Err("server stopped".into()))
                    .map_err(|e| err(ERR_OTHER, e))?;
                Ok(vec![String::new()])
            }

            ("Tcl", "eval") => {
                let cmd = a.string(0)?;
                let (res, lines) = self.h().tcl_with_output(&cmd);
                let mut out = lines.join("\n");
                match res {
                    Ok(r) => {
                        if !r.is_empty() {
                            if !out.is_empty() {
                                out.push('\n');
                            }
                            out.push_str(&r);
                        }
                        Ok(vec![String::new(), j(&out)])
                    }
                    Err(e) => Err(err(ERR_OTHER, if out.is_empty() { e } else { format!("{out}\n{e}") })),
                }
            }

            _ => Err(err(ERR_INV_COMMAND, format!("Unsupported command: {service}.{command}"))),
        }
    }

    // --- RunControl ------------------------------------------------------------------

    fn rc_children(&self, a: &Args) -> Reply {
        let parent = a.opt_string(0)?;
        let ids: Vec<String> = match parent.as_deref() {
            None => vec![CONTAINER.to_string()],
            Some(CONTAINER) => cores(self.h()).into_iter().filter(|c| c.visible).map(|c| c.id).collect(),
            Some(_) => vec![],
        };
        Ok(vec![String::new(), j(&ids)])
    }

    fn rc_context(&self, a: &Args) -> Reply {
        let id = a.string(0)?;
        if id == CONTAINER {
            let name = chip_name(&cores(self.h()));
            return Ok(vec![
                String::new(),
                json!({ "ID": CONTAINER, "Name": name, "IsContainer": true, "HasState": false,
                        "CanSuspend": false, "CanResume": 0, "CanCount": 0, "CanTerminate": false,
                        "RCGroup": CONTAINER, "SymbolsGroup": CONTAINER, "WordSize": 4 })
                .to_string(),
            ]);
        }
        let c = self.core(&id)?;
        if owning_core(&id).as_deref() != Some(id.as_str()) {
            return Err(err(ERR_INV_CONTEXT, format!("Invalid context: {id}")));
        }
        Ok(vec![String::new(), crate::contexts::context_json(&c).to_string()])
    }

    fn rc_state(&self, a: &Args) -> Reply {
        let c = self.core(&a.string(0)?)?;
        if !c.suspended() {
            return Ok(vec![String::new(), "false".into(), "0".into(), "null".into(), "null".into()]);
        }
        let pc = read_pc(self.h(), c.idx).unwrap_or(0);
        Ok(vec![
            String::new(),
            "true".into(),
            pc.to_string(),
            j(reason_text(c.reason)),
            json!({ "StateName": c.reason.name(), "Context": c.id }).to_string(),
        ])
    }

    fn rc_suspend(&self, a: &Args) -> Reply {
        let c = self.core(&a.string(0)?)?;
        if let Some(cancel) = self.shared.stepping.lock().unwrap().get(&c.idx) {
            // Pause during a source-level step: stop stepping.
            cancel.store(true, Ordering::Relaxed);
            return Ok(vec![String::new()]);
        }
        if c.suspended() {
            return Err(err(ERR_ALREADY_STOPPED, "Already stopped"));
        }
        let idx = c.idx;
        self.h()
            .call(move |cx| ops::halt(cx, idx).map_err(|e| e.to_string()))
            .unwrap_or_else(|| Err("server stopped".into()))
            .map_err(|e| err(ERR_OTHER, e))?;
        Ok(vec![String::new()])
    }

    fn rc_resume(&self, a: &Args) -> Reply {
        let c = self.core(&a.string(0)?)?;
        let mode = a.u64(1).unwrap_or(0);
        if !c.suspended() {
            return Err(err(ERR_ALREADY_RUNNING, "Already running"));
        }
        if self.shared.is_stepping(c.idx) {
            return Err(err(ERR_ALREADY_RUNNING, "Already running (step in progress)"));
        }
        let idx = c.idx;
        let r = match mode {
            0 => self.h().call(move |cx| ops::resume(cx, idx, None).map_err(|e| e.to_string())),
            // Source-level step over / into / out: asynchronous, reported by contextSuspended.
            3..=5 => {
                crate::stepping::start(self.shared.clone(), idx, c.id.clone(), mode);
                return Ok(vec![String::new()]);
            }
            _ => {
                // Instruction step.
                self.shared.broadcast(&wire::event("RunControl", "contextResumed", &[j(&c.id)]));
                self.h().call(move |cx| ops::step(cx, idx, None).map_err(|e| e.to_string()))
            }
        };
        r.unwrap_or_else(|| Err("server stopped".into())).map_err(|e| err(ERR_OTHER, e))?;
        Ok(vec![String::new()])
    }

    // --- Memory ----------------------------------------------------------------------

    /// The target for a memory access of a core context to addresses the core accesses through
    /// its own path (HSM-private memory, MCS program RAM).
    fn own_access(&self, ctx: &str, addr: u32, size: usize) -> Option<usize> {
        let idx = find_core(self.h(), ctx)?.idx;
        let last = addr.wrapping_add(size.saturating_sub(1) as u32);
        self.h()
            .call(move |cx| cx.with(|c| c.targets.get(idx).is_some_and(|t| t.core.owns_address(addr) || t.core.owns_address(last))))
            .unwrap_or(false)
            .then_some(idx)
    }

    fn mem_error(addr: u32, size: usize, msg: String) -> String {
        json!([{ "addr": addr as u64, "size": size, "stat": 0x300, "msg": { "Code": ERR_INV_ADDRESS, "Format": msg } }]).to_string()
    }

    fn mem_get(&self, a: &Args) -> Reply {
        let ctx = a.string(0)?;
        let addr = a.u64(1)? as u32;
        let size = a.u64(3)? as usize;
        if let Some(t) = self.own_access(&ctx, addr, size) {
            let r = self
                .h()
                .call(move |cx| ops::read_memory_as(cx, t, addr, size).map_err(|e| e.to_string()))
                .unwrap_or_else(|| Err("server stopped".into()));
            return Ok(match r {
                Ok(data) => vec![j(&B64.encode(&data)), String::new(), "null".into()],
                Err(e) => vec![j(&B64.encode(vec![0u8; size])), String::new(), Self::mem_error(addr, size, e)],
            });
        }
        let res = self
            .h()
            .call(move |cx| {
                cx.with(|c| -> Result<tas::OpResult, String> {
                    let mut b = tas::Batch::new();
                    b.read(addr as u64, size);
                    let mut r = c.io().map_err(|e| e.to_string())?.batch(&b).map_err(|e| e.to_string())?;
                    Ok(r.remove(0))
                })
            })
            .unwrap_or_else(|| Err("server stopped".into()))
            .map_err(|e| err(ERR_INV_ADDRESS, e))?;
        let err_addrs = if res.ok() {
            "null".to_string()
        } else {
            let ok = res.bytes_ok.min(size);
            json!([{ "addr": addr as u64 + ok as u64, "size": size - ok, "stat": 0x300,
                     "msg": { "Code": ERR_INV_ADDRESS, "Format": tas::pl_err_name(res.err) } }])
            .to_string()
        };
        Ok(vec![j(&B64.encode(&res.data)), String::new(), err_addrs])
    }

    fn mem_set(&self, a: &Args) -> Reply {
        let ctx = a.string(0)?;
        let addr = a.u64(1)? as u32;
        let size = a.u64(3)? as usize;
        let data = B64.decode(a.string(5)?).map_err(|e| err(ERR_JSON_SYNTAX, format!("base64: {e}")))?;
        let data: Vec<u8> = data.into_iter().take(size).collect();
        let len = data.len();
        if let Some(t) = self.own_access(&ctx, addr, len) {
            let r = self
                .h()
                .call(move |cx| ops::write_memory_as(cx, t, addr, &data).map_err(|e| e.to_string()))
                .unwrap_or_else(|| Err("server stopped".into()));
            return Ok(match r {
                Ok(()) => vec!["null".into(), String::new(), "null".into()],
                Err(e) => vec!["null".into(), String::new(), Self::mem_error(addr, len, e)],
            });
        }
        let res = self
            .h()
            .call(move |cx| {
                cx.with(|c| -> Result<tas::OpResult, String> {
                    let mut b = tas::Batch::new();
                    b.write(addr as u64, &data);
                    let mut r = c.io().map_err(|e| e.to_string())?.batch(&b).map_err(|e| e.to_string())?;
                    Ok(r.remove(0))
                })
            })
            .unwrap_or_else(|| Err("server stopped".into()))
            .map_err(|e| err(ERR_INV_ADDRESS, e))?;
        // Same layout as Memory.get (no data field value): the extension's parser takes a
        // reply with three or more fields as [data, error, errorAddresses].
        if res.ok() {
            Ok(vec!["null".into(), String::new(), "null".into()])
        } else {
            let ok = res.bytes_ok.min(len);
            Ok(vec![
                "null".into(),
                String::new(),
                json!([{ "addr": addr as u64 + ok as u64, "size": len - ok, "stat": 0x300,
                         "msg": { "Code": ERR_INV_ADDRESS, "Format": tas::pl_err_name(res.err) } }])
                .to_string(),
            ])
        }
    }

    // --- Registers -------------------------------------------------------------------

    fn reg_children(&self, a: &Args) -> Reply {
        let id = a.string(0)?;
        let c = self.core(&id)?;
        let ids: Vec<String> = if id == c.id {
            vec![format!("{}.RG.core", c.id), format!("{}.RG.debug", c.id)]
        } else if id == format!("{}.RG.core", c.id) {
            c.arch.regs.iter().filter(|r| r.group != RegGroup::Debug).map(|r| format!("{}.R.{}", c.id, r.name)).collect()
        } else if id == format!("{}.RG.debug", c.id) {
            c.arch.regs.iter().filter(|r| r.group == RegGroup::Debug).map(|r| format!("{}.R.{}", c.id, r.name)).collect()
        } else {
            vec![]
        };
        Ok(vec![String::new(), j(&ids)])
    }

    fn reg_index(&self, id: &str) -> Result<(CoreInfo, usize), TcfError> {
        let c = self.core(id)?;
        let name = id.strip_prefix(&format!("{}.R.", c.id)).ok_or_else(|| err(ERR_INV_CONTEXT, format!("Invalid register: {id}")))?;
        let idx = c.arch.regs.iter().position(|r| r.name == name).ok_or_else(|| err(ERR_INV_CONTEXT, format!("Invalid register: {id}")))?;
        Ok((c, idx))
    }

    fn reg_context(&self, a: &Args) -> Reply {
        let id = a.string(0)?;
        let c = self.core(&id)?;
        if let Some(group) = id.strip_prefix(&format!("{}.RG.", c.id)) {
            let (name, desc) = match group {
                "core" => ("Core Registers", c.arch.description),
                "debug" => ("Debug Registers", "Debug registers of the core"),
                _ => return Err(err(ERR_INV_CONTEXT, format!("Invalid register group: {id}"))),
            };
            return Ok(vec![String::new(), json!({ "ID": id, "ParentID": c.id, "ProcessID": c.id, "Name": name, "Description": desc }).to_string()]);
        }
        let (c, idx) = self.reg_index(&id)?;
        let r = &c.arch.regs[idx];
        let mut ctx = json!({
            "ID": id,
            "ParentID": format!("{}.RG.{}", c.id, if r.group == RegGroup::Debug { "debug" } else { "core" }),
            "ProcessID": c.id,
            "Name": r.name,
            "Description": r.description,
            "Size": 4,
            "Readable": true,
            "Writeable": true,
            "ReadOnce": false,
            "WriteOnce": false,
            "SideEffects": false,
            "Volatile": r.group == RegGroup::Debug,
            "Float": false,
            "BigEndian": false,
            "LeftToRight": false,
            "FirstBit": 0,
            "MemoryAddress": 0,
            "MemoryContext": c.id,
        });
        if let Some(d) = r.dwarf {
            ctx["DwarfID"] = json!(d);
        }
        let role = match idx {
            i if i == c.arch.pc => Some("PC"),
            i if i == c.arch.sp => Some("SP"),
            i if i == c.arch.ra => Some("RET"),
            _ => None,
        };
        if let Some(role) = role {
            ctx["Role"] = json!(role);
        }
        Ok(vec![String::new(), ctx.to_string()])
    }

    fn reg_get(&self, a: &Args) -> Reply {
        let (c, idx) = self.reg_index(&a.string(0)?)?;
        let ti = c.idx;
        let v = self
            .h()
            .call(move |cx| ops::read_reg(cx, ti, idx).map_err(|e| e.to_string()))
            .unwrap_or_else(|| Err("server stopped".into()))
            .map_err(|e| err(ERR_OTHER, e))?;
        Ok(vec![String::new(), j(&B64.encode(v.to_le_bytes()))])
    }

    fn reg_set(&self, a: &Args) -> Reply {
        let (c, idx) = self.reg_index(&a.string(0)?)?;
        if c.arch.regs[idx].group != RegGroup::Debug && c.state != TargetState::Halted {
            return Err(err(ERR_IS_RUNNING, "Execution context is running"));
        }
        let bytes = B64.decode(a.string(1)?).map_err(|e| err(ERR_JSON_SYNTAX, format!("base64: {e}")))?;
        let mut w = [0u8; 4];
        let n = bytes.len().min(4);
        w[..n].copy_from_slice(&bytes[..n]);
        let v = u32::from_le_bytes(w);
        let ti = c.idx;
        self.h()
            .call(move |cx| ops::write_reg(cx, ti, idx, v).map_err(|e| e.to_string()))
            .unwrap_or_else(|| Err("server stopped".into()))
            .map_err(|e| err(ERR_OTHER, e))?;
        Ok(vec![String::new()])
    }

    // --- Breakpoints -----------------------------------------------------------------

    fn bp_list(v: Value) -> Result<Vec<Map<String, Value>>, TcfError> {
        match v {
            Value::Null => Ok(vec![]),
            Value::Array(items) => items
                .into_iter()
                .map(|i| match i {
                    Value::Object(m) => Ok(m),
                    other => Err(err(ERR_JSON_SYNTAX, format!("breakpoint must be an object, got {other}"))),
                })
                .collect(),
            other => Err(err(ERR_JSON_SYNTAX, format!("expected breakpoint list, got {other}"))),
        }
    }

    fn apply(&self, list: Vec<Map<String, Value>>, replace_all: bool) -> Reply {
        self.shared.apply_breakpoints(list, replace_all, self.id).map_err(|e| err(ERR_JSON_SYNTAX, e))?;
        Ok(vec![String::new()])
    }

    fn bp_set(&self, a: &Args) -> Reply {
        let list = Self::bp_list(a.value(0)?)?;
        self.apply(list, true)
    }

    fn bp_add(&self, a: &Args) -> Reply {
        match a.value(0)? {
            Value::Object(m) => self.apply(vec![m], false),
            other => Err(err(ERR_JSON_SYNTAX, format!("expected breakpoint object, got {other}"))),
        }
    }

    fn bp_remove(&self, a: &Args) -> Reply {
        let ids: Vec<String> = serde_json::from_value(a.value(0)?).map_err(|e| err(ERR_JSON_SYNTAX, e.to_string()))?;
        self.shared.remove_breakpoints(ids);
        Ok(vec![String::new()])
    }

    fn bp_enable(&self, a: &Args, enable: bool) -> Reply {
        let ids: Vec<String> = serde_json::from_value(a.value(0)?).map_err(|e| err(ERR_JSON_SYNTAX, e.to_string()))?;
        let list: Vec<Map<String, Value>> = {
            let t = self.shared.bps.lock().unwrap();
            ids.iter()
                .filter_map(|id| t.bps.get(id))
                .map(|bp| {
                    let mut p = bp.props.clone();
                    p.insert("Enabled".into(), json!(enable));
                    p
                })
                .collect()
        };
        self.apply(list, false)
    }

    // --- StackTrace --------------------------------------------------------------------

    fn frames(&self, c: &CoreInfo) -> Result<Vec<Frame>, TcfError> {
        if let Some(f) = self.shared.stacks.lock().unwrap().get(&c.idx) {
            return Ok(f.clone());
        }
        let frames = crate::stack::unwind_core(&self.shared, c.idx).map_err(|e| err(ERR_OTHER, e))?;
        self.shared.stacks.lock().unwrap().insert(c.idx, frames.clone());
        Ok(frames)
    }

    fn st_children(&self, a: &Args, range: Option<(i64, i64)>) -> Reply {
        let id = a.string(0)?;
        if id == CONTAINER {
            return Ok(vec![String::new(), "[]".into()]);
        }
        let c = self.core(&id)?;
        if !c.suspended() || self.shared.is_stepping(c.idx) {
            return Err(err(ERR_IS_RUNNING, "Execution context is running"));
        }
        let n = self.frames(&c)?.len();
        let ids: Vec<String> = match range {
            // getChildren: bottom (outermost) frame first
            None => (0..n).rev().map(|i| format!("{}.F{i}", c.id)).collect(),
            // getChildrenRange: frame indices, top frame first
            Some((s, e)) => (0..n).filter(|&i| i as i64 >= s && i as i64 <= e).map(|i| format!("{}.F{i}", c.id)).collect(),
        };
        Ok(vec![String::new(), j(&ids)])
    }

    fn st_context(&self, a: &Args) -> Reply {
        let ids: Vec<String> = serde_json::from_value(a.value(0)?).map_err(|e| err(ERR_JSON_SYNTAX, e.to_string()))?;
        let mut out = Vec::new();
        for fid in ids {
            let c = self.core(&fid)?;
            let syms = self.shared.symbols_for(&c.id);
            let index: usize = fid
                .rsplit_once(".F")
                .and_then(|(_, n)| n.parse().ok())
                .ok_or_else(|| err(ERR_INV_CONTEXT, format!("Invalid stack frame: {fid}")))?;
            let frames = self.frames(&c)?;
            let f = frames.get(index).ok_or_else(|| err(ERR_INV_CONTEXT, format!("Invalid stack frame: {fid}")))?;
            let mut ctx = json!({ "ID": fid, "ParentID": c.id, "ProcessID": c.id, "TopFrame": index == 0,
                                  "Index": index, "IP": f.pc });
            if let Some(sp) = f.sp() {
                ctx["FP"] = json!(sp);
            }
            if let Some(ra) = f.ra() {
                ctx["RP"] = json!(ra);
            }
            if let Some(s) = &syms {
                // Caller frames hold return addresses: look up the call instruction.
                let look = if index == 0 { f.pc } else { f.pc.wrapping_sub(1) };
                if let Some((e, s)) = s.for_addr(look) {
                    if let Some(fi) = s.function_index_at(look) {
                        ctx["Name"] = json!(s.functions[fi].name);
                        ctx["FuncID"] = json!(elf_id(*e, &format!("FN:{fi}")));
                    }
                }
                if let Some(li) = s.line_at(look) {
                    ctx["CodeArea"] = code_area(&li);
                }
            }
            out.push(ctx);
        }
        Ok(vec![j(&out), String::new()])
    }

}

fn code_area(li: &LineInfo) -> Value {
    json!({ "File": li.file, "SLine": li.line, "SCol": li.column, "ELine": li.line,
            "SAddr": li.start, "EAddr": li.end, "IsStmt": true })
}
