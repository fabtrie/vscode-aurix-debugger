//! Target bookkeeping shared by all target types (OpenOCD `struct target`).

use std::collections::BTreeMap;

use tas::Batch;

use crate::adapter::Io;
use crate::arch::{ArchInfo, CORTEX_M, GTM_MCS, TRICORE};
use crate::cortex_m::CortexM;
use crate::gtm_mcs::McsChannel;
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

/// A core of a target: TriCore (host cores), Cortex-M (HSM) or a GTM MCS channel.
pub enum Core {
    Tricore(Tricore),
    CortexM(CortexM),
    Mcs(McsChannel),
}

/// Calls the same method on either core type.
macro_rules! each {
    ($self:expr, $c:ident => $e:expr) => {
        match $self {
            Core::Tricore($c) => $e,
            Core::CortexM($c) => $e,
            Core::Mcs($c) => $e,
        }
    };
}

/// Run control performed by the probe itself, instead of by writing OCDS
/// registers over the bus.
///
/// The TAS path drives an AURIX by writing its debug registers as ordinary
/// memory ([`crate::tricore`]), so `Adapter` only has to move bytes. A probe
/// whose vendor software owns OCDS -- an iSYSTEM BlueBox through the winIDEA
/// SDK -- cannot expose that, and offers halt/resume/step directly instead.
/// Such an adapter returns `Some` from [`crate::adapter::Adapter::run_control`]
/// and the [`Core`] methods below route to it.
///
/// Only TriCore cores are routed. The HSM ([`crate::cortex_m`], reached through
/// its debug bridge) and the GTM MCS channels ([`crate::gtm_mcs`], plain module
/// RAM) use no OCDS registers, so they keep working over ordinary bus accesses
/// on any adapter.
///
/// Methods mirror the [`Core`] ones, minus the [`Io`]: the implementation talks
/// to its own probe and gets the core it is acting on, which carries the core
/// id and the state fields to update.
pub trait RunControl {
    fn examine(&mut self, c: &mut Tricore) -> Result<()>;
    fn poll(&mut self, c: &mut Tricore) -> Result<Transition>;
    fn halt(&mut self, c: &mut Tricore) -> Result<()>;
    fn resume(&mut self, c: &mut Tricore, addr: Option<u32>, mode: RunMode) -> Result<()>;
    fn step(&mut self, c: &mut Tricore, addr: Option<u32>, mode: RunMode) -> Result<bool>;
    fn unmask_irq(&mut self, c: &mut Tricore) -> Result<()>;
    fn read_regs(&mut self, c: &mut Tricore, idxs: &[usize]) -> Result<Vec<u32>>;
    fn write_reg(&mut self, c: &mut Tricore, idx: usize, value: u32) -> Result<()>;
    fn add_breakpoint(&mut self, c: &mut Tricore, addr: u32, len: u32) -> Result<u8>;
    fn add_watchpoint(&mut self, c: &mut Tricore, addr: u32, len: u32, access: u32) -> Result<u8>;
    fn remove_trigger(&mut self, c: &mut Tricore, slot: u8) -> Result<()>;
    fn restore_triggers(&mut self, c: &mut Tricore) -> Result<()>;
    /// Replaces the OCDS register work of `arp_reset deassert`; see
    /// [`Core::after_reset`].
    fn after_reset(&mut self, c: &mut Tricore, halt: bool) -> Result<Transition>;
    /// Whether this probe can drive the core at all. A core it cannot reach
    /// is left out of a halting reset instead of failing it; the default is
    /// "yes", which is every core on the OCDS path.
    fn drives(&self, _c: &Tricore) -> bool {
        true
    }
    /// A whole source-level step, done by the probe from its own line
    /// information, when it has the same ELF (`elf`) loaded.
    ///
    /// `None` means not available here, and the caller steps instruction by
    /// instruction. `Some(Ok(true))`: the core is halted at the new line;
    /// `Some(Ok(false))`: the step is still running, and the poll reports the
    /// halt.
    fn source_step(
        &mut self,
        _c: &mut Tricore,
        _kind: SourceStep,
        _elf: &std::path::Path,
    ) -> Option<Result<bool>> {
        None
    }
}

/// A source-level step, see [`RunControl::source_step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceStep {
    Over,
    Into,
    Out,
}

/// Defines a [`Core`] method that a [`RunControl`] adapter may take over.
///
/// Without one (the TAS and simulator adapters) this is exactly the `each!`
/// dispatch the method had before.
macro_rules! forward {
    ($(#[$m:meta])* $name:ident($($arg:ident: $ty:ty),*) -> $ret:ty) => {
        $(#[$m])*
        pub fn $name(&mut self, io: &mut Io $(, $arg: $ty)*) -> Result<$ret> {
            if let Core::Tricore(c) = self {
                if let Some(rc) = io.adapter.run_control() {
                    return rc.$name(c $(, $arg)*);
                }
            }
            each!(self, c => c.$name(io $(, $arg)*))
        }
    };
}

impl Core {
    /// Creates a core of a target type (`tricore`, `cortex_m`).
    pub fn for_type(type_name: &str) -> Option<Core> {
        match type_name {
            "tricore" => Some(Core::Tricore(Tricore::new())),
            "cortex_m" => Some(Core::CortexM(CortexM::new())),
            "gtm_mcs" => Some(Core::Mcs(McsChannel::new())),
            _ => None,
        }
    }

    pub const TYPES: &'static [&'static str] = &["tricore", "cortex_m", "gtm_mcs"];

    pub fn arch(&self) -> &'static ArchInfo {
        match self {
            Core::Tricore(_) => &TRICORE,
            Core::CortexM(_) => &CORTEX_M,
            Core::Mcs(_) => &GTM_MCS,
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

    pub fn as_mcs(&self) -> Option<&McsChannel> {
        match self {
            Core::Mcs(c) => Some(c),
            _ => None,
        }
    }

    pub fn as_mcs_mut(&mut self) -> Option<&mut McsChannel> {
        match self {
            Core::Mcs(c) => Some(c),
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

    /// Whether frontends list the core (an MCS channel only once it is used).
    pub fn visible(&self) -> bool {
        match self {
            Core::Mcs(c) => c.examined && c.visible(),
            _ => true,
        }
    }

    /// Minimum time between examination attempts of an unexamined core in the background poll.
    pub fn examine_interval(&self) -> std::time::Duration {
        match self {
            Core::Mcs(_) => std::time::Duration::from_secs(1),
            _ => std::time::Duration::ZERO,
        }
    }

    /// Addresses the core accesses through its own path instead of the host address space
    /// (HSM-private memory, MCS program RAM).
    pub fn owns_address(&self, addr: u32) -> bool {
        match self {
            Core::CortexM(_) => crate::cortex_m::hsm_private(addr),
            Core::Mcs(c) => c.owns_address(addr),
            Core::Tricore(_) => false,
        }
    }

    /// Reads memory as the core sees it (see [`Self::owns_address`]).
    pub fn read_memory(&mut self, io: &mut Io, addr: u32, len: usize) -> Result<Vec<u8>> {
        match self {
            Core::CortexM(c) if crate::cortex_m::hsm_private(addr) => c.read_memory(io, addr, len),
            Core::Mcs(c) if c.owns_address(addr) => c.read_memory(io, addr, len),
            _ => io.read(addr, len),
        }
    }

    /// Reads memory as the core sees it; unreadable words of HSM-private memory only fail their own
    /// bytes. Other memory fails as a whole, as in [`Self::read_memory`].
    pub fn read_memory_partial(&mut self, io: &mut Io, addr: u32, len: usize) -> Result<crate::adapter::PartialRead> {
        match self {
            Core::CortexM(c) if crate::cortex_m::hsm_private(addr) => c.read_memory_partial(io, addr, len),
            _ => Ok(crate::adapter::PartialRead { data: self.read_memory(io, addr, len)?, unreadable: Vec::new() }),
        }
    }

    pub fn write_memory(&mut self, io: &mut Io, addr: u32, data: &[u8]) -> Result<()> {
        match self {
            Core::CortexM(c) if crate::cortex_m::hsm_private(addr) => c.write_memory(io, addr, data),
            Core::Mcs(c) if c.owns_address(addr) => c.write_memory(io, addr, data),
            _ => io.write(addr, data),
        }
    }

    /// A poll found something that needs target access (see [`Self::fixup`]).
    pub fn wants_fixup(&self) -> bool {
        match self {
            Core::Mcs(c) => c.wants_fixup(),
            _ => false,
        }
    }

    pub fn fixup(&mut self, io: &mut Io) -> Result<()> {
        match self {
            Core::Mcs(c) => c.fixup(io),
            _ => Ok(()),
        }
    }

    pub fn invalidate(&mut self) {
        each!(self, c => c.invalidate())
    }
    forward!(examine() -> ());
    pub fn poll_queue(&self, b: &mut Batch) -> PollOps {
        each!(self, c => c.poll_queue(b))
    }
    forward!(poll() -> Transition);
    pub fn poll_apply(&mut self, vals: &[u32]) -> Transition {
        each!(self, c => c.poll_apply(vals))
    }
    pub fn hit_trigger(&self) -> Option<usize> {
        each!(self, c => c.hit_trigger())
    }
    forward!(halt() -> ());
    forward!(resume(addr: Option<u32>, mode: RunMode) -> ());
    forward!(step(addr: Option<u32>, mode: RunMode) -> bool);
    pub fn irq_masked(&self) -> bool {
        each!(self, c => c.irq_masked())
    }
    forward!(unmask_irq() -> ());
    /// One register, as the single-register case of [`Core::read_regs`] -- a
    /// probe that does its own run control answers both the same way.
    pub fn read_reg(&mut self, io: &mut Io, idx: usize) -> Result<u32> {
        if let Core::Tricore(c) = self {
            if let Some(rc) = io.adapter.run_control() {
                return Ok(rc.read_regs(c, &[idx])?[0]);
            }
        }
        each!(self, c => c.read_reg(io, idx))
    }
    forward!(read_regs(idxs: &[usize]) -> Vec<u32>);
    forward!(write_reg(idx: usize, value: u32) -> ());
    forward!(add_breakpoint(addr: u32, len: u32) -> u8);
    forward!(add_watchpoint(addr: u32, len: u32, access: u32) -> u8);
    forward!(remove_trigger(slot: u8) -> ());
    forward!(restore_triggers() -> ());
    /// A source-level step done by the probe; see [`RunControl::source_step`].
    /// `None` for cores and adapters that do not offer it.
    pub fn source_step(
        &mut self,
        io: &mut Io,
        kind: SourceStep,
        elf: &std::path::Path,
    ) -> Option<Result<bool>> {
        match self {
            Core::Tricore(c) => io.adapter.run_control()?.source_step(c, kind, elf),
            _ => None,
        }
    }

    /// Whether the adapter can drive this core. Always true without a
    /// [`RunControl`] adapter.
    pub fn drivable(&mut self, io: &mut Io) -> bool {
        match self {
            Core::Tricore(c) => io.adapter.run_control().is_none_or(|rc| rc.drives(c)),
            _ => true,
        }
    }

    /// Brings this core back up after `arp_reset deassert` released reset.
    ///
    /// TriCore cores need their OCDS re-enabled and the halt-after-reset
    /// trigger cleared; the HSM and the MCS channels are simply re-examined,
    /// since they only become accessible once the startup software has run.
    pub fn after_reset(&mut self, io: &mut Io, halt: bool, name: &str) -> Result<Transition> {
        if let Core::Tricore(c) = self {
            if let Some(rc) = io.adapter.run_control() {
                return rc.after_reset(c, halt);
            }
        }
        match self {
            Core::Tricore(core) => core.after_reset(io, halt, name),
            core => {
                // The HSM starts when the host startup software releases it and is only
                // accessible when its firmware enables HSM debugging; MCS channels once the
                // startup software enables the GTM. They are examined again (e.g. by the
                // reset-end handler after the HSM release, or the background poll) instead of
                // failing the reset.
                if !core.examined() {
                    return Ok(Transition::None);
                }
                match core.examine(io).and_then(|_| core.poll(io)) {
                    Ok(tr) => {
                        core.restore_triggers(io)?;
                        Ok(tr)
                    }
                    Err(err) => {
                        crate::log_debug!("[{name}] not accessible after reset: {err}");
                        core.set_examined(false);
                        Ok(Transition::None)
                    }
                }
            }
        }
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

/// Re-examination interval of a target whose poll fails right after each re-examination (e.g. the
/// HSM while the device is in reset).
pub const FLAP_RETRY: std::time::Duration = std::time::Duration::from_secs(1);

/// How long the polls of a target must fail without interruption before it counts as failed. The
/// HSM debug window rejects single accesses now and then while the host software has not started
/// yet (e.g. after a halting reset).
pub const POLL_FAIL_TIME: std::time::Duration = std::time::Duration::from_millis(100);

/// How long a target must be inaccessible before the frontends show it as such. Shorter outages
/// (a reset) are not shown.
pub const UNREACHABLE_TIME: std::time::Duration = std::time::Duration::from_secs(1);

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
    /// Last examination attempt of the background poll.
    pub last_examine: Option<std::time::Instant>,
    /// First poll of the current run of failed polls (see [`POLL_FAIL_TIME`]).
    pub failing_since: Option<std::time::Instant>,
    /// The current failure was logged as an error, so its recovery is logged too.
    pub failure_logged: bool,
    /// Re-examined after this many failed attempts; reported once a poll succeeds.
    pub recovering: Option<u32>,
    /// Failed again right after a re-examination: re-examined at most every [`FLAP_RETRY`].
    pub flapping: bool,
    /// Not accessible since (unexamined or polls failing).
    pub down_since: Option<std::time::Instant>,
    /// Inaccessible for [`UNREACHABLE_TIME`]: shown as such by the frontends.
    pub unreachable: bool,
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
            last_examine: None,
            failing_since: None,
            failure_logged: false,
            recovering: None,
            flapping: false,
            down_since: None,
            unreachable: false,
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
