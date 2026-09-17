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
mod stack;
mod stepping;
mod symstore;
mod wire;

use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use aurix_core::unwind::Frame;
use aurix_core::{log_info, log_warn, ServerHandle};

use breakpoints::BpTable;
use channel::Writer;
use expressions::ExprTable;
use symstore::{SymbolSet, SymbolStore};

/// Services announced in `Locator.Hello`.
pub const SERVICES: &[&str] = &[
    "Locator",
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
    next_id: AtomicU64,
}

impl Shared {
    pub fn broadcast(&self, msg: &[u8]) {
        let chans: Vec<Writer> = self.channels.lock().unwrap().values().cloned().collect();
        for w in chans {
            w.send(msg);
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

/// Starts the TCF listener. Returns the bound port.
pub fn start(handle: ServerHandle, bind: &str, port: u16) -> std::io::Result<u16> {
    let listener = TcpListener::bind((aurix_core::telnet::bind_address(bind), port))?;
    let actual = listener.local_addr()?.port();
    let shared = Arc::new(Shared {
        handle: handle.clone(),
        channels: Mutex::new(HashMap::new()),
        bps: Mutex::new(BpTable::default()),
        memory_maps: Mutex::new(HashMap::new()),
        symbols: Mutex::new(SymbolStore::default()),
        stacks: Mutex::new(HashMap::new()),
        stepping: Mutex::new(HashMap::new()),
        exprs: Mutex::new(ExprTable::default()),
        next_id: AtomicU64::new(1),
    });
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
