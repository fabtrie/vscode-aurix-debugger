//! TCF frontend: serves the VS Code aurix-tcf extension on top of the
//! protocol-neutral core ([`aurix_core::ServerHandle`]).
//!
//! Wire format: messages are `\0`-separated fields ending with `\x03\x01`.
//! Arguments and results are JSON; binary data is base64 in JSON strings.

mod breakpoints;
mod channel;
mod contexts;
mod disassembly;
mod expressions;
mod forwarder;
mod osaware;
mod profile;
mod stack;
mod stepping;
mod symstore;
mod wire;

use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{Map, Value};

use aurix_core::unwind::Frame;
use aurix_core::{log_info, log_warn, ServerHandle};

use breakpoints::BpTable;
use channel::Writer;
use expressions::ExprTable;
pub use expressions::{evaluate_global, symbol_ref, GlobalError, GlobalValue};
pub use symstore::{elf_id, Elf, SymbolSet, SymbolStore};

pub use breakpoints::{Bp, Instance};
pub use contexts::{core_id, cores, owning_core, CoreInfo, CONTAINER};

/// Owner of the breakpoints set through the REST/MCP API (TCF channel IDs start at 1). They
/// stay armed when a TCF client disconnects.
pub const API_OWNER: u64 = 0;

/// Session changes for frontends other than TCF (REST/MCP).
#[derive(Clone, Debug)]
pub enum SessionEvent {
    /// Breakpoint IDs added, changed or removed by any client.
    Breakpoints { added: Vec<String>, changed: Vec<String>, removed: Vec<String> },
}

/// Services announced in `Locator.Hello`.
pub const SERVICES: &[&str] = &[
    "Locator",
    "AurixOS",
    "ContextQuery",
    "RunControl",
    "Memory",
    "Registers",
    "Breakpoints",
    "StackTrace",
    "LineNumbers",
    "Symbols",
    "Expressions",
    "Disassembly",
    "MemoryMap",
    "ContextReset",
    "Tcl",
];

/// State shared by all TCF connections.
pub struct Shared {
    pub handle: ServerHandle,
    pub channels: Mutex<HashMap<u64, Writer>>,
    pub bps: Mutex<BpTable>,
    /// MemoryMap.set per context: the ELF files to load symbols from.
    pub memory_maps: Mutex<HashMap<String, serde_json::Value>>,
    pub symbols: Mutex<SymbolStore>,
    /// Unwound call stacks per target, valid until the target runs again.
    pub stacks: Mutex<HashMap<usize, Vec<Frame>>>,
    /// Targets doing a source-level step, with their cancel flag.
    pub stepping: Mutex<HashMap<usize, Arc<AtomicBool>>>,
    pub exprs: Mutex<ExprTable>,
    /// The loaded ORTI file with its attributes planned, shared by every frontend. Handed out as
    /// an `Arc` so reading a large object table does not hold the lock while it talks to the core.
    pub orti: Mutex<Option<Arc<orti::Awareness>>>,
    /// The running task profiler, while one is started.
    pub profile: Mutex<Option<Arc<profile::Profile>>>,
    /// Receivers of [`SessionEvent`]s (bounded: a receiver that does not keep up misses events).
    listeners: Mutex<Vec<SyncSender<SessionEvent>>>,
    next_id: AtomicU64,
}

impl Shared {
    pub fn broadcast(&self, msg: &[u8]) {
        let chans: Vec<Writer> = self.channels.lock().unwrap().values().cloned().collect();
        for w in chans {
            w.send(msg);
        }
    }

    pub fn subscribe(&self) -> Receiver<SessionEvent> {
        let (tx, rx) = sync_channel(256);
        self.listeners.lock().unwrap().push(tx);
        rx
    }

    fn notify(&self, ev: SessionEvent) {
        self.listeners.lock().unwrap().retain(|l| !matches!(l.try_send(ev.clone()), Err(TrySendError::Disconnected(_))));
    }

    fn announce(&self, added: &[Map<String, Value>], changed: &[Map<String, Value>], statuses: &[(String, Value)]) {
        if !added.is_empty() {
            self.broadcast(&wire::event("Breakpoints", "contextAdded", &[json(added)]));
        }
        if !changed.is_empty() {
            self.broadcast(&wire::event("Breakpoints", "contextChanged", &[json(changed)]));
        }
        for (id, st) in statuses {
            self.broadcast(&wire::event("Breakpoints", "status", &[json(id), st.to_string()]));
        }
        let ids = |l: &[Map<String, Value>]| l.iter().filter_map(|p| p.get("ID").and_then(Value::as_str).map(str::to_string)).collect::<Vec<_>>();
        if !statuses.is_empty() {
            self.notify(SessionEvent::Breakpoints { added: ids(added), changed: ids(changed), removed: vec![] });
        }
    }

    fn announce_removed(&self, removed: &[String]) {
        if !removed.is_empty() {
            self.broadcast(&wire::event("Breakpoints", "contextRemoved", &[json(removed)]));
            self.notify(SessionEvent::Breakpoints { added: vec![], changed: vec![], removed: removed.to_vec() });
        }
    }

    /// Adds or changes breakpoints of `owner` (TCF properties). With `replace_all`
    /// (`Breakpoints.set`), the other breakpoints of `owner` are removed.
    pub fn apply_breakpoints(&self, list: Vec<Map<String, Value>>, replace_all: bool, owner: u64) -> Result<(), String> {
        let mut added = Vec::new();
        let mut changed = Vec::new();
        let mut statuses = Vec::new();
        let removed: Vec<String>;
        let syms = |ctx: &str| self.symbols_for(ctx);
        {
            let mut t = self.bps.lock().unwrap();
            let new_ids: Vec<String> = list.iter().filter_map(|p| p.get("ID").and_then(Value::as_str).map(str::to_string)).collect();
            // Only the client's own breakpoints: another client's (or the API's) stay.
            removed = if replace_all {
                t.bps.iter().filter(|(id, b)| b.owner == owner && !new_ids.contains(id)).map(|(id, _)| id.clone()).collect()
            } else {
                vec![]
            };
            for id in &removed {
                t.remove(&self.handle, id);
            }
            for props in list {
                let unchanged = props
                    .get("ID")
                    .and_then(Value::as_str)
                    .and_then(|id| t.bps.get(id))
                    .is_some_and(|old| old.props == props);
                if unchanged {
                    continue;
                }
                // A change (e.g. enable) keeps the breakpoint with the client that created it.
                let own = props.get("ID").and_then(Value::as_str).and_then(|id| t.bps.get(id)).map_or(owner, |b| b.owner);
                let (id, existed) = t.insert(&self.handle, props, own, &syms)?;
                let bp = &t.bps[&id];
                if bp.planted() || !bp.enabled() {
                    if existed {
                        changed.push(bp.props.clone());
                    } else {
                        added.push(bp.props.clone());
                    }
                }
                statuses.push((id.clone(), t.status(&id)));
            }
        }
        self.announce_removed(&removed);
        self.announce(&added, &changed, &statuses);
        Ok(())
    }

    /// Removes breakpoints by ID. Returns the IDs that existed.
    pub fn remove_breakpoints(&self, ids: Vec<String>) -> Vec<String> {
        let removed: Vec<String> = {
            let mut t = self.bps.lock().unwrap();
            ids.into_iter().filter(|id| t.remove(&self.handle, id)).collect()
        };
        self.announce_removed(&removed);
        removed
    }

    /// Removes the breakpoints of a closed channel: they must not stay armed.
    fn remove_owned(&self, owner: u64) {
        let removed: Vec<String> = {
            let mut table = self.bps.lock().unwrap();
            let ids = table.owned_by(owner);
            for bid in &ids {
                table.remove(&self.handle, bid);
            }
            ids
        };
        self.announce_removed(&removed);
    }

    /// Re-plants the breakpoints that depend on symbols after symbol files changed.
    pub fn reresolve_breakpoints(&self) {
        let mut added = Vec::new();
        let mut statuses = Vec::new();
        {
            let syms = |ctx: &str| self.symbols_for(ctx);
            let mut t = self.bps.lock().unwrap();
            for id in t.reresolve(&self.handle, &syms) {
                if t.bps[&id].planted() {
                    added.push(t.bps[&id].props.clone());
                }
                statuses.push((id.clone(), t.status(&id)));
            }
        }
        self.announce(&added, &[], &statuses);
    }

    /// All breakpoints by ID.
    pub fn breakpoints(&self) -> Vec<(String, Bp)> {
        self.bps.lock().unwrap().bps.iter().map(|(id, b)| (id.clone(), b.clone())).collect()
    }

    /// The breakpoint `id` with its TCF status (`Instances`, `Error`).
    pub fn breakpoint(&self, id: &str) -> Option<(Bp, Value)> {
        let t = self.bps.lock().unwrap();
        t.bps.get(id).map(|b| (b.clone(), t.status(id)))
    }

    /// A new breakpoint ID for [`API_OWNER`] breakpoints.
    pub fn next_api_breakpoint_id(&self) -> String {
        format!("api-{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Cancels a source-level step of `target`. Returns false if none runs.
    pub fn cancel_step(&self, target: usize) -> bool {
        match self.stepping.lock().unwrap().get(&target) {
            Some(cancel) => {
                cancel.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// Symbol set of the core a context ID belongs to (frames, registers, ...). The chip
    /// context or an empty ID use the first core with symbols.
    pub fn symbols_for(&self, ctx: &str) -> Option<Arc<SymbolSet>> {
        let store = self.symbols.lock().unwrap();
        match contexts::owning_core(ctx) {
            Some(core) => store.set(&core),
            None => {
                let mut ctxs = store.contexts();
                ctxs.sort_by_key(|c| contexts::owning_core(c).and_then(|c| c.rsplit_once(".C").and_then(|(_, n)| n.parse::<u32>().ok())));
                ctxs.iter().find_map(|c| store.set(c))
            }
        }
    }

    /// Source file and line of `addr` on the core of context `ctx`, if its symbols are loaded.
    pub fn source_line(&self, ctx: &str, addr: u32) -> Option<(String, u32)> {
        self.symbols_for(ctx)?.line_at(addr).map(|li| (li.file, li.line))
    }

    /// A loaded ELF by its number (symbol IDs `E<n>/...`).
    pub fn elf(&self, n: usize) -> Option<Arc<symbols::Symbols>> {
        self.symbols.lock().unwrap().elf(n)
    }

    pub fn is_stepping(&self, target: usize) -> bool {
        self.stepping.lock().unwrap().contains_key(&target)
    }

    pub fn any_stepping(&self) -> bool {
        !self.stepping.lock().unwrap().is_empty()
    }

    pub fn invalidate_stack(&self, target: usize) {
        self.stacks.lock().unwrap().remove(&target);
    }
}

fn json<T: serde::Serialize + ?Sized>(v: &T) -> String {
    serde_json::to_string(v).unwrap()
}

/// The debug session state shared by the TCF connections and the other frontends
/// (breakpoints, symbols, source-level stepping).
pub fn session(handle: ServerHandle) -> Arc<Shared> {
    Arc::new(Shared {
        handle,
        channels: Mutex::new(HashMap::new()),
        bps: Mutex::new(BpTable::default()),
        memory_maps: Mutex::new(HashMap::new()),
        symbols: Mutex::new(SymbolStore::default()),
        stacks: Mutex::new(HashMap::new()),
        stepping: Mutex::new(HashMap::new()),
        exprs: Mutex::new(ExprTable::default()),
        orti: Mutex::new(None),
        profile: Mutex::new(None),
        listeners: Mutex::new(Vec::new()),
        next_id: AtomicU64::new(1),
    })
}

/// Starts the TCF listener with a new session. Returns the bound port.
pub fn start(handle: ServerHandle, bind: &str, port: u16) -> std::io::Result<u16> {
    listen(&session(handle), bind, port)
}

/// Starts the TCF listener on `shared` (once per session). Returns the bound port.
pub fn listen(shared: &Arc<Shared>, bind: &str, port: u16) -> std::io::Result<u16> {
    let listener = TcpListener::bind((aurix_core::telnet::bind_address(bind), port))?;
    let actual = listener.local_addr()?.port();
    let shared = shared.clone();
    forwarder::start(shared.clone());
    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
                    let sh = shared.clone();
                    thread::spawn(move || {
                        let peer = s.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                        log_info!("TCF: client {peer} connected");
                        channel::run(sh, id, s);
                        log_info!("TCF: client {peer} disconnected");
                    });
                }
                Err(e) => log_warn!("TCF: accept failed: {e}"),
            }
        }
    });
    Ok(actual)
}
