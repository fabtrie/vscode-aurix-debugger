//! Core state shared between the Tcl commands and the server loop.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use jim::Interp;

use crate::adapter::{Adapter, AdapterSettings, Io};
use crate::daq;
use crate::error::{Error, Result};
use crate::flash::FlashBank;
use crate::target::TargetEntry;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Config,
    Exec,
}

#[derive(Clone, Debug)]
pub struct Tap {
    pub name: String,
    pub irlen: u32,
    pub expected_ids: Vec<u32>,
}

#[derive(Clone, Debug)]
pub struct Ocmts {
    pub name: String,
    pub tap: String,
}

#[derive(Clone, Debug)]
pub struct Ports {
    pub bind: String,
    pub telnet: Option<u16>,
    pub tcf: Option<u16>,
    pub gdb: Option<u16>,
    pub tcl: Option<u16>,
    /// REST / MCP / Swagger server.
    pub api: Option<u16>,
}

/// Run-state poll period unless the cfg (`polling_period`) or the adapter sets another.
pub const DEFAULT_POLL_PERIOD_MS: u64 = 10;

pub struct Ctx {
    pub mode: Mode,
    pub adapter_name: Option<String>,
    /// Forces a driver regardless of `adapter driver` (tests, `--adapter`).
    pub adapter_override: Option<String>,
    pub adapter: Option<Box<dyn Adapter>>,
    pub adapter_settings: AdapterSettings,
    pub transport: Option<String>,
    pub reset_config: Vec<String>,
    pub taps: Vec<Tap>,
    pub ocmts: Vec<Ocmts>,
    pub targets: Vec<TargetEntry>,
    pub current: Option<usize>,
    /// Target selected while an event handler runs.
    pub override_stack: Vec<usize>,
    pub active_events: HashSet<(usize, String)>,
    pub flash_banks: Vec<FlashBank>,
    pub search_dirs: Vec<PathBuf>,
    pub poll_enabled: bool,
    pub poll_period_ms: u64,
    /// Rotates which plain-memory cores a run-control probe polls this period
    /// (see `ops::poll_all`).
    pub poll_turn: usize,
    /// Periodic sampling; idle unless a frontend opened a session.
    pub daq: daq::Registry,
    /// How far the sampler may read past what was asked for to fold two points into one bus
    /// access (`daq gap <bytes>`; 0 merges only adjacent points). See `daq::DEFAULT_COALESCE_GAP`.
    pub daq_gap: u32,
    /// Let the run-state poll carry the DAQ points that are due, instead of the sampler paying a
    /// second round trip right behind it (`daq merge <0|1>`). Runtime-settable for the same
    /// reason a measurement knob is useful at all: so the two can be compared in one session.
    pub daq_merge: bool,
    pub ports: Ports,
    pub next_bp_id: u32,
    pub initialized: bool,
    pub noinit: bool,
    pub shutdown: bool,
    pub in_reset: bool,
    pub srst_asserted: bool,
    /// Tcl proc that handles every reset request (`reset_hook`), called with the mode.
    pub reset_hook: Option<String>,
    /// Set while the hook runs: a `reset` inside it is the built-in one.
    pub in_reset_hook: bool,
    /// The adapter's [`crate::adapter::LinkHealth`] last reported to the frontends.
    pub health: Option<crate::adapter::LinkHealth>,
    /// Since when the adapter reports the probe lost.
    pub probe_lost_since: Option<std::time::Instant>,
    /// `Event::ProbeLost` was sent for the current loss.
    pub probe_lost_reported: bool,
    /// How long the probe may be gone before the frontends end their sessions
    /// (`probe_lost_timeout <ms>`); 0: never, keep reconnecting.
    pub probe_lost_timeout_ms: u64,
}

/// See [`Ctx::probe_lost_timeout_ms`].
pub const DEFAULT_PROBE_LOST_TIMEOUT_MS: u64 = 1000;

impl Default for Ctx {
    fn default() -> Self {
        Self::new()
    }
}

impl Ctx {
    pub fn new() -> Self {
        Ctx {
            mode: Mode::Config,
            adapter_name: None,
            adapter_override: None,
            adapter: None,
            adapter_settings: AdapterSettings::default(),
            transport: None,
            reset_config: Vec::new(),
            taps: Vec::new(),
            ocmts: Vec::new(),
            targets: Vec::new(),
            current: None,
            override_stack: Vec::new(),
            active_events: HashSet::new(),
            flash_banks: Vec::new(),
            search_dirs: Vec::new(),
            poll_enabled: true,
            poll_period_ms: DEFAULT_POLL_PERIOD_MS,
            poll_turn: 0,
            daq: daq::Registry::default(),
            daq_gap: daq::DEFAULT_COALESCE_GAP,
            daq_merge: true,
            ports: Ports { bind: "127.0.0.1".into(), telnet: Some(4444), tcf: Some(1534), gdb: None, tcl: None, api: Some(8080) },
            next_bp_id: 1,
            initialized: false,
            noinit: false,
            shutdown: false,
            in_reset: false,
            srst_asserted: false,
            reset_hook: None,
            in_reset_hook: false,
            health: None,
            probe_lost_since: None,
            probe_lost_reported: false,
            probe_lost_timeout_ms: DEFAULT_PROBE_LOST_TIMEOUT_MS,
        }
    }

    /// Target addressed by commands: event override, else `targets` selection.
    pub fn current_target(&self) -> Result<usize> {
        self.override_stack
            .last()
            .copied()
            .or(self.current)
            .filter(|&t| t < self.targets.len())
            .ok_or_else(|| Error::Usage("No current target".into()))
    }

    pub fn target_by_name(&self, name: &str) -> Option<usize> {
        self.targets.iter().position(|t| t.name == name).or_else(|| {
            name.parse::<usize>().ok().filter(|&i| i < self.targets.len())
        })
    }

    pub fn io(&mut self) -> Result<Io<'_>> {
        match self.adapter.as_deref_mut() {
            Some(a) if a.initialized() => Ok(Io::new(a)),
            _ => Err(Error::Usage("The debug adapter is not initialized (run 'init')".into())),
        }
    }

    /// Adapter access together with a mutable target entry.
    pub fn target_io(&mut self, t: usize) -> Result<(Io<'_>, &mut TargetEntry)> {
        let Ctx { adapter, targets, .. } = self;
        let a = match adapter.as_deref_mut() {
            Some(a) if a.initialized() => a,
            _ => return Err(Error::Usage("The debug adapter is not initialized (run 'init')".into())),
        };
        let e = targets.get_mut(t).ok_or_else(|| Error::Usage(format!("invalid target {t}")))?;
        Ok((Io::new(a), e))
    }
}

pub type Shared = Rc<RefCell<Ctx>>;

/// Everything a command or operation needs: the interpreter and the state.
///
/// Rule: never evaluate Tcl while holding a borrow of the state - use
/// [`Cx::with`] for short state accesses only.
#[derive(Clone, Copy)]
pub struct Cx<'a> {
    pub interp: &'a Interp,
    pub sh: &'a Shared,
}

impl<'a> Cx<'a> {
    pub fn with<R>(&self, f: impl FnOnce(&mut Ctx) -> R) -> R {
        f(&mut self.sh.borrow_mut())
    }
}
