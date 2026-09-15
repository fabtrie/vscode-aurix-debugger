//! Target bookkeeping shared by all target types (OpenOCD `struct target`).

use std::collections::BTreeMap;

use crate::tricore::Tricore;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetState {
    Unknown,
    Running,
    Halted,
    Reset,
    DebugRunning,
}

impl TargetState {
    pub fn name(self) -> &'static str {
        match self {
            TargetState::Unknown => "unknown",
            TargetState::Running => "running",
            TargetState::Halted => "halted",
            TargetState::Reset => "reset",
            TargetState::DebugRunning => "debug-running",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebugReason {
    NotHalted,
    DbgRq,
    Breakpoint,
    Watchpoint,
    SingleStep,
    Reset,
    /// Core not started yet (SYSCON.BHALT), not a debug halt.
    BootHalt,
    Undefined,
}

impl DebugReason {
    pub fn name(self) -> &'static str {
        match self {
            DebugReason::NotHalted => "not halted",
            DebugReason::DbgRq => "debug-request",
            DebugReason::Breakpoint => "breakpoint",
            DebugReason::Watchpoint => "watchpoint",
            DebugReason::SingleStep => "single-step",
            DebugReason::Reset => "reset",
            DebugReason::BootHalt => "boot-halt",
            DebugReason::Undefined => "undefined",
        }
    }
}

/// What a resume or step is done for. Selects the `tricore irq_mask` setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunMode {
    Continue,
    /// Instruction step, or a source-level step into.
    Step,
    StepOver,
    StepOut,
}

/// State change found by a poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transition {
    None,
    /// `boot`: only SYSCON.BHALT is set - the core was never started. No
    /// `halted` Tcl event is fired for it, since multi-core sync handlers
    /// (e.g. suspend all STMs) must not react to a core that is just not
    /// started yet.
    Halted { debug: bool, boot: bool },
    /// The core runs again without a resume from us (e.g. a raw DBGSR
    /// write by a multi-core sync script).
    Running,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BpKind {
    Hard,
    Soft,
}

#[derive(Clone, Debug)]
pub struct Breakpoint {
    pub id: u32,
    pub addr: u32,
    pub len: u32,
    pub kind: BpKind,
    pub slot: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchAccess {
    Read,
    Write,
    Access,
}

impl WatchAccess {
    /// TRxEVT.ASI bits (store = 1, load = 2).
    pub fn bits(self) -> u32 {
        match self {
            WatchAccess::Write => 1,
            WatchAccess::Read => 2,
            WatchAccess::Access => 3,
        }
    }
    pub fn letter(self) -> char {
        match self {
            WatchAccess::Read => 'r',
            WatchAccess::Write => 'w',
            WatchAccess::Access => 'a',
        }
    }
}

#[derive(Clone, Debug)]
pub struct Watchpoint {
    pub id: u32,
    pub addr: u32,
    pub len: u32,
    pub access: WatchAccess,
    pub slot: u8,
}

pub struct TargetEntry {
    pub name: String,
    pub type_name: String,
    /// Event handlers in configuration order (`-event name body`).
    pub events: Vec<(String, String)>,
    /// Generic `configure` options not interpreted elsewhere.
    pub options: BTreeMap<String, String>,
    pub chain_position: Option<String>,
    pub defer_examine: bool,
    pub core: Tricore,
    /// Consecutive failed polls / examinations (0 = healthy).
    pub poll_failures: u32,
    pub breakpoints: Vec<Breakpoint>,
    pub watchpoints: Vec<Watchpoint>,
    /// Set by `arp_reset assert` for the following deassert.
    pub reset_halt: bool,
}

impl TargetEntry {
    pub fn new(name: &str, type_name: &str) -> Self {
        TargetEntry {
            name: name.to_string(),
            type_name: type_name.to_string(),
            events: Vec::new(),
            options: BTreeMap::new(),
            chain_position: None,
            defer_examine: false,
            core: Tricore::new(),
            poll_failures: 0,
            breakpoints: Vec::new(),
            watchpoints: Vec::new(),
            reset_halt: false,
        }
    }

    pub fn event(&self, name: &str) -> Option<&str> {
        self.events.iter().find(|(n, _)| n == name).map(|(_, b)| b.as_str())
    }

    pub fn set_event(&mut self, name: &str, body: &str) {
        self.events.retain(|(n, _)| n != name);
        if !body.is_empty() {
            self.events.push((name.to_string(), body.to_string()));
        }
    }
}
