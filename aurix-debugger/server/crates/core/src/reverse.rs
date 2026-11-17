//! Reverse debugging on the hardware: a checkpoint before every action, Step Back writes it back.
//!
//! An **action** is one thing the user does to the reversed core: a run, an instruction step, a
//! source step (many steps and runs, one action), a register or memory write. Before it starts the
//! core is halted and its state is saved: all registers and a snapshot of the RAM regions
//! ([`Region`]). Going back to a checkpoint writes the registers and the RAM back, so the core
//! really stands there again and runs from there.
//!
//! While the mode is on it owns the miniMCDS ([`crate::mcds_trace::Owner::Reverse`]): each action
//! except instruction steps and writes is recorded from its checkpoint to the halt that ends it.
//! The frontend that has the code and the symbols (TCF) decodes the recordings: which bytes an
//! action wrote (so shared memory is restored selectively) and points inside an action (reached by
//! restoring its checkpoint and running again). Here, without them, a restore compares the
//! snapshot with the current memory.
//!
//! The snapshots are read in full at every checkpoint but kept as differences: the newest one
//! whole, each older one as the bytes that differ from the next newer one.

use std::time::{Instant, SystemTime};

use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::mcds_trace::{self as mt, Owner, Recording};
use crate::target::{Core, DebugReason, RunMode, TargetState};

/// Registers saved per checkpoint: the debugger's numbering 0..=41 (d0-d15, a0-a15, lcx, fcx,
/// pcxi, psw, pc, icr, isp, btv, biv, syscon).
pub const NREGS: usize = 42;
pub const REG_PCXI: usize = 34;
pub const REG_PC: usize = 36;

pub const REG_NAMES: [&str; NREGS] = [
    "d0", "d1", "d2", "d3", "d4", "d5", "d6", "d7", "d8", "d9", "d10", "d11", "d12", "d13", "d14", "d15", "a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7",
    "a8", "a9", "a10", "a11", "a12", "a13", "a14", "a15", "lcx", "fcx", "pcxi", "psw", "pc", "icr", "isp", "btv", "biv", "syscon",
];

/// A RAM range saved at every checkpoint (global addresses).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    pub start: u32,
    pub len: u32,
    /// Written only by the reversed core (its stacks, its context save areas): always restored by
    /// difference. Other regions are restored only where the core wrote, when that is known.
    pub private: bool,
    /// Another core's own area (its stacks, its context save area): never restored by
    /// difference (that core's registers stay as they are), only where the reversed core wrote.
    pub foreign: bool,
    pub name: String,
}

impl Region {
    pub fn end(&self) -> u32 {
        self.start.wrapping_add(self.len)
    }
    pub fn contains(&self, a: u32) -> bool {
        a >= self.start && a < self.end()
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// The reversed core (target index).
    pub target: usize,
    /// Checkpoints kept (0: no limit).
    pub history: usize,
    /// Record each action (miniMCDS program trace, CPU0 only).
    pub trace: bool,
    pub regions: Vec<Region>,
    /// Refuse a Step Back that cannot restore everything.
    pub strict: bool,
    /// Re-execution: most breakpoint hits counted.
    pub max_hits: u32,
}

#[derive(Debug, Clone)]
enum Snap {
    /// The newest checkpoint: every region in full.
    Full(Vec<Vec<u8>>),
    /// An older one: `(address, bytes)` where it differs from the next newer checkpoint.
    Delta(Vec<(u32, Vec<u8>)>),
}

/// How an action ended.
#[derive(Debug, Clone)]
pub struct End {
    pub pc: u32,
    pub pcxi: u32,
    pub reason: DebugReason,
    /// Registers at the end (the next checkpoint's, unless something changed in between).
    pub regs: Vec<u32>,
}

/// A checkpoint and the action that followed it.
#[derive(Debug, Clone)]
pub struct Checkpoint {
    pub id: u64,
    /// The action: "step", "step over", "step into", "step out", "continue", "write", ...
    pub kind: String,
    pub mode: RunMode,
    pub at: SystemTime,
    pub regs: Vec<u32>,
    snap: Snap,
    /// Old bytes of writes outside the regions (RAM only), in the order they were made.
    pub outside: Vec<(u32, Vec<u8>)>,
    /// A write action: the bytes it wrote (global addresses).
    pub wrote: Written,
    /// The action's program trace, when recorded.
    pub recording: Option<Recording>,
    /// The recording was armed at this checkpoint and runs to the action's end (not re-armed
    /// when a breakpoint condition let the run go on).
    pub from_start: bool,
    pub end: Option<End>,
    /// Time the checkpoint took (registers, snapshot, arming).
    pub cost_ms: u64,
}

impl Checkpoint {
    pub fn pc(&self) -> u32 {
        self.regs.get(REG_PC).copied().unwrap_or(0)
    }
    pub fn pcxi(&self) -> u32 {
        self.regs.get(REG_PCXI).copied().unwrap_or(0)
    }
}

#[derive(Debug, Default)]
pub struct State {
    pub cfg: Option<Config>,
    pub history: Vec<Checkpoint>,
    /// The last checkpoint's action runs (or was not closed yet).
    pub open: bool,
    /// The core stands exactly at the last checkpoint (after a restore): the next action uses it
    /// instead of saving the same state again.
    pub at_checkpoint: bool,
    /// Open action scopes (a source step, a re-execution): the steps and runs inside make no
    /// checkpoints of their own.
    pub scope: u32,
    /// While > 0 nothing is checkpointed (restores, counting runs).
    pub suspended: u32,
    /// The next run continues the last action (a breakpoint whose condition was false).
    pub continuing: bool,
    /// The core runs without an action (it ran when the mode was switched on, or something else
    /// started it): recorded anyway, so its halt comes with the trace up to it (no checkpoint).
    pub watching: bool,
    /// Why the history was last cleared.
    pub cleared: Option<String>,
    next_id: u64,
}

impl State {
    pub fn enabled(&self) -> bool {
        self.cfg.is_some()
    }
    pub fn target(&self) -> Option<usize> {
        self.cfg.as_ref().map(|c| c.target)
    }
    pub fn snapshot_bytes(&self) -> u64 {
        self.cfg.as_ref().map_or(0, |c| c.regions.iter().map(|r| r.len as u64).sum())
    }
}

/// What a restore did.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub checkpoint: u64,
    pub kind: String,
    pub registers: Vec<&'static str>,
    pub bytes: u64,
    /// Shared regions compared with the snapshot (takes back writes of other masters too).
    pub by_difference: bool,
    pub outside_bytes: u64,
    /// Ranges whose restore the target refused (read back differently), outside the private
    /// regions: not undone.
    pub refused: Vec<(u32, u32)>,
    pub ms: u64,
}

/// Bytes an action (or several) wrote: sorted `(start, len)` ranges in global addresses.
pub type Written = Vec<(u32, u32)>;

fn is_tricore(c: &Ctx, t: usize) -> bool {
    matches!(c.targets.get(t).map(|e| &e.core), Some(Core::Tricore(_)))
}

fn halted(c: &Ctx, t: usize) -> bool {
    c.targets.get(t).is_some_and(|e| e.core.state() == TargetState::Halted)
}

/// The MCDS core number of target `t` if it can be traced.
fn traced_core(c: &Ctx, t: usize) -> Option<u8> {
    match c.targets.get(t).map(|e| &e.core) {
        Some(Core::Tricore(tc)) if tc.coreid == 0 => Some(0),
        _ => None,
    }
}

pub fn trace_config(core: u8) -> mcds::TraceConfig {
    mcds::TraceConfig { core, kind: mcds::Kind::Program { filter: None }, stop: mcds::Stop::Manual, timestamps: mcds::Timestamps::Messages }
}

/// Switches reverse debugging on for `cfg.target`. Refused while a manual trace is set up.
pub fn enable(c: &mut Ctx, cfg: Config) -> Result<()> {
    if !is_tricore(c, cfg.target) {
        return Err(Error::Usage("reverse debugging needs a TriCore core".into()));
    }
    if cfg.regions.is_empty() {
        return Err(Error::Usage("no RAM regions to save (load the ELF or give the regions)".into()));
    }
    if let Some(a) = &c.mcds.armed {
        if a.owner != Owner::Reverse {
            return Err(Error::Resource(format!("a manual trace is active (owner {}): stop it first", a.owner.as_str())));
        }
    }
    if c.mcds.auto.is_some() {
        return Err(Error::Resource("the trace is armed on every run (mcds auto): switch that off first".into()));
    }
    c.reverse.history.clear();
    c.reverse.open = false;
    c.reverse.at_checkpoint = false;
    c.reverse.scope = 0;
    c.reverse.cleared = None;
    c.reverse.watching = false;
    let t = cfg.target;
    c.reverse.cfg = Some(cfg);
    // Running already (attached to a running application): record up to its halt.
    if !halted(c, t) {
        watch(c, t);
    }
    Ok(())
}

/// Records a run of `t` that is no action (no checkpoint): the trace up to the halt, for the
/// trace panel and the instruction trace.
fn watch(c: &mut Ctx, t: usize) {
    let trace = c.reverse.cfg.as_ref().is_some_and(|cfg| cfg.trace);
    let Some(core) = traced_core(c, t).filter(|_| trace) else { return };
    if c.mcds.armed.is_some() {
        return;
    }
    match mt::arm(c, trace_config(core), Owner::Reverse) {
        Ok(_) => c.reverse.watching = true,
        Err(e) => crate::log_warn!("reverse: cannot record the run: {e}"),
    }
}

/// The poll found the reversed core running without a run of the debugger's (another client, a
/// script): the history no longer leads to the core's state, so it is dropped; the run is
/// recorded up to its halt.
pub fn on_untracked_run(c: &mut Ctx, t: usize) {
    if c.reverse.target() != Some(t) || c.reverse.open || c.reverse.scope > 0 || c.reverse.suspended > 0 || c.reverse.watching {
        return;
    }
    if !c.reverse.history.is_empty() {
        clear(c, "the core ran outside the debugger's run control");
    }
    watch(c, t);
}

/// Switches reverse debugging off: the history is dropped, the trace released.
pub fn disable(c: &mut Ctx) {
    let _ = mt::release(c, Owner::Reverse);
    c.reverse = State { next_id: c.reverse.next_id, ..Default::default() };
}

/// Drops the history (reset, flash programming, lost target).
pub fn clear(c: &mut Ctx, why: &str) {
    if !c.reverse.enabled() {
        return;
    }
    if !c.reverse.history.is_empty() {
        crate::log_info!("reverse: history cleared ({why})");
    }
    if c.mcds.armed.as_ref().is_some_and(|a| a.owner == Owner::Reverse) {
        let _ = mt::release(c, Owner::Reverse);
    }
    c.reverse.history.clear();
    c.reverse.open = false;
    c.reverse.at_checkpoint = false;
    c.reverse.watching = false;
    c.reverse.cleared = Some(why.to_string());
}

/// Whether an action on `t` gets a checkpoint now: the mode is on for `t`, no restore or counting
/// run is going on, and no reset or cfg event handler runs (their resumes and writes are not the
/// user's).
fn wants(c: &Ctx, t: usize) -> bool {
    c.reverse.target() == Some(t) && c.reverse.suspended == 0 && !c.in_reset && !c.in_reset_hook && c.active_events.is_empty()
}

fn read_regs(c: &mut Ctx, t: usize) -> Result<Vec<u32>> {
    let idxs: Vec<usize> = (0..NREGS).collect();
    let (mut io, e) = c.target_io(t)?;
    e.core.read_regs(&mut io, &idxs)
}

/// The address the debugger accesses RAM at ([`crate::tricore::bus_address`]: LMU/DLMU through
/// the non-cached segment `0xB`). `Io` maps it too; regions keep their linked addresses.
pub fn bus(a: u32) -> u32 {
    crate::tricore::bus_address(a)
}

fn read_regions(c: &mut Ctx, regions: &[Region]) -> Result<Vec<Vec<u8>>> {
    let mut io = c.io()?;
    regions
        .iter()
        .map(|r| io.read(bus(r.start), r.len as usize).map_err(|e| Error::Other(format!("reverse: cannot read {} ({:#010x}, {} bytes): {e}", r.name, r.start, r.len))))
        .collect()
}

/// Master tags of the debugger's accesses (TC3xx): Cerberus / IOC32E JTAG = 28, IOC32E DAP = 30.
/// A window counts as open only if both are enabled (which one a probe uses is not checked).
pub const DEBUGGER_TAGS: u32 = 1 << 28 | 1 << 30;

/// Address windows other bus masters (the debugger among them) may write, from the TriCore
/// cores' safety protection: per core 8 DSPR regions (`CPUx_SPR_SPROT_RGNLA/UA/ACCENA_W/B_W` at
/// `0xF880E000 + 0x20000 * x`) and 8 DLMU regions (+0x200, at the `0xB` alias). An address of a
/// DSPR or DLMU outside every region with a write enable is not written by anyone but its core.
/// Measured on a TC377: such a debugger write is dropped without an error (it may also raise a
/// safety alarm: never written). A window counts as open for the debugger only if it enables
/// the debugger's master tags ([`DEBUGGER_TAGS`]).
/// Returns `(memory start, memory end, open windows)` per protected memory.
#[allow(clippy::type_complexity)]
pub fn protection_windows(io: &mut crate::adapter::Io, coreids: &[u32]) -> Vec<(u32, u32, Vec<(u32, u32)>)> {
    let mut out = Vec::new();
    for &n in coreids {
        let base = 0xF880_E000 + 0x2_0000 * n;
        let dspr = 0x7000_0000u32.wrapping_sub(0x1000_0000 * n);
        for (block, mem_start, mem_end, alias) in [(0x000u32, dspr, dspr + 0x10_0000, 0u32), (0x200, 0x9000_0000 + 0x1_0000 * n, 0x9001_0000 + 0x1_0000 * n, 0x2000_0000)] {
            let addrs: Vec<u32> = (0..8).flat_map(|i| (0..4).map(move |k| base + block + 0x10 * i + 4 * k)).collect();
            let Ok(v) = io.read32_many(&addrs) else { continue };
            if v.iter().all(|&x| x == 0) {
                // Not configured (a model without the registers): the reset state is open.
                continue;
            }
            let mut open = Vec::new();
            for i in 0..8 {
                let (la, ua, wa, wb) = (v[4 * i], v[4 * i + 1], v[4 * i + 2], v[4 * i + 3]);
                if (wa | wb) & DEBUGGER_TAGS == DEBUGGER_TAGS && ua > la {
                    // DLMU regions name the 0xB alias; regions are kept in global (0x9) addresses.
                    open.push((la & !alias, ua & !alias));
                }
            }
            out.push((mem_start, mem_end, open));
        }
    }
    out
}

/// Bytes of `start..start + len` inside a protected memory but outside its open windows.
pub fn closed_bytes(windows: &[(u32, u32, Vec<(u32, u32)>)], start: u32, len: u32) -> u32 {
    let end = start.wrapping_add(len);
    let mut closed = 0u32;
    for (ms, me, open) in windows {
        let (s, e) = (start.max(*ms), end.min(*me));
        if s >= e {
            continue;
        }
        let mut covered: Vec<(u32, u32)> = open.iter().map(|&(a, b)| (a.max(s), b.min(e))).filter(|(a, b)| a < b).collect();
        covered.sort();
        let mut n = 0u32;
        let mut at = s;
        for (a, b) in covered {
            if b <= at {
                continue;
            }
            n += a.max(at) - at;
            at = b.max(at);
        }
        n += e - at.min(e);
        closed += n;
    }
    closed
}

/// Byte runs where `a` differs from `b` (same length), as `(offset, len)`; runs closer than 8
/// bytes are joined.
fn diff_runs(a: &[u8], b: &[u8]) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    let n = a.len().min(b.len());
    while i < n {
        if a[i] == b[i] {
            i += 1;
            continue;
        }
        let s = i;
        while i < n && a[i] != b[i] {
            i += 1;
        }
        match out.last_mut() {
            Some((ls, ll)) if s - (*ls + *ll) < 8 => *ll = i - *ls,
            _ => out.push((s, i - s)),
        }
    }
    out
}

/// Turns the newest checkpoint's full snapshot into a difference against `newer`.
fn to_delta(regions: &[Region], old: &[Vec<u8>], newer: &[Vec<u8>]) -> Vec<(u32, Vec<u8>)> {
    let mut d = Vec::new();
    for ((r, o), n) in regions.iter().zip(old).zip(newer) {
        for (off, len) in diff_runs(o, n) {
            d.push((r.start + off as u32, o[off..off + len].to_vec()));
        }
    }
    d
}

fn apply(regions: &[Region], buf: &mut [Vec<u8>], delta: &[(u32, Vec<u8>)]) {
    for (a, bytes) in delta {
        if let Some((i, r)) = regions.iter().enumerate().find(|(_, r)| r.contains(*a)) {
            let off = (a - r.start) as usize;
            let n = bytes.len().min(buf[i].len().saturating_sub(off));
            buf[i][off..off + n].copy_from_slice(&bytes[..n]);
        }
    }
}

/// The snapshot of checkpoint `k`.
fn snapshot_of(st: &State, k: usize) -> Option<Vec<Vec<u8>>> {
    let regions = &st.cfg.as_ref()?.regions;
    let mut buf = match &st.history.last()?.snap {
        Snap::Full(v) => v.clone(),
        Snap::Delta(_) => return None,
    };
    for j in (k..st.history.len() - 1).rev() {
        if let Snap::Delta(d) = &st.history[j].snap {
            apply(regions, &mut buf, d);
        }
    }
    Some(buf)
}

/// The snapshot of history entry `k`: `(start, bytes)` per region.
pub fn snapshot(c: &Ctx, k: usize) -> Option<Vec<(u32, Vec<u8>)>> {
    let regions = &c.reverse.cfg.as_ref()?.regions;
    let snap = snapshot_of(&c.reverse, k)?;
    Some(regions.iter().map(|r| r.start).zip(snap).collect())
}

/// Saves a checkpoint before an action on `t` (the core is halted). `kind`: what the action is.
/// Does nothing when the mode is off for `t`, inside an action scope, or for writes and runs that
/// are not the user's ([`wants`]).
pub fn begin_action(c: &mut Ctx, t: usize, kind: &str, mode: RunMode) {
    if !wants(c, t) || c.reverse.scope > 0 {
        return;
    }
    if c.reverse.continuing {
        reopen(c, t);
        return;
    }
    if !halted(c, t) {
        return;
    }
    if c.reverse.open {
        finish_action(c, t);
    }
    if c.reverse.watching {
        c.reverse.watching = false;
        let _ = mt::release(c, Owner::Reverse);
    }
    let t0 = Instant::now();
    if c.reverse.at_checkpoint && !c.reverse.history.is_empty() {
        // The core stands at the last checkpoint (restored): its state is saved already.
        let cp = c.reverse.history.last_mut().unwrap();
        cp.kind = kind.to_string();
        cp.mode = mode;
        cp.at = SystemTime::now();
        cp.recording = None;
        cp.from_start = false;
        cp.end = None;
        cp.outside.clear();
        cp.wrote.clear();
    } else if let Err(e) = take_checkpoint(c, t, kind, mode) {
        crate::log_warn!("{e}");
        clear(c, &format!("checkpoint failed: {e}"));
        return;
    }
    c.reverse.at_checkpoint = false;
    c.reverse.open = true;
    let trace = c.reverse.cfg.as_ref().is_some_and(|cfg| cfg.trace) && !matches!(kind, "step" | "write" | "write register");
    if let (true, Some(core)) = (trace, traced_core(c, t)) {
        match mt::arm(c, trace_config(core), Owner::Reverse) {
            Ok(_) => {
                if let Some(cp) = c.reverse.history.last_mut() {
                    cp.from_start = true;
                }
            }
            Err(e) => crate::log_warn!("reverse: cannot record the action: {e}"),
        }
    }
    if let Some(cp) = c.reverse.history.last_mut() {
        cp.cost_ms = t0.elapsed().as_millis() as u64;
    }
}

fn take_checkpoint(c: &mut Ctx, t: usize, kind: &str, mode: RunMode) -> Result<()> {
    let regions = c.reverse.cfg.as_ref().map(|c| c.regions.clone()).unwrap_or_default();
    let regs = read_regs(c, t)?;
    let snap = read_regions(c, &regions)?;
    let st = &mut c.reverse;
    if let Some(prev) = st.history.last_mut() {
        if let Snap::Full(old) = &prev.snap {
            prev.snap = Snap::Delta(to_delta(&regions, old, &snap));
        }
    }
    let id = st.next_id;
    st.next_id += 1;
    st.history.push(Checkpoint {
        id,
        kind: kind.to_string(),
        mode,
        at: SystemTime::now(),
        regs,
        snap: Snap::Full(snap),
        outside: Vec::new(),
        wrote: Vec::new(),
        recording: None,
        from_start: false,
        end: None,
        cost_ms: 0,
    });
    let limit = st.cfg.as_ref().map_or(0, |c| c.history);
    if limit > 0 && st.history.len() > limit {
        let drop = st.history.len() - limit;
        st.history.drain(..drop);
    }
    Ok(())
}

/// A run that continues the last action (the forwarder resumes after a breakpoint whose
/// condition is false): no new checkpoint; the recording starts again here.
fn reopen(c: &mut Ctx, t: usize) {
    if c.reverse.history.is_empty() || c.reverse.open {
        return;
    }
    c.reverse.open = true;
    let trace = c.reverse.cfg.as_ref().is_some_and(|cfg| cfg.trace);
    if let Some(cp) = c.reverse.history.last_mut() {
        cp.end = None;
        cp.from_start = false;
        cp.recording = None;
    }
    if let (true, Some(core)) = (trace, traced_core(c, t)) {
        if let Err(e) = mt::arm(c, trace_config(core), Owner::Reverse) {
            crate::log_warn!("reverse: cannot record the action: {e}");
        }
    }
}

/// Closes the running action: the recording is read, the end state noted.
pub fn finish_action(c: &mut Ctx, t: usize) {
    if !c.reverse.open {
        return;
    }
    c.reverse.open = false;
    let rec = if c.mcds.armed.as_ref().is_some_and(|a| a.owner == Owner::Reverse) {
        match mt::stop(c, Owner::Reverse) {
            Ok(r) => Some(r),
            Err(e) => {
                crate::log_warn!("reverse: reading the action's recording failed: {e}");
                None
            }
        }
    } else {
        None
    };
    let reason = c.targets.get(t).map(|e| e.core.reason()).unwrap_or(DebugReason::Undefined);
    let end = read_regs(c, t).ok().map(|regs| End { pc: regs[REG_PC], pcxi: regs[REG_PCXI], reason, regs });
    if let Some(cp) = c.reverse.history.last_mut() {
        if rec.is_none() {
            cp.from_start = false;
        }
        cp.recording = rec;
        cp.end = end;
    }
}

/// The reversed core halted (seen by the poll or after a step): an action outside a scope ends.
pub fn on_halted(c: &mut Ctx, t: usize) {
    if c.reverse.target() != Some(t) {
        return;
    }
    if c.reverse.open && c.reverse.scope == 0 && c.reverse.suspended == 0 {
        finish_action(c, t);
    } else if c.reverse.watching {
        // The run without an action ended: its recording becomes the last one (not in the history).
        c.reverse.watching = false;
        if c.mcds.armed.as_ref().is_some_and(|a| a.owner == Owner::Reverse) {
            if let Err(e) = mt::stop(c, Owner::Reverse) {
                crate::log_warn!("reverse: reading the run's recording failed: {e}");
            }
        }
    }
}

/// Opens an action scope (a source step, a re-execution): one checkpoint for all its steps and runs.
pub fn begin_scope(c: &mut Ctx, t: usize, kind: &str, mode: RunMode) {
    if c.reverse.target() != Some(t) {
        return;
    }
    begin_action(c, t, kind, mode);
    c.reverse.scope += 1;
}

/// Closes an action scope; the last one ends the action.
pub fn end_scope(c: &mut Ctx, t: usize) {
    if c.reverse.target() != Some(t) || c.reverse.scope == 0 {
        return;
    }
    c.reverse.scope -= 1;
    if c.reverse.scope == 0 && c.reverse.suspended == 0 {
        finish_action(c, t);
    }
}

/// A frontend writes memory: the write is an action (undone by the next Step Back). Bytes outside
/// the regions are saved one by one (RAM only; peripheral registers are not read, their writes
/// cannot be undone).
pub fn on_write_memory(c: &mut Ctx, addr: u32, len: usize) {
    let Some(t) = c.reverse.target() else { return };
    if !wants(c, t) || c.reverse.scope > 0 || !halted(c, t) {
        return;
    }
    begin_action(c, t, "write", RunMode::Step);
    let regions = c.reverse.cfg.as_ref().map(|c| c.regions.clone()).unwrap_or_default();
    let mut outside: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut a = addr;
    let end = addr.wrapping_add(len as u32);
    while a < end {
        if regions.iter().any(|r| r.contains(a)) {
            a += 1;
            continue;
        }
        let s = a;
        while a < end && !regions.iter().any(|r| r.contains(a)) {
            a += 1;
        }
        if mcds::replay::place(0, s).1 == mcds::replay::solve::Class::Ram {
            if let Ok(old) = c.io().and_then(|mut io| io.read(bus(s), (a - s) as usize)) {
                outside.push((s, old));
            }
        }
    }
    let core = match &c.targets[t].core {
        Core::Tricore(tc) => tc.coreid as u8,
        _ => 0,
    };
    if let Some(cp) = c.reverse.history.last_mut() {
        cp.outside = outside;
        cp.wrote = vec![(mcds::replay::place(core, addr).0, len as u32)];
    }
    finish_write(c, t);
}

/// A frontend writes a register of the reversed core: an action.
pub fn on_write_reg(c: &mut Ctx, t: usize) {
    if !wants(c, t) || c.reverse.scope > 0 || !halted(c, t) {
        return;
    }
    begin_action(c, t, "write register", RunMode::Step);
    finish_write(c, t);
}

/// A write action ends at once; its end state is read after the write by the next checkpoint.
fn finish_write(c: &mut Ctx, _t: usize) {
    c.reverse.open = false;
}

/// Restores checkpoint `k` (index into the history) into the halted core. `written`: the bytes
/// the actions from `k` on wrote, when known for all of them; shared regions are then restored
/// only there, else by difference. The checkpoints after `k` are dropped; the core stands at `k`.
pub fn restore(c: &mut Ctx, k: usize, written: Option<&Written>) -> Result<Report> {
    let t0 = Instant::now();
    let cfg = c.reverse.cfg.clone().ok_or_else(|| Error::Usage("reverse debugging is off".into()))?;
    let t = cfg.target;
    if !halted(c, t) {
        return Err(Error::NotHalted(c.targets[t].name.clone()));
    }
    if k >= c.reverse.history.len() {
        return Err(Error::Usage("no such checkpoint".into()));
    }
    if c.reverse.open {
        finish_action(c, t);
    }
    let snap = snapshot_of(&c.reverse, k).ok_or_else(|| Error::Other("reverse: the snapshot is missing".into()))?;
    let mut report = Report { checkpoint: c.reverse.history[k].id, kind: c.reverse.history[k].kind.clone(), by_difference: written.is_none(), ..Default::default() };

    // Memory: private regions (and all of them without the written bytes) by difference.
    let diff_regions: Vec<usize> = (0..cfg.regions.len()).filter(|&i| cfg.regions[i].private || (written.is_none() && !cfg.regions[i].foreign)).collect();
    let current = read_regions(c, &diff_regions.iter().map(|&i| cfg.regions[i].clone()).collect::<Vec<_>>())?;
    let mut writes: Vec<(u32, Vec<u8>)> = Vec::new();
    for (n, &i) in diff_regions.iter().enumerate() {
        let r = &cfg.regions[i];
        for (off, len) in diff_runs(&current[n], &snap[i]) {
            writes.push((r.start + off as u32, snap[i][off..off + len].to_vec()));
        }
    }
    if let Some(w) = written {
        for &(a, n) in w {
            for (i, r) in cfg.regions.iter().enumerate().filter(|(_, r)| !r.private) {
                let s = a.max(r.start);
                let e = a.wrapping_add(n).min(r.end());
                if s < e {
                    let off = (s - r.start) as usize;
                    writes.push((s, snap[i][off..off + (e - s) as usize].to_vec()));
                }
            }
        }
    }
    // Writes outside the regions, newest first, so the oldest value stays.
    let mut outside: Vec<(u32, Vec<u8>)> = c.reverse.history[k..].iter().rev().flat_map(|cp| cp.outside.iter().rev().cloned()).collect();
    // Nothing may go where the application's RAM protection closes memory to the debugger: such
    // a write raises a safety alarm (a reset on the TC377 board). In the core's own regions that
    // makes the restore impossible; elsewhere those bytes are not undone.
    let coreids: Vec<u32> = c.targets.iter().filter_map(|e| match &e.core { Core::Tricore(tc) => Some(tc.coreid), _ => None }).collect();
    let windows = protection_windows(&mut c.io()?, &coreids);
    let mut closed_private: Vec<(u32, u32)> = Vec::new();
    let mut keep = |list: &mut Vec<(u32, Vec<u8>)>, report: &mut Report| {
        let mut out = Vec::new();
        for (a, bytes) in list.drain(..) {
            // Split into runs of open and closed bytes.
            let mut i = 0usize;
            while i < bytes.len() {
                let closed = closed_bytes(&windows, a + i as u32, 1) > 0;
                let mut j = i + 1;
                while j < bytes.len() && (closed_bytes(&windows, a + j as u32, 1) > 0) == closed {
                    j += 1;
                }
                let at = a + i as u32;
                if !closed {
                    out.push((at, bytes[i..j].to_vec()));
                } else if cfg.regions.iter().any(|r| r.private && r.contains(at)) {
                    closed_private.push((at, (j - i) as u32));
                } else {
                    report.refused.push((at, (j - i) as u32));
                }
                i = j;
            }
        }
        *list = out;
    };
    keep(&mut writes, &mut report);
    keep(&mut outside, &mut report);
    if !closed_private.is_empty() || (cfg.strict && !report.refused.is_empty()) {
        let list = if closed_private.is_empty() { &report.refused } else { &closed_private };
        let n: u32 = list.iter().map(|x| x.1).sum();
        let first: Vec<String> = list.iter().take(3).map(|(a, n)| format!("{a:#010x} ({n} bytes)")).collect();
        return Err(Error::Other(format!(
            "the application's RAM protection closes {n} bytes the restore has to write to the debugger ({}{}): \
             the state cannot be restored, nothing was changed",
            first.join(", "),
            if list.len() > 3 { ", ..." } else { "" }
        )));
    }
    {
        let mut io = c.io()?;
        // What is there now, for putting it back if the restore cannot be completed.
        let mut before: Vec<(u32, Vec<u8>)> = Vec::with_capacity(writes.len() + outside.len());
        for (a, bytes) in writes.iter().chain(outside.iter()) {
            before.push((*a, io.read(bus(*a), bytes.len())?));
        }
        for (a, bytes) in &writes {
            io.write(bus(*a), bytes)?;
            report.bytes += bytes.len() as u64;
        }
        for (a, old) in &outside {
            io.write(bus(*a), old)?;
            report.outside_bytes += old.len() as u64;
        }
        // Read back: an application's RAM protection can drop the debugger's writes silently.
        let mut refused_private: Vec<(u32, u32)> = Vec::new();
        for (a, bytes) in writes.iter().chain(outside.iter()) {
            let got = io.read(bus(*a), bytes.len())?;
            for (off, len) in diff_runs(&got, bytes) {
                let at = a + off as u32;
                if cfg.regions.iter().any(|r| r.private && r.contains(at)) {
                    refused_private.push((at, len as u32));
                } else {
                    report.refused.push((at, len as u32));
                }
            }
        }
        if !refused_private.is_empty() || (cfg.strict && !report.refused.is_empty()) {
            // Leave the core as it was: memory back, registers untouched, history kept.
            for (a, old) in before.iter().rev() {
                let _ = io.write(bus(*a), old);
            }
            let list = |v: &[(u32, u32)]| v.iter().take(4).map(|(a, n)| format!("{a:#010x} ({n} bytes)")).collect::<Vec<_>>().join(", ");
            let what = if refused_private.is_empty() { list(&report.refused) } else { list(&refused_private) };
            return Err(Error::Other(format!(
                "the target refused the debugger's writes to {what}{} (memory protection of the application?): \
                 the state cannot be restored, nothing was changed",
                if refused_private.len() + report.refused.len() > 4 { ", ..." } else { "" }
            )));
        }
    }

    // Registers that differ.
    let regs = c.reverse.history[k].regs.clone();
    let now = read_regs(c, t)?;
    {
        let (mut io, e) = c.target_io(t)?;
        for i in 0..NREGS {
            if now.get(i) != regs.get(i) {
                e.core.write_reg(&mut io, i, regs[i])?;
                report.registers.push(REG_NAMES[i]);
            }
        }
    }

    // The core stands at checkpoint k: later checkpoints are gone.
    let st = &mut c.reverse;
    st.history.truncate(k + 1);
    let cp = st.history.last_mut().unwrap();
    cp.snap = Snap::Full(snap);
    cp.recording = None;
    cp.from_start = false;
    cp.end = None;
    cp.outside.clear();
    cp.wrote.clear();
    st.at_checkpoint = true;
    st.open = false;
    report.ms = t0.elapsed().as_millis() as u64;
    Ok(report)
}

/// The history as JSON (oldest first).
pub fn history_json(c: &Ctx) -> serde_json::Value {
    use serde_json::json;
    let list: Vec<serde_json::Value> = c
        .reverse
        .history
        .iter()
        .enumerate()
        .map(|(i, cp)| {
            json!({
                "index": i,
                "id": cp.id,
                "kind": cp.kind,
                "time": cp.at.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0),
                "pc": format!("{:#010x}", cp.pc()),
                "end_pc": cp.end.as_ref().map(|e| format!("{:#010x}", e.pc)),
                "end_reason": cp.end.as_ref().map(|e| e.reason.name()),
                "recorded": cp.recording.is_some(),
                "from_start": cp.from_start,
                "cost_ms": cp.cost_ms,
            })
        })
        .collect();
    json!(list)
}

pub fn status_json(c: &Ctx) -> serde_json::Value {
    use serde_json::json;
    let st = &c.reverse;
    match &st.cfg {
        None => json!({"enabled": false, "cleared": st.cleared}),
        Some(cfg) => json!({
            "enabled": true,
            "target": c.targets.get(cfg.target).map(|e| e.name.clone()),
            "checkpoints": st.history.len(),
            "history": cfg.history,
            "trace": cfg.trace && traced_core(c, cfg.target).is_some(),
            "strict": cfg.strict,
            "max_hits": cfg.max_hits,
            "open": st.open,
            "recording_run": st.watching,
            "at_checkpoint": st.at_checkpoint,
            "snapshot_bytes": st.snapshot_bytes(),
            "last_cost_ms": st.history.last().map(|c| c.cost_ms),
            "regions": cfg.regions.iter().map(|r| json!({"name": r.name, "start": format!("{:#010x}", r.start), "len": r.len, "private": r.private, "foreign": r.foreign})).collect::<Vec<_>>(),
            "cleared": st.cleared,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(start: u32, len: u32) -> Region {
        Region { start, len, private: false, foreign: false, name: "r".into() }
    }

    #[test]
    fn runs_join_close_differences() {
        let a = [0u8, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2];
        let mut b = a;
        b[1] = 9;
        b[3] = 9;
        b[19] = 9;
        assert_eq!(diff_runs(&a, &b), vec![(1, 3), (19, 1)]);
        assert!(diff_runs(&a, &a).is_empty());
    }

    #[test]
    fn deltas_rebuild_older_snapshots() {
        let regions = vec![region(0x100, 8), region(0x200, 4)];
        let s0 = vec![vec![1, 2, 3, 4, 5, 6, 7, 8], vec![9, 9, 9, 9]];
        let s1 = vec![vec![1, 2, 30, 4, 5, 6, 7, 8], vec![9, 9, 9, 9]];
        let s2 = vec![vec![1, 2, 30, 4, 5, 6, 7, 80], vec![0, 9, 9, 9]];
        let mk = |snap| Checkpoint {
            id: 0,
            kind: String::new(),
            mode: RunMode::Step,
            at: SystemTime::now(),
            regs: vec![0; NREGS],
            snap,
            outside: Vec::new(),
            wrote: Vec::new(),
            recording: None,
            from_start: false,
            end: None,
            cost_ms: 0,
        };
        let st = State {
            cfg: Some(Config { target: 0, history: 0, trace: false, regions: regions.clone(), strict: false, max_hits: 0 }),
            history: vec![mk(Snap::Delta(to_delta(&regions, &s0, &s1))), mk(Snap::Delta(to_delta(&regions, &s1, &s2))), mk(Snap::Full(s2.clone()))],
            ..Default::default()
        };
        assert_eq!(snapshot_of(&st, 2).unwrap(), s2);
        assert_eq!(snapshot_of(&st, 1).unwrap(), s1);
        assert_eq!(snapshot_of(&st, 0).unwrap(), s0);
    }
}
