//! Peripheral suspend: the modules whose OCS is set so that they stop with the cores (OCDS
//! suspend), as winIDEA's SuspendPeripheralsWhileStopped.
//!
//! One rule instead of per-module knowledge: a module's OCS is written as soon as the write can
//! land, and never while it cannot. It lands when the module is enabled (CLC.DISS = 0) and its
//! ACCEN0 lets the debugger write (TAG bit 28); a write while it cannot is not harmless (on one
//! board a refused OCS write reset the chip, and its SBC followed with a dozen more resets).
//!
//! A power-on class reset clears every OCS, so after each reset all modules are pending again.
//! They are tried:
//! - right after the reset, in the same step that switches OCDS on again ([`crate::restore`]): the
//!   modules that are on after a reset (STM, SMU) land there, before the application can lock
//!   them through ACCEN0;
//! - while the cores run, by the background poll ([`poll_tick`]): a module the application
//!   enables later (CAN, EVADC) gets its suspend within a poll step, without a halt;
//! - before the debugger's pause, and once more when a halt is seen (late, logged);
//! - at `init` (hot attach) and by `suspend now`.
//!
//! A module that the application locks (ACCEN0) before its suspend was written stays `locked`
//! until the next reset; one whose write does not take is left alone after [`MAX_TRIES`]
//! (`refused`); one not enabled [`IDLE_AFTER`] after a reset is checked only every [`IDLE_RETRY`].
//!
//! Addresses come from the chip's bundled SVD description (`<MODULE>.OCS`, `.CLC`, `.ACCEN0`), or
//! are given (`suspend add NAME -ocs A -clc A -accen A`).

use std::cell::RefCell;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tas::Batch;

use crate::adapter::Io;
use crate::ctx::{Ctx, Cx};
use crate::target::{Core, TargetState};
use crate::{log_info, log_warn};

/// A module not enabled this long after a reset is checked only every [`IDLE_RETRY`].
pub const IDLE_AFTER: Duration = Duration::from_secs(5);
pub const IDLE_RETRY: Duration = Duration::from_secs(2);
/// How often the background poll checks the pending modules while the cores run.
pub const POLL_EVERY: Duration = Duration::from_millis(20);
/// Writes whose read-back does not show the suspend before a module is left alone (`refused`).
pub const MAX_TRIES: u8 = 2;

const OCS_SUS_P: u32 = 1 << 28;
const CLC_DISS: u32 = 1 << 1;
const ACCEN_DEBUGGER: u32 = 1 << 28;

fn sus_of(ocs: u32) -> u32 {
    (ocs >> 24) & 0xF
}

/// OCS.SUS: how the module reacts to the OCDS suspend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// 0: not suspended (also clears a suspend an earlier session left).
    Off,
    /// 1: hard suspend, the clock stops at once (the only one GPT12 has).
    Hard,
    /// 2: soft suspend, the module finishes what it is doing.
    Soft,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "off" | "0" => Some(Mode::Off),
            "hard" | "1" => Some(Mode::Hard),
            "soft" | "2" => Some(Mode::Soft),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::Hard => "hard",
            Mode::Soft => "soft",
        }
    }
    fn sus(self) -> u32 {
        match self {
            Mode::Off => 0,
            Mode::Hard => 1,
            Mode::Soft => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Still to write: disabled so far, or not tried since the reset.
    Pending,
    Done,
    /// Enabled, but ACCEN0 shuts the debugger out: until the next reset.
    Locked,
    /// Written, but the read-back did not show it ([`MAX_TRIES`] times): until the next reset.
    Refused,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Pending => "pending",
            State::Done => "done",
            State::Locked => "locked",
            State::Refused => "refused",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Module {
    pub name: String,
    pub ocs: u32,
    pub clc: u32,
    pub accen: u32,
    pub mode: Mode,
    pub state: State,
    tries: u8,
    /// When it got its suspend, counted from the reset (or the attach).
    pub done_after: Option<Duration>,
    /// Last check of an idle module.
    idle_checked: Option<Instant>,
}

/// What occasion tries the modules (for the log).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    Reset,
    Attach,
    Poll,
    Pause,
    Halted,
    Command,
}

/// Which pending modules an attempt takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Select {
    /// All of them.
    All,
    /// Not the idle ones.
    Active,
    /// The background poll's: the active ones, and the idle ones whose turn has come.
    Due,
}

/// The modules and their state, shared by the commands, the poll and the device watch.
#[derive(Debug)]
pub struct SuspendSet {
    pub modules: Vec<Module>,
    /// Whether this server writes OCS (set at `init`: not where the probe owns OCDS).
    pub active: bool,
    reset_at: Instant,
    /// Counts the resets: an attempt that spans one does not apply its stale results.
    epoch: u64,
    last_poll: Option<Instant>,
}

impl Default for SuspendSet {
    fn default() -> Self {
        SuspendSet { modules: Vec::new(), active: false, reset_at: Instant::now(), epoch: 0, last_poll: None }
    }
}

impl SuspendSet {
    /// After a reset (or at the attach): every module pending again.
    pub fn mark_reset(&mut self) {
        self.reset_at = Instant::now();
        self.epoch += 1;
        for m in &mut self.modules {
            m.state = State::Pending;
            m.tries = 0;
            m.done_after = None;
            m.idle_checked = None;
        }
    }

    fn idle(&self, now: Instant) -> bool {
        now.duration_since(self.reset_at) >= IDLE_AFTER
    }

    fn wanted(&self, m: &Module, now: Instant, sel: Select) -> bool {
        if m.state != State::Pending {
            return false;
        }
        match sel {
            Select::All => true,
            Select::Active => !self.idle(now),
            Select::Due => !self.idle(now) || m.idle_checked.is_none_or(|t| now.duration_since(t) >= IDLE_RETRY),
        }
    }

    /// Whether an attempt with `sel` would try anything.
    pub fn has_wanted(&self, sel: Select) -> bool {
        let now = Instant::now();
        self.active && self.modules.iter().any(|m| self.wanted(m, now, sel))
    }

    pub fn names_in(&self, state: State) -> Vec<String> {
        self.modules.iter().filter(|m| m.state == state).map(|m| m.name.clone()).collect()
    }

    pub fn add(&mut self, name: &str, ocs: u32, clc: u32, accen: u32, mode: Mode) {
        let m = Module { name: name.to_string(), ocs, clc, accen, mode, state: State::Pending, tries: 0, done_after: None, idle_checked: None };
        match self.modules.iter_mut().find(|x| x.name.eq_ignore_ascii_case(name)) {
            Some(x) => *x = m,
            None => self.modules.push(m),
        }
    }
}

/// Reads `addrs` in as few transactions as possible: a failing access also fails the rest of its
/// transaction, so the reads after a failure go again in a new one. `None` for a failed read.
fn read_tolerant(io: &mut Io, addrs: &[u32]) -> Vec<Option<u32>> {
    let mut out = Vec::with_capacity(addrs.len());
    while out.len() < addrs.len() {
        let mut b = Batch::new();
        for &a in &addrs[out.len()..] {
            b.read32(a as u64);
        }
        let Ok(r) = io.batch(&b) else {
            out.resize(addrs.len(), None);
            break;
        };
        let before = out.len();
        for res in r {
            if res.ok() {
                out.push(Some(res.u32()));
            } else {
                out.push(None);
                break;
            }
        }
        if out.len() == before {
            out.push(None);
        }
    }
    out
}

enum Outcome {
    Done,
    /// Not enabled yet (or CLC unreadable): stays pending.
    Disabled,
    Locked,
    WriteFailed,
}

/// Tries the pending modules `sel` takes: reads CLC and OCS, then ACCEN0 of the enabled ones,
/// writes the OCS of those it lets through (one packed transaction) and reads them back. Returns
/// the names still pending afterwards.
pub fn attempt(io: &mut Io, set: &RefCell<SuspendSet>, why: Why, sel: Select) -> Vec<String> {
    let now = Instant::now();
    let (epoch, plan) = {
        let s = set.borrow();
        if !s.active {
            return Vec::new();
        }
        let plan: Vec<(usize, u32, u32, u32, u32)> = s
            .modules
            .iter()
            .enumerate()
            .filter(|(_, m)| s.wanted(m, now, sel))
            .map(|(i, m)| (i, m.ocs, m.clc, m.accen, m.mode.sus()))
            .collect();
        (s.epoch, plan)
    };
    if !plan.is_empty() {
        let outcomes = try_modules(io, &plan);
        apply(set, epoch, why, now, &plan, outcomes);
    }
    set.borrow().names_in(State::Pending)
}

fn try_modules(io: &mut Io, plan: &[(usize, u32, u32, u32, u32)]) -> Vec<Outcome> {
    // CLC first: always readable. OCS reads are harmless too; ACCEN0 is read only for modules
    // that are enabled.
    let addrs: Vec<u32> = plan.iter().flat_map(|&(_, ocs, clc, _, _)| [clc, ocs]).collect();
    let first = read_tolerant(io, &addrs);
    let mut out: Vec<Outcome> = Vec::with_capacity(plan.len());
    let mut enabled = Vec::new();
    for (k, &(_, _, _, _, sus)) in plan.iter().enumerate() {
        let (clc, ocs) = (first[2 * k], first[2 * k + 1]);
        if ocs.is_some_and(|v| sus_of(v) == sus) {
            out.push(Outcome::Done);
        } else if clc.is_some_and(|c| c & CLC_DISS == 0) {
            out.push(Outcome::Disabled);
            enabled.push(k);
        } else {
            out.push(Outcome::Disabled);
        }
    }
    if enabled.is_empty() {
        return out;
    }
    let accens = read_tolerant(io, &enabled.iter().map(|&k| plan[k].3).collect::<Vec<_>>());
    let mut writable = Vec::new();
    for (&k, a) in enabled.iter().zip(accens) {
        match a {
            Some(a) if a & ACCEN_DEBUGGER != 0 => writable.push(k),
            Some(_) => out[k] = Outcome::Locked,
            None => {}
        }
    }
    if writable.is_empty() {
        return out;
    }
    let writes: Vec<(u32, u32)> = writable.iter().map(|&k| (plan[k].1, OCS_SUS_P | (plan[k].4 << 24))).collect();
    // A failed write is judged by the read-back like one that did not take.
    let _ = io.write32_seq_packed(&writes);
    let back = read_tolerant(io, &writable.iter().map(|&k| plan[k].1).collect::<Vec<_>>());
    for (&k, v) in writable.iter().zip(back) {
        out[k] = if v.is_some_and(|v| sus_of(v) == plan[k].4) { Outcome::Done } else { Outcome::WriteFailed };
    }
    out
}

fn apply(set: &RefCell<SuspendSet>, epoch: u64, why: Why, now: Instant, plan: &[(usize, u32, u32, u32, u32)], outcomes: Vec<Outcome>) {
    let mut s = set.borrow_mut();
    // A reset in between (answered by the device watch during this attempt): its own attempt
    // decided, and these results are from before it.
    if s.epoch != epoch {
        return;
    }
    let since = now.duration_since(s.reset_at);
    let idle = s.idle(now);
    for (&(i, ..), o) in plan.iter().zip(outcomes) {
        let Some(m) = s.modules.get_mut(i) else { continue };
        match o {
            Outcome::Done => {
                m.state = State::Done;
                m.done_after = Some(since);
                match why {
                    Why::Halted => log_warn!("Suspend: {} ({}) set only after the halt, {} ms after the reset", m.name, m.mode.name(), since.as_millis()),
                    Why::Poll => log_info!("Suspend: {} ({}) set, {} ms after the reset", m.name, m.mode.name(), since.as_millis()),
                    _ => {}
                }
            }
            Outcome::Disabled => {
                if idle {
                    m.idle_checked = Some(now);
                }
            }
            Outcome::Locked => {
                m.state = State::Locked;
                log_warn!("Suspend: {} is locked against the debugger (ACCEN0) before its suspend was written; the next reset gives it another chance", m.name);
            }
            Outcome::WriteFailed => {
                m.tries += 1;
                if m.tries >= MAX_TRIES {
                    m.state = State::Refused;
                    log_warn!("Suspend: {} does not take its OCS write; left alone until the next reset", m.name);
                }
            }
        }
    }
}

/// The background poll's part (see the module documentation): while the cores run, every
/// [`POLL_EVERY`], the pending modules that are due.
pub fn poll_tick(cx: &Cx) {
    let rs = cx.with(|c| c.restore.clone());
    {
        let mut s = rs.suspend.borrow_mut();
        if !s.has_wanted(Select::Due) {
            return;
        }
        let now = Instant::now();
        if s.last_poll.is_some_and(|t| now.duration_since(t) < POLL_EVERY) {
            return;
        }
        s.last_poll = Some(now);
    }
    cx.with(|c| {
        let running = c.targets.iter().any(|e| matches!(e.core, Core::Tricore(_)) && e.core.examined() && e.core.state() == TargetState::Running);
        if !running {
            return;
        }
        if let Ok(mut io) = c.io() {
            attempt(&mut io, &rs.suspend, Why::Poll, Select::Due);
        }
    });
}

/// An attempt from a command or an operation (`suspend now`, the pause, a halt seen by the poll),
/// if anything is pending for `sel`. Returns the names still pending.
pub fn attempt_now(cx: &Cx, why: Why, sel: Select) -> Vec<String> {
    let rs = cx.with(|c| c.restore.clone());
    if !rs.suspend.borrow().has_wanted(sel) {
        return rs.suspend.borrow().names_in(State::Pending);
    }
    cx.with(|c| match c.io() {
        Ok(mut io) => attempt(&mut io, &rs.suspend, why, sel),
        Err(_) => rs.suspend.borrow().names_in(State::Pending),
    })
}

// --- Names -------------------------------------------------------------------------------------

/// The chip of the target configuration: the name prefix of its first TriCore core (`tc37x`).
fn chip(c: &Ctx) -> Option<String> {
    let t = c.targets.iter().find(|e| matches!(e.core, Core::Tricore(_)))?;
    Some(t.name.split_once('.').map_or(t.name.as_str(), |(chip, _)| chip).to_string())
}

/// The chip's bundled SVD description, loaded once (only the device file: the extra files hold
/// the HSM and SCR, which have no OCS).
fn device(c: &mut Ctx) -> Result<Arc<svd::Device>, String> {
    if let Some(d) = &c.svd_device {
        return Ok(d.clone());
    }
    let chip = chip(c).ok_or("no TriCore target yet to tell the chip by: load the target cfg first")?;
    let cat = svd::Catalog::bundled();
    let entry = cat.find(&chip).ok_or_else(|| format!("no bundled SVD description for `{chip}`: give the addresses (suspend add NAME -ocs A -clc A -accen A)"))?;
    let t0 = Instant::now();
    let d = Arc::new(svd::load(&entry.path, &[]).map_err(|e| format!("{}: {e}", entry.path.display()))?);
    crate::log_debug!("Suspend: {} loaded in {} ms", entry.path.display(), t0.elapsed().as_millis());
    c.svd_device = Some(d.clone());
    Ok(d)
}

/// OCS, CLC and ACCEN0 of a module, from the SVD description.
fn resolve(c: &mut Ctx, name: &str) -> Result<(u32, u32, u32), String> {
    let d = device(c)?;
    let reg = |r: &str| -> Result<u32, String> {
        match d.find(&format!("{name}.{r}")) {
            Some(svd::Node::Register(info)) => Ok(info.address as u32),
            _ => Err(format!("{name}: no {name}.{r} register in the SVD description (not a module with OCDS suspend?)")),
        }
    };
    if d.peripheral(name).is_none() {
        return Err(format!("{name}: no such module in the SVD description"));
    }
    Ok((reg("OCS")?, reg("CLC")?, reg("ACCEN0")?))
}

// --- Command -----------------------------------------------------------------------------------

const USAGE: &str = "usage: suspend add <module|cores> ?soft|hard|off? | suspend add <name> -ocs <addr> -clc <addr> -accen <addr> ?soft|hard|off? \
                     | suspend remove <name> | suspend clear | suspend list | suspend status | suspend now | suspend pending ?locked?";

fn tcl_list(items: &[String]) -> String {
    items.iter().map(|s| jim::quote(s)).collect::<Vec<_>>().join(" ")
}

/// `suspend ...`: see [`USAGE`] and the module documentation.
pub fn cmd_suspend(cx: &Cx, a: &[&str]) -> Result<String, String> {
    match a.get(1).copied() {
        Some("add") => cmd_add(cx, &a[2..]),
        Some("remove") => {
            let name = a.get(2).ok_or(USAGE)?;
            let rs = cx.with(|c| c.restore.clone());
            let mut s = rs.suspend.borrow_mut();
            let n = s.modules.len();
            s.modules.retain(|m| !m.name.eq_ignore_ascii_case(name));
            if s.modules.len() == n {
                return Err(format!("{name}: not in the suspend list"));
            }
            Ok(String::new())
        }
        Some("clear") => {
            let rs = cx.with(|c| c.restore.clone());
            rs.suspend.borrow_mut().modules.clear();
            Ok(String::new())
        }
        Some("list") => {
            let rs = cx.with(|c| c.restore.clone());
            let s = rs.suspend.borrow();
            let items: Vec<String> = s
                .modules
                .iter()
                .map(|m| format!("{} {} 0x{:08X} 0x{:08X} 0x{:08X}", m.name, m.mode.name(), m.ocs, m.clc, m.accen))
                .collect();
            Ok(tcl_list(&items))
        }
        Some("status") => {
            let rs = cx.with(|c| c.restore.clone());
            let s = rs.suspend.borrow();
            let items: Vec<String> = s
                .modules
                .iter()
                .map(|m| {
                    let after = m.done_after.map_or_else(|| "{}".to_string(), |d| d.as_millis().to_string());
                    let state = if m.state == State::Pending && s.idle(Instant::now()) { "idle" } else { m.state.name() };
                    format!("name {} mode {} state {} after_ms {} ocs 0x{:08X}", m.name, m.mode.name(), state, after, m.ocs)
                })
                .collect();
            Ok(tcl_list(&items))
        }
        Some("now") => {
            if !cx.with(|c| c.initialized) {
                return Err("suspend now: run 'init' first".into());
            }
            Ok(tcl_list(&attempt_now(cx, Why::Command, Select::All)))
        }
        Some("pending") => {
            let rs = cx.with(|c| c.restore.clone());
            let state = match a.get(2).copied() {
                None => State::Pending,
                Some("locked") => State::Locked,
                Some(_) => return Err(USAGE.into()),
            };
            let names = rs.suspend.borrow().names_in(state);
            Ok(tcl_list(&names))
        }
        _ => Err(USAGE.into()),
    }
}

fn cmd_add(cx: &Cx, a: &[&str]) -> Result<String, String> {
    let name = *a.first().ok_or(USAGE)?;
    let mut mode = Mode::Soft;
    let (mut ocs, mut clc, mut accen) = (None, None, None);
    let mut i = 1;
    while i < a.len() {
        let addr = |k: usize| -> Result<u32, String> { crate::util::parse_u32(a.get(k).ok_or(USAGE)?) };
        match a[i] {
            "-ocs" => ocs = Some(addr(i + 1)?),
            "-clc" => clc = Some(addr(i + 1)?),
            "-accen" => accen = Some(addr(i + 1)?),
            m => {
                mode = Mode::parse(m).ok_or_else(|| format!("{m}: not a suspend mode (soft, hard, off)"))?;
                i += 1;
                continue;
            }
        }
        i += 2;
    }
    let entries: Vec<(String, u32, u32, u32)> = match (ocs, clc, accen) {
        (Some(o), Some(c), Some(x)) => vec![(name.to_string(), o, c, x)],
        (None, None, None) => cx.with(|c| -> Result<Vec<_>, String> {
            let names: Vec<String> = if name.eq_ignore_ascii_case("cores") {
                let ids: Vec<u32> = c.targets.iter().filter_map(|e| if let Core::Tricore(t) = &e.core { Some(t.coreid) } else { None }).collect();
                if ids.is_empty() {
                    return Err("suspend add cores: no TriCore target yet (load the target cfg first)".into());
                }
                ids.iter().map(|i| format!("STM{i}")).collect()
            } else {
                vec![name.to_ascii_uppercase()]
            };
            names
                .into_iter()
                .map(|n| {
                    let (o, cl, x) = resolve(c, &n)?;
                    Ok((n, o, cl, x))
                })
                .collect()
        })?,
        _ => return Err("suspend add: give all of -ocs, -clc and -accen, or none".into()),
    };
    let rs = cx.with(|c| c.restore.clone());
    let mut s = rs.suspend.borrow_mut();
    for (n, o, c, x) in entries {
        s.add(&n, o, c, x, mode);
    }
    Ok(String::new())
}
