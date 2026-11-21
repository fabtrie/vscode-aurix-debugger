//! miniMCDS trace on the target: switch the MCDS on, arm a configuration, read the trace RAM.
//!
//! One MCDS, one trace RAM: a recording belongs to one owner at a time - a snapshot (Tcl `mcds`,
//! the TCF/REST trace API) or the DAQ registry (traced points). The register setup comes from
//! the `mcds` crate; this module only moves it to the chip and the trace RAM back.
//!
//! A device reset switches the MCDS off (CLC back to 3) and loses the recording: [`on_reset`]
//! drops the armed state so the owner arms again.

use std::time::SystemTime;

use mcds::{buffer, config, regs, Plan, TraceConfig};

use crate::ctx::Ctx;
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// Tcl `mcds arm`, the trace panel, the REST/MCP trace operations.
    Snapshot,
    /// The DAQ registry, for traced points.
    Daq,
    /// Reverse debugging: one recording per action, held while the mode is on.
    Reverse,
}

impl Owner {
    pub fn as_str(self) -> &'static str {
        match self {
            Owner::Snapshot => "snapshot",
            Owner::Daq => "daq",
            Owner::Reverse => "reverse",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Armed {
    pub cfg: TraceConfig,
    pub plan: Plan,
    pub owner: Owner,
    pub since: SystemTime,
    /// A stop-on-halt trace the debugger stopped at the core's halt (flushed, host stop).
    pub finished: bool,
    /// A ring recording that went round the trace RAM at least once (see [`fill`]).
    pub lapped: bool,
    /// The recording this one continues ([`auto_arm`]: armed at its halt, nothing ran between).
    pub continues: Option<Box<Recording>>,
}

/// A stopped recording: the whole trace RAM and where the writer stood.
#[derive(Debug, Clone)]
pub struct Recording {
    pub raw: Vec<u8>,
    pub write_ptr: u32,
    pub ring: bool,
    /// The recorder had finished by itself (buffer full, stop event).
    pub done: bool,
    pub cfg: TraceConfig,
    pub armed_at: SystemTime,
    pub stopped_at: SystemTime,
    /// The traced core's PC if it was halted at the stop: where a stop-on-halt trace ends. The
    /// last instructions before it are usually not in the trace (the PTU reports a stretch at the
    /// next discontinuity, which the halt cuts off).
    pub halt_pc: Option<u32>,
    /// All registers of the traced core at the stop when it was halted (the debugger's numbering
    /// 0..=41: d0-d15, a0-a15, lcx, fcx, pcxi, psw, pc, icr, isp, btv, biv, syscon): the anchor
    /// of the register replay.
    pub halt_regs: Option<Vec<u32>>,
    /// Why the recording no longer ends at the traced core's current state: it ran ("run"), took
    /// an instruction step ("step") or the device was reset ("reset") after the stop. None while
    /// the core still stands where the recording stopped.
    pub ran_since: Option<&'static str>,
    /// The automatic trace's recordings before this one, oldest first, that the core ran through
    /// to its start without a gap (each armed at the previous halt): a run from a breakpoint
    /// that only reaches the next one records a few instructions, the history before it is
    /// here. Their newest paragraphs only, one trace RAM's worth with this recording's own.
    pub earlier: Vec<Earlier>,
}

/// An earlier recording joined to a later one ([`Recording::earlier`]).
#[derive(Debug, Clone)]
pub struct Earlier {
    /// Paragraphs with data, oldest first.
    pub paragraphs: Vec<(usize, Vec<u8>)>,
    /// Where the core halted at its end: where the next recording starts.
    pub halt_pc: Option<u32>,
}

impl Recording {
    /// The paragraphs that hold data, oldest first.
    pub fn paragraphs(&self) -> Vec<(usize, &[u8])> {
        buffer::ordered(&self.raw, self.write_ptr, self.ring)
    }

    /// The program trace to decode: the earlier recordings, then this one ending at `halt_pc`.
    pub fn segments(&self, halt_pc: Option<u32>) -> Vec<mcds::Segment<'_>> {
        let mut v: Vec<mcds::Segment<'_>> = self
            .earlier
            .iter()
            .map(|e| mcds::Segment { paragraphs: e.paragraphs.iter().map(|(i, p)| (*i, p.as_slice())).collect(), halt_pc: e.halt_pc })
            .collect();
        v.push(mcds::Segment { paragraphs: self.paragraphs(), halt_pc });
        v
    }

    /// Joins `prev`, which this recording continues, in front of it: as many of the newest
    /// paragraphs as fit beside this recording's own in one trace RAM's worth of bytes. A
    /// paragraph counts with the bytes it holds (the last one of a recording only up to its
    /// write pointer), so the short recordings of steps do not each use up a whole paragraph.
    fn join(&mut self, prev: Recording) {
        if self.ring && self.paragraphs().first().is_some_and(|p| p.0 != 0) {
            // The ring went round: its start is lost, so it does not follow `prev` without a gap.
            self.earlier = Vec::new();
            return;
        }
        let mut budget = (regs::TRAM_SIZE as usize).saturating_sub(self.paragraphs().iter().map(|(_, p)| p.len()).sum());
        let own = Earlier { paragraphs: prev.paragraphs().into_iter().map(|(i, p)| (i, p.to_vec())).collect(), halt_pc: prev.halt_pc };
        let mut pieces = prev.earlier;
        pieces.push(own);
        let mut kept = Vec::new();
        for mut e in pieces.into_iter().rev() {
            // The newest paragraphs of this piece that fit.
            let mut n = 0;
            for (_, p) in e.paragraphs.iter().rev() {
                if p.len() > budget {
                    break;
                }
                budget -= p.len();
                n += 1;
            }
            let whole = n == e.paragraphs.len();
            e.paragraphs.drain(..e.paragraphs.len() - n);
            if n > 0 {
                kept.push(e);
            }
            // Older pieces only behind a whole one: the history stays without holes.
            if !whole {
                break;
            }
        }
        kept.reverse();
        self.earlier = kept;
    }

    /// The trace RAM with winIDEA's 60-byte RawData header, so the analysis scripts read it.
    pub fn to_file(&self) -> Vec<u8> {
        let mut h = vec![0u8; 60];
        h[0..4].copy_from_slice(&0xBBE2_7D51u32.to_le_bytes());
        h[4..8].copy_from_slice(&2u32.to_le_bytes());
        h[8..12].copy_from_slice(&0x38u32.to_le_bytes());
        h[12..16].copy_from_slice(&0x3Cu32.to_le_bytes());
        h[16..20].copy_from_slice(&regs::TRAM_SIZE.to_le_bytes());
        h[36..40].copy_from_slice(&regs::TRAM_SIZE.to_le_bytes());
        h.extend_from_slice(&self.raw);
        h
    }
}

#[derive(Debug, Default)]
pub struct TraceState {
    pub armed: Option<Armed>,
    pub last: Option<Recording>,
    /// Set by a device reset while armed; reported once by `status`.
    pub lost_by_reset: bool,
    /// Arm this configuration whenever the traced core is run (`ops::resume`), and read the
    /// recording when it halts ([`auto_arm`], [`on_halted`]).
    pub auto: Option<TraceConfig>,
    /// The configuration last written to the MCDS: arming it again writes only the restart set
    /// ([`Plan::restart_writes`]) after checking that the MCDS still holds it ([`arm`]).
    pub configured: Option<TraceConfig>,
    /// With [`Self::auto`]: steps are recorded too, a source step (step over, into, out) as one
    /// recording and an instruction step on its own ([`begin_step`], [`end_step`]).
    pub auto_steps: bool,
    /// The core whose step is being recorded: its halts inside the step (each instruction step
    /// of a source step) do not end the recording, [`end_step`] does.
    pub step_scope: Option<usize>,
}

/// Why a manual trace is refused while reverse debugging is on.
pub const REVERSE_OWNS: &str = "the trace is used by reverse debugging; switch it off with `reverse off` or in the launch configuration";

/// Recorder state as read from the chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub ctrl: u32,
    pub write_ptr: u32,
    pub done: bool,
}

fn io(c: &mut Ctx) -> Result<crate::adapter::Io<'_>> {
    c.io()
}

/// Switches the MCDS on if a reset switched it off. Fails if the device has no miniMCDS.
pub fn init(c: &mut Ctx) -> Result<()> {
    let mut io = io(c)?;
    let clc = io.read32(regs::CLC).map_err(|e| Error::Resource(format!("no miniMCDS on this device (CLC not readable: {e})")))?;
    if clc != 0 {
        // Every other MCDS register (0x010 included) answers only with the clock on.
        io.write32(regs::CLC, 0)?;
    }
    let cfg10 = io.read32(regs::CFG10)?;
    io.write32_seq(&config::init_writes(0, cfg10))
}

pub fn status(c: &mut Ctx) -> Result<Status> {
    let mut io = io(c)?;
    let v = io.read32_many(&[regs::CTRL, regs::FIFONOW])?;
    Ok(Status { ctrl: v[0], write_ptr: v[1], done: v[0] & regs::CTRL_DONE != 0 })
}

/// Whether the MCDS still holds `plan` (written last, see [`TraceState::configured`]): its clock
/// is on (a reset switches it off) and the check words read back as written. One batch.
fn still_configured(c: &mut Ctx, plan: &Plan) -> bool {
    let check = plan.check_words();
    let mut addrs = vec![regs::CLC];
    addrs.extend(check.iter().map(|&(a, _)| a));
    match io(c).and_then(|mut io| io.read32_many(&addrs)) {
        Ok(v) => v[0] == 0 && v[1..].iter().zip(&check).all(|(got, &(_, want))| *got == want),
        Err(_) => false,
    }
}

/// Whether `owner`'s recordings are only read through [`stop`]: then the trace RAM needs no full
/// clear at the arm, the stop reads only what was written. The DAQ drain reads paragraphs while
/// recording ([`read_pieces`]) and keeps the full clear.
fn partial_ram(owner: Owner) -> bool {
    owner != Owner::Daq
}

/// Arms `cfg` for `owner`: MCDS on, configuration, trace RAM cleared, start. When the MCDS still
/// holds `cfg` from the last arm, only the restart set is written; the configuration writes go
/// as one packed sequence (a BlueBox sends them in a few messages instead of one per write).
pub fn arm(c: &mut Ctx, cfg: TraceConfig, owner: Owner) -> Result<Plan> {
    if owner != Owner::Reverse && c.reverse.enabled() {
        return Err(Error::Resource(REVERSE_OWNS.into()));
    }
    if let Some(a) = &c.mcds.armed {
        if a.owner != owner {
            return Err(Error::Resource(format!("the trace is in use ({})", a.owner.as_str())));
        }
    }
    let plan = cfg.plan().map_err(Error::Usage)?;
    let quick = c.mcds.configured.as_ref() == Some(&cfg) && still_configured(c, &plan);
    // Unknown until the writes below succeed (a failed one leaves a half-written configuration).
    c.mcds.configured = None;
    if !quick {
        init(c)?;
    }
    let mut io = io(c)?;
    io.write32_seq_packed(if quick { plan.restart_writes() } else { plan.writes.clone() }.as_slice())?;
    if partial_ram(owner) {
        // The lap marker only (see `fill`): `stop` reads up to the write pointer.
        io.write(regs::TRAM + regs::TRAM_SIZE - regs::PARAGRAPH, &vec![0u8; regs::PARAGRAPH as usize])?;
    } else {
        // Cleared completely, so paragraphs the recording never reaches read as unwritten.
        io.write(regs::TRAM, &vec![0u8; regs::TRAM_SIZE as usize])?;
    }
    io.write32(regs::CTRL, plan.start)?;
    c.mcds.configured = Some(cfg.clone());
    c.mcds.armed = Some(Armed { cfg, plan: plan.clone(), owner, since: SystemTime::now(), finished: false, lapped: false, continues: None });
    c.mcds.lost_by_reset = false;
    Ok(plan)
}

/// How much of the trace RAM holds recorded data, 0 to 1. A ring keeps writing after the end
/// and stays full from then on: it has lapped when the last paragraph holds data (the RAM is
/// cleared at arm) while the writer is before it. Checked by reading that paragraph until it
/// has, so it costs a 1 KB read per call only during the first lap.
pub fn fill(c: &mut Ctx, st: &Status) -> Result<f64> {
    let Some(a) = c.mcds.armed.as_ref() else { return Ok(0.0) };
    let last = regs::TRAM_SIZE - regs::PARAGRAPH;
    let by_ptr = (st.write_ptr as f64 / regs::TRAM_SIZE as f64).min(1.0);
    if !a.plan.ring || st.write_ptr >= last {
        return Ok(by_ptr);
    }
    if !a.lapped {
        let p = io(c)?.read(regs::TRAM + last, regs::PARAGRAPH as usize)?;
        if p.iter().any(|&b| b != 0) {
            if let Some(a) = c.mcds.armed.as_mut() {
                a.lapped = true;
            }
        }
    }
    Ok(if c.mcds.armed.as_ref().is_some_and(|a| a.lapped) { 1.0 } else { by_ptr })
}

/// Stops the recording (if it still runs) and reads the trace RAM.
pub fn stop(c: &mut Ctx, owner: Owner) -> Result<Recording> {
    let Some(a) = c.mcds.armed.clone() else {
        return Err(Error::Usage("no trace armed".into()));
    };
    if a.owner != owner {
        return Err(Error::Resource(format!("the trace is in use ({})", a.owner.as_str())));
    }
    let mut st = status(c)?;
    let mut io = io(c)?;
    let mut w = Vec::new();
    if !st.done {
        // Still recording: push the last word out first, else its messages are lost.
        w.extend_from_slice(&a.plan.flush);
        w.push((regs::CTRL, regs::CTRL_STOP_HOST));
        st.done = true;
    }
    w.extend(config::stop_writes(st.done));
    io.write32_seq_packed(&w)?;
    let wp = io.read32(regs::FIFONOW)?;
    let raw = if partial_ram(a.owner) { read_written(&mut io, wp)? } else { io.read(regs::TRAM, regs::TRAM_SIZE as usize)? };
    let halt_pc = halted_pc(c, a.cfg.core as u32);
    let halt_regs = halt_pc.and_then(|_| halted_regs(c, a.cfg.core as u32));
    let mut rec = Recording { raw, write_ptr: wp, ring: a.plan.ring, done: st.done, cfg: a.cfg, armed_at: a.since, stopped_at: SystemTime::now(), halt_pc, halt_regs, ran_since: None, earlier: Vec::new() };
    if let Some(prev) = a.continues {
        rec.join(*prev);
    }
    c.mcds.armed = None;
    c.mcds.last = Some(rec.clone());
    Ok(rec)
}

/// The trace RAM of a recording armed with only the lap marker cleared ([`partial_ram`]): the
/// whole RAM once the ring went round (the last paragraph holds data), else only the bytes before
/// the write pointer; the rest reads as zero, as after a full clear.
fn read_written(io: &mut crate::adapter::Io<'_>, wp: u32) -> Result<Vec<u8>> {
    let size = regs::TRAM_SIZE as usize;
    let last = size - regs::PARAGRAPH as usize;
    let wp = (wp as usize).min(size);
    if wp >= last {
        return io.read(regs::TRAM, size);
    }
    let v = io.read_many(&[(regs::TRAM + last as u32, size - last), (regs::TRAM, wp)])?;
    if v[0].iter().any(|&b| b != 0) {
        // Lapped: everything is this recording's.
        return io.read(regs::TRAM, size);
    }
    let mut raw = vec![0u8; size];
    raw[..wp].copy_from_slice(&v[1]);
    Ok(raw)
}

/// Stops without reading (the DAQ owner releasing the trace, reverse debugging switched off or
/// its history cleared).
pub fn release(c: &mut Ctx, owner: Owner) -> Result<()> {
    match &c.mcds.armed {
        Some(a) if a.owner == owner => {
            c.mcds.armed = None;
            let mut io = io(c)?;
            io.write32_seq_packed(&config::stop_writes(false))
        }
        _ => Ok(()),
    }
}

/// A core halted (seen by the poll): a stop-on-halt trace of that core is flushed and stopped
/// now, before anything can resume the core. Nothing is recorded while the core is halted (the
/// time stamps and ticks are gated with `dcu_sus`), so the poll's delay costs nothing.
pub fn on_halted(c: &mut Ctx, t: usize) {
    if c.mcds.step_scope == Some(t) {
        // A halt inside a recorded step: the step's end reads the recording.
        return;
    }
    let Some(a) = &c.mcds.armed else { return };
    if a.finished || !matches!(a.cfg.stop, mcds::Stop::Halt) {
        return;
    }
    let core = a.cfg.core as u32;
    let flush = a.plan.flush.clone();
    let is_traced = matches!(&c.targets[t].core, crate::target::Core::Tricore(tc) if tc.coreid == core);
    if !is_traced {
        return;
    }
    let mut w = flush;
    w.push((regs::CTRL, regs::CTRL_STOP_HOST));
    let r = io(c).and_then(|mut io| io.write32_seq_packed(&w));
    match r {
        Ok(()) => {
            if let Some(a) = c.mcds.armed.as_mut() {
                a.finished = true;
            }
            // Armed on run: read it now, so the recording (and its replay) is there at once.
            if c.mcds.auto.is_some() {
                if let Err(e) = stop(c, Owner::Snapshot) {
                    crate::log_warn!("trace: reading the recording at the halt failed: {e}");
                }
            }
        }
        Err(e) => crate::log_warn!("trace: stopping at the halt failed: {e}"),
    }
}

/// Sets (or with `None` ends) the automatic trace. Set while the traced core runs, it is armed
/// right away, so the run to the next halt is recorded too (a launch sets it after the reset
/// started the application).
pub fn set_auto(c: &mut Ctx, cfg: Option<TraceConfig>) -> Result<()> {
    let Some(cfg) = cfg else {
        c.mcds.auto = None;
        return Ok(());
    };
    if c.reverse.enabled() {
        return Err(Error::Resource(REVERSE_OWNS.into()));
    }
    let core = cfg.core as u32;
    c.mcds.auto = Some(cfg);
    let running = c.targets.iter().position(|e| {
        matches!(&e.core, crate::target::Core::Tricore(tc) if tc.coreid == core) && e.core.examined() && e.core.state() == crate::target::TargetState::Running
    });
    if let Some(t) = running.filter(|_| !c.in_reset && !c.in_reset_hook) {
        auto_arm(c, t);
    }
    Ok(())
}

/// Arms the automatic trace before core `t` runs (a plain run, not a step). Leaves a trace
/// alone that another owner uses or that is still recording; a finished one is read first.
///
/// When the core still stands where the last recording of this configuration halted (it has
/// not run, stepped or been reset since), the new recording continues it: decoded together
/// ([`Recording::earlier`]).
pub fn auto_arm(c: &mut Ctx, t: usize) {
    if c.reverse.enabled() {
        return;
    }
    let Some(cfg) = c.mcds.auto.clone() else { return };
    let traced = matches!(&c.targets[t].core, crate::target::Core::Tricore(tc) if tc.coreid == cfg.core as u32);
    if !traced {
        return;
    }
    match &c.mcds.armed {
        Some(a) if a.owner != Owner::Snapshot => {
            crate::log_info!("trace: not armed on run, the trace is in use ({})", a.owner.as_str());
            return;
        }
        Some(a) if !a.finished => return,
        Some(_) => {
            if let Err(e) = stop(c, Owner::Snapshot) {
                crate::log_warn!("trace: reading the finished recording failed: {e}");
            }
        }
        None => {}
    }
    let core = cfg.core as u32;
    let continues = match c.mcds.last.as_ref() {
        Some(r) if r.ran_since.is_none() && r.ring && r.cfg == cfg && matches!(cfg.kind, mcds::Kind::Program { .. }) && r.halt_pc.is_some() => {
            let r = r.clone();
            (halted_pc(c, core) == r.halt_pc).then(|| Box::new(r))
        }
        _ => None,
    };
    match arm(c, cfg, Owner::Snapshot) {
        Ok(_) => {
            if let Some(a) = c.mcds.armed.as_mut() {
                a.continues = continues;
            }
        }
        Err(e) => crate::log_warn!("trace: arming on run failed: {e}"),
    }
}

/// A step of core `t` begins (a source step, or an instruction step outside one): with steps
/// included in arming on run ([`TraceState::auto_steps`]) the trace is armed as for a run
/// ([`auto_arm`]: joined to the last recording when the core still stands where it halted),
/// and runs until [`end_step`].
pub fn begin_step(c: &mut Ctx, t: usize) {
    if !c.mcds.auto_steps || c.mcds.auto.is_none() || c.mcds.step_scope.is_some() {
        return;
    }
    if c.targets.get(t).is_none_or(|e| e.core.state() != crate::target::TargetState::Halted) {
        return;
    }
    auto_arm(c, t);
    if c.mcds.armed.as_ref().is_some_and(|a| a.owner == Owner::Snapshot && !a.finished) {
        c.mcds.step_scope = Some(t);
    }
}

/// The step [`begin_step`] armed for has ended: its recording is read (the trace up to where the
/// step stopped, after the earlier recordings it continues).
pub fn end_step(c: &mut Ctx, t: usize) {
    if c.mcds.step_scope != Some(t) {
        return;
    }
    c.mcds.step_scope = None;
    if c.mcds.armed.as_ref().is_some_and(|a| a.owner == Owner::Snapshot) {
        if let Err(e) = stop(c, Owner::Snapshot) {
            crate::log_warn!("trace: reading the step's recording failed: {e}");
        }
    }
}

/// Pushes the last trace RAM word out of a running recording (the DAQ drain, when no new data
/// came for a while: else a rarely written variable's last write waits for the next one).
pub fn flush(c: &mut Ctx) -> Result<()> {
    let Some(a) = &c.mcds.armed else { return Ok(()) };
    let flush = a.plan.flush.clone();
    io(c)?.write32_seq_packed(&flush)
}

/// A frontend writes `addr..addr + len`: a write into the MCDS registers may change the
/// configuration the next arm would reuse ([`TraceState::configured`]).
pub fn on_write_memory(c: &mut Ctx, addr: u32, len: usize) {
    let end = addr as u64 + len as u64;
    if (addr as u64) < regs::BASE as u64 + 0x4000 && end > regs::BASE as u64 {
        c.mcds.configured = None;
    }
}

/// Reads the given trace RAM pieces (`(paragraph, length)`), for the incremental drain.
pub fn read_pieces(c: &mut Ctx, reads: &[(usize, usize)]) -> Result<Vec<(usize, Vec<u8>)>> {
    let mut io = io(c)?;
    let mut out = Vec::with_capacity(reads.len());
    for &(i, n) in reads {
        out.push((i, io.read(regs::TRAM + i as u32 * regs::PARAGRAPH, n)?));
    }
    Ok(out)
}

/// Core `t` runs (a resume, a step, or released outside the debugger): the last recording of
/// it no longer ends where the core stands.
pub fn on_run(c: &mut Ctx, t: usize, why: &'static str) {
    let Some(r) = c.mcds.last.as_mut() else { return };
    let traced = matches!(&c.targets[t].core, crate::target::Core::Tricore(tc) if tc.coreid == r.cfg.core as u32);
    if traced && r.ran_since.is_none() {
        r.ran_since = Some(why);
    }
}

/// A reset (by the debugger or detected): the last recording is from before it.
pub fn on_device_reset(c: &mut Ctx) {
    if let Some(r) = c.mcds.last.as_mut() {
        r.ran_since = Some("reset");
    }
}

/// A device reset switched the MCDS off: the armed recording is gone.
pub fn on_reset(c: &mut Ctx) {
    crate::reverse::clear(c, "device reset");
    on_device_reset(c);
    c.mcds.step_scope = None;
    if c.mcds.armed.take().is_some() {
        c.mcds.lost_by_reset = true;
    }
}

/// The PC of TriCore `coreid` if it is halted (as last polled).
/// The registers the replay starts from, if the TriCore `coreid` is halted.
pub fn halted_regs(c: &mut Ctx, coreid: u32) -> Option<Vec<u32>> {
    let t = c.targets.iter().position(|t| matches!(&t.core, crate::target::Core::Tricore(tc) if tc.coreid == coreid && tc.state == crate::target::TargetState::Halted))?;
    let idxs: Vec<usize> = (0..=41).collect();
    let (mut io, e) = c.target_io(t).ok()?;
    e.core.read_regs(&mut io, &idxs).ok()
}

fn halted_pc(c: &mut Ctx, coreid: u32) -> Option<u32> {
    let addr = c.targets.iter().find_map(|t| match &t.core {
        crate::target::Core::Tricore(tc) if tc.coreid == coreid && tc.state == crate::target::TargetState::Halted => Some(tc.csfr(crate::arch::PC)),
        _ => None,
    })?;
    c.io().ok()?.read32(addr).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcds::{Kind, Stop, Timestamps};

    /// A stopped ring that has not lapped: `n` paragraphs with data (byte `tag`), halted at `pc`.
    fn rec(n: usize, tag: u8, pc: u32) -> Recording {
        let mut raw = vec![0u8; regs::TRAM_SIZE as usize];
        raw[..n * regs::PARAGRAPH as usize].fill(tag);
        let cfg = TraceConfig { core: 0, kind: Kind::Program { filter: None }, stop: Stop::Halt, timestamps: Timestamps::Messages };
        let t = SystemTime::now();
        Recording { raw, write_ptr: n as u32 * regs::PARAGRAPH - 0x20, ring: true, done: true, cfg, armed_at: t, stopped_at: t, halt_pc: Some(pc), halt_regs: None, ran_since: None, earlier: Vec::new() }
    }

    #[test]
    fn joined_recordings_keep_one_trace_ram() {
        // Three recordings joined in turn: 3, 2 and 4 paragraphs, then a short one of 2.
        let mut b = rec(2, 2, 0x200);
        b.join(rec(3, 1, 0x100));
        assert_eq!(b.earlier.iter().map(|e| e.paragraphs.len()).collect::<Vec<_>>(), vec![3]);
        let mut c = rec(4, 3, 0x300);
        c.join(b);
        // 8 - 4 = 4 paragraphs of history: b's own 2, then the newest 2 of the first one.
        assert_eq!(c.earlier.iter().map(|e| (e.paragraphs.len(), e.halt_pc)).collect::<Vec<_>>(), vec![(2, Some(0x100)), (2, Some(0x200))]);
        assert!(c.earlier[0].paragraphs.iter().all(|(_, p)| p[0] == 1));
        let segs = c.segments(c.halt_pc);
        assert_eq!(segs.iter().map(|s| (s.paragraphs.len(), s.halt_pc)).collect::<Vec<_>>(), vec![(2, Some(0x100)), (2, Some(0x200)), (4, Some(0x300))]);
        // A recording that fills the trace RAM by itself keeps none.
        let mut full = rec(8, 4, 0x400);
        full.join(c);
        assert!(full.earlier.is_empty());
    }

    /// A step's recording: `bytes` of messages at the start of the trace RAM.
    fn short(bytes: usize, tag: u8, pc: u32) -> Recording {
        let mut r = rec(1, tag, pc);
        r.raw[bytes..].fill(0);
        r.write_ptr = bytes as u32;
        r
    }

    #[test]
    fn short_recordings_count_with_their_bytes() {
        // A run of 4 paragraphs, then 20 steps of 64 bytes each: all steps fit beside the run's
        // newest paragraphs (20 x 64 = 1280 bytes), not one paragraph per step.
        let mut last = rec(4, 1, 0x100);
        for k in 0..20u32 {
            let mut s = short(0x40, 2, 0x200 + k);
            s.join(last);
            last = s;
        }
        let steps = last.earlier.iter().filter(|e| e.paragraphs.iter().all(|(_, p)| p[0] == 2)).count();
        assert_eq!(steps, 19, "the 19 earlier steps (the 20th is the recording itself)");
        let run: usize = last.earlier.iter().filter(|e| e.paragraphs[0].1[0] == 1).map(|e| e.paragraphs.len()).sum();
        assert_eq!(run, 4, "the run is kept whole: 4 x 1 KB + 20 x 64 bytes fit in 8 KB");
        let bytes: usize = last.segments(last.halt_pc).iter().flat_map(|s| s.paragraphs.iter()).map(|(_, p)| p.len()).sum();
        assert!(bytes <= regs::TRAM_SIZE as usize, "{bytes}");
    }
}
