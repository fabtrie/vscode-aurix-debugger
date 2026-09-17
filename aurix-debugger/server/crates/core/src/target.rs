//! Target bookkeeping shared by all target types (OpenOCD `struct target`).

use std::collections::BTreeMap;

use tas::Batch;

use crate::adapter::Io;
use crate::arch::{ArchInfo, CORTEX_M, TRICORE};
use crate::cortex_m::CortexM;
use crate::error::Result;
use crate::tricore::{IrqMask, Tricore};

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

/// Batch operations of a poll: all of them (checked for errors) and the reads
/// whose values go to `poll_apply`.
#[derive(Clone, Debug, Default)]
pub struct PollOps {
    pub all: Vec<usize>,
    pub reads: Vec<usize>,
}

/// Run control settings shared by all core types.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RunCfg {
    /// `tl1_halt on`: halt requests also pulse OTGS trigger line 1.
    pub tl1_halt: bool,
    /// `tl1_step on`: a single step lets all TL1 suspend targets run for the
    /// instruction. The step releases this core while TL1 still holds it and
    /// then clears the TL1 hold, so all cores start in the same cycle; the step
    /// trigger's BRKOUT latches TL1 and stops them in the same cycle.
    pub tl1_step: bool,
    /// `irq_mask`: run modes executed with interrupts masked.
    pub irq_mask: IrqMask,
}

/// A core of a target: TriCore (host cores) or Cortex-M (HSM).
pub enum Core {
    Tricore(Tricore),
    CortexM(CortexM),
}

/// Calls the same method on either core type.
macro_rules! each {
    ($self:expr, $c:ident => $e:expr) => {
        match $self {
            Core::Tricore($c) => $e,
            Core::CortexM($c) => $e,
        }
    };
}

impl Core {
    /// Creates a core of a target type (`tricore`, `cortex_m`).
    pub fn for_type(type_name: &str) -> Option<Core> {
        match type_name {
            "tricore" => Some(Core::Tricore(Tricore::new())),
            "cortex_m" => Some(Core::CortexM(CortexM::new())),
            _ => None,
        }
    }

    pub const TYPES: &'static [&'static str] = &["tricore", "cortex_m"];

    pub fn arch(&self) -> &'static ArchInfo {
        match self {
            Core::Tricore(_) => &TRICORE,
            Core::CortexM(_) => &CORTEX_M,
        }
    }

    pub fn as_tricore(&self) -> Option<&Tricore> {
        match self {
            Core::Tricore(c) => Some(c),
            _ => None,
        }
    }

    pub fn as_tricore_mut(&mut self) -> Option<&mut Tricore> {
        match self {
            Core::Tricore(c) => Some(c),
            _ => None,
        }
    }

    pub fn as_cortex_m_mut(&mut self) -> Option<&mut CortexM> {
        match self {
            Core::CortexM(c) => Some(c),
            _ => None,
        }
    }

    pub fn coreid(&self) -> u32 {
        each!(self, c => c.coreid)
    }
    pub fn set_coreid(&mut self, v: u32) {
        each!(self, c => c.coreid = v)
    }
    pub fn dbgbase(&self) -> Option<u32> {
        each!(self, c => c.dbgbase)
    }
    pub fn set_dbgbase(&mut self, v: u32) {
        each!(self, c => c.dbgbase = Some(v))
    }
    pub fn ocmts(&self) -> Option<&str> {
        each!(self, c => c.ocmts.as_deref())
    }
    pub fn set_ocmts(&mut self, v: String) {
        each!(self, c => c.ocmts = Some(v))
    }
    pub fn state(&self) -> TargetState {
        each!(self, c => c.state)
    }
    pub fn reason(&self) -> DebugReason {
        each!(self, c => c.reason)
    }
    pub fn examined(&self) -> bool {
        each!(self, c => c.examined)
    }
    pub fn set_examined(&mut self, v: bool) {
        each!(self, c => c.examined = v)
    }
    pub fn run(&self) -> &RunCfg {
        each!(self, c => &c.run)
    }
    pub fn run_mut(&mut self) -> &mut RunCfg {
        each!(self, c => &mut c.run)
    }
    /// The core must be halted after a halting reset (the HSM runs depending on its firmware).
    pub fn halts_after_reset(&self) -> bool {
        matches!(self, Core::Tricore(_))
    }

    pub fn invalidate(&mut self) {
        each!(self, c => c.invalidate())
    }
    pub fn examine(&mut self, io: &mut Io) -> Result<()> {
        each!(self, c => c.examine(io))
    }
    pub fn poll_queue(&self, b: &mut Batch) -> PollOps {
        each!(self, c => c.poll_queue(b))
    }
    pub fn poll(&mut self, io: &mut Io) -> Result<Transition> {
        each!(self, c => c.poll(io))
    }
    pub fn poll_apply(&mut self, vals: &[u32]) -> Transition {
        each!(self, c => c.poll_apply(vals))
    }
    pub fn hit_trigger(&self) -> Option<usize> {
        each!(self, c => c.hit_trigger())
    }
    pub fn halt(&mut self, io: &mut Io) -> Result<()> {
        each!(self, c => c.halt(io))
    }
    pub fn resume(&mut self, io: &mut Io, addr: Option<u32>, mode: RunMode) -> Result<()> {
        each!(self, c => c.resume(io, addr, mode))
    }
    pub fn step(&mut self, io: &mut Io, addr: Option<u32>, mode: RunMode) -> Result<bool> {
        each!(self, c => c.step(io, addr, mode))
    }
    pub fn irq_masked(&self) -> bool {
        each!(self, c => c.irq_masked())
    }
    pub fn unmask_irq(&mut self, io: &mut Io) -> Result<()> {
        each!(self, c => c.unmask_irq(io))
    }
    pub fn read_reg(&mut self, io: &mut Io, idx: usize) -> Result<u32> {
        each!(self, c => c.read_reg(io, idx))
    }
    pub fn read_regs(&mut self, io: &mut Io, idxs: &[usize]) -> Result<Vec<u32>> {
        each!(self, c => c.read_regs(io, idxs))
    }
    pub fn write_reg(&mut self, io: &mut Io, idx: usize, value: u32) -> Result<()> {
        each!(self, c => c.write_reg(io, idx, value))
    }
    pub fn add_breakpoint(&mut self, io: &mut Io, addr: u32, len: u32) -> Result<u8> {
        each!(self, c => c.add_breakpoint(io, addr, len))
    }
    pub fn add_watchpoint(&mut self, io: &mut Io, addr: u32, len: u32, access: u32) -> Result<u8> {
        each!(self, c => c.add_watchpoint(io, addr, len, access))
    }
    pub fn remove_trigger(&mut self, io: &mut Io, slot: u8) -> Result<()> {
        each!(self, c => c.remove_trigger(io, slot))
    }
    pub fn restore_triggers(&self, io: &mut Io) -> Result<()> {
        each!(self, c => c.restore_triggers(io))
    }
    pub fn assert_reset(&mut self) {
        each!(self, c => c.assert_reset())
    }
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
    pub core: Core,
    /// Consecutive failed polls / examinations (0 = healthy).
    pub poll_failures: u32,
    pub breakpoints: Vec<Breakpoint>,
    pub watchpoints: Vec<Watchpoint>,
    /// Set by `arp_reset assert` for the following deassert.
    pub reset_halt: bool,
}

impl TargetEntry {
    /// A target of a type from [`Core::TYPES`] (TriCore for unknown types).
    pub fn new(name: &str, type_name: &str) -> Self {
        TargetEntry {
            name: name.to_string(),
            type_name: type_name.to_string(),
            events: Vec::new(),
            options: BTreeMap::new(),
            chain_position: None,
            defer_examine: false,
            core: Core::for_type(type_name).unwrap_or(Core::Tricore(Tricore::new())),
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
