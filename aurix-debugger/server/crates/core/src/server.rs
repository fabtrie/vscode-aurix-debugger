//! The core thread: owns the interpreter and the target state, serves
//! frontend requests and polls the targets.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::Once;
use std::time::{Duration, Instant};

use jim::{CmdResult, Interp};

use crate::commands;
use crate::ctx::{Ctx, Cx, Shared};
use crate::events::{self, Event};
use crate::log;
use crate::ops;

/// Work sent from frontend threads to the core thread.
pub type Job = Box<dyn for<'a, 'b> FnOnce(&'b Cx<'a>) + Send>;

/// Thread-safe handle frontends use to talk to the core.
#[derive(Clone)]
pub struct ServerHandle {
    tx: Sender<Job>,
}

impl ServerHandle {
    /// Runs `f` on the core thread and returns its result.
    pub fn call<R, F>(&self, f: F) -> Option<R>
    where
        R: Send + 'static,
        F: for<'a, 'b> FnOnce(&'b Cx<'a>) -> R + Send + 'static,
    {
        let (rtx, rrx) = channel();
        let job: Job = Box::new(move |cx| {
            let _ = rtx.send(f(cx));
        });
        self.tx.send(job).ok()?;
        rrx.recv().ok()
    }

    /// Evaluates a Tcl command line at global level.
    pub fn tcl(&self, script: &str) -> CmdResult {
        let s = script.to_string();
        self.call(move |cx| cx.interp.eval_global(&s)).unwrap_or_else(|| Err("server stopped".into()))
    }

    /// Like [`tcl`](Self::tcl), also returning the log output (e.g. `echo`)
    /// produced while the command ran.
    pub fn tcl_with_output(&self, script: &str) -> (CmdResult, Vec<String>) {
        let s = script.to_string();
        self.call(move |cx| log::capture(|| cx.interp.eval_global(&s)))
            .unwrap_or_else(|| (Err("server stopped".into()), Vec::new()))
    }

    pub fn subscribe(&self) -> Receiver<Event> {
        events::subscribe()
    }
}

pub struct Server {
    pub interp: Interp,
    pub sh: Shared,
    tx: Sender<Job>,
    rx: Receiver<Job>,
}

static LOG_TO_EVENTS: Once = Once::new();

impl Default for Server {
    fn default() -> Self {
        Self::new()
    }
}

impl Server {
    pub fn new() -> Server {
        LOG_TO_EVENTS.call_once(|| {
            log::add_sink(Box::new(|_lvl, line| events::emit(Event::Log { text: line.to_string() })));
        });
        let interp = Interp::new();
        let sh: Shared = Rc::new(RefCell::new(Ctx::new()));
        commands::register_all(&interp, &sh);
        interp.eval_global(commands::PRELUDE).expect("prelude must evaluate");
        let (tx, rx) = channel();
        Server { interp, sh, tx, rx }
    }

    pub fn cx(&self) -> Cx<'_> {
        Cx { interp: &self.interp, sh: &self.sh }
    }

    pub fn handle(&self) -> ServerHandle {
        ServerHandle { tx: self.tx.clone() }
    }

    /// Serves requests and polls targets until `shutdown`.
    pub fn run(&self) {
        let cx = self.cx();
        let mut next_poll = Instant::now();
        loop {
            if cx.with(|c| c.shutdown) {
                break;
            }
            let wait = next_poll.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(wait) {
                Ok(job) => job(&cx),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if Instant::now() >= next_poll {
                ops::poll_all(&cx);
                let period = Duration::from_millis(cx.with(|c| c.poll_period_ms).max(1));
                next_poll = Instant::now() + period;
            }
        }
        cx.with(|c| {
            if let Some(a) = c.adapter.as_deref_mut() {
                a.quit();
            }
        });
        events::emit(Event::Shutdown);
    }
}
