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
}

impl Owner {
    pub fn as_str(self) -> &'static str {
        match self {
            Owner::Snapshot => "snapshot",
            Owner::Daq => "daq",
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
}

impl Recording {
    /// The paragraphs that hold data, oldest first.
    pub fn paragraphs(&self) -> Vec<(usize, &[u8])> {
        buffer::ordered(&self.raw, self.write_ptr, self.ring)
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
}

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

/// Arms `cfg` for `owner`: MCDS on, configuration, trace RAM cleared, start.
pub fn arm(c: &mut Ctx, cfg: TraceConfig, owner: Owner) -> Result<Plan> {
    if let Some(a) = &c.mcds.armed {
        if a.owner != owner {
            return Err(Error::Resource(format!("the trace is in use ({})", a.owner.as_str())));
        }
    }
    let plan = cfg.plan().map_err(Error::Usage)?;
    init(c)?;
    let mut io = io(c)?;
    io.write32_seq(&plan.writes)?;
    // Cleared completely, so paragraphs the recording never reaches read as unwritten.
    io.write(regs::TRAM, &vec![0u8; regs::TRAM_SIZE as usize])?;
    io.write32(regs::CTRL, plan.start)?;
    c.mcds.armed = Some(Armed { cfg, plan: plan.clone(), owner, since: SystemTime::now(), finished: false, lapped: false });
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
    if !st.done {
        // Still recording: push the last word out first, else its messages are lost.
        io.write32_seq(&a.plan.flush)?;
        io.write32(regs::CTRL, regs::CTRL_STOP_HOST)?;
        st.done = true;
    }
    io.write32_seq(&config::stop_writes(st.done))?;
    let wp = io.read32(regs::FIFONOW)?;
    let raw = io.read(regs::TRAM, regs::TRAM_SIZE as usize)?;
    let halt_pc = halted_pc(c, a.cfg.core as u32);
    let halt_regs = halt_pc.and_then(|_| halted_regs(c, a.cfg.core as u32));
    let rec = Recording { raw, write_ptr: wp, ring: a.plan.ring, done: st.done, cfg: a.cfg, armed_at: a.since, stopped_at: SystemTime::now(), halt_pc, halt_regs };
    c.mcds.armed = None;
    c.mcds.last = Some(rec.clone());
    Ok(rec)
}

/// Stops without reading (the DAQ owner releasing the trace).
pub fn release(c: &mut Ctx, owner: Owner) -> Result<()> {
    match &c.mcds.armed {
        Some(a) if a.owner == owner => {
            c.mcds.armed = None;
            let mut io = io(c)?;
            io.write32_seq(&config::stop_writes(false))
        }
        _ => Ok(()),
    }
}

/// A core halted (seen by the poll): a stop-on-halt trace of that core is flushed and stopped
/// now, before anything can resume the core. Nothing is recorded while the core is halted (the
/// time stamps and ticks are gated with `dcu_sus`), so the poll's delay costs nothing.
pub fn on_halted(c: &mut Ctx, t: usize) {
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
    let r = io(c).and_then(|mut io| {
        io.write32_seq(&flush)?;
        io.write32(regs::CTRL, regs::CTRL_STOP_HOST)
    });
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

/// Arms the automatic trace before core `t` runs (a plain run, not a step). Leaves a trace
/// alone that another owner uses or that is still recording; a finished one is read first.
pub fn auto_arm(c: &mut Ctx, t: usize) {
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
    if let Err(e) = arm(c, cfg, Owner::Snapshot) {
        crate::log_warn!("trace: arming on run failed: {e}");
    }
}

/// Pushes the last trace RAM word out of a running recording (the DAQ drain, when no new data
/// came for a while: else a rarely written variable's last write waits for the next one).
pub fn flush(c: &mut Ctx) -> Result<()> {
    let Some(a) = &c.mcds.armed else { return Ok(()) };
    let flush = a.plan.flush.clone();
    io(c)?.write32_seq(&flush)
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

/// A device reset switched the MCDS off: the armed recording is gone.
pub fn on_reset(c: &mut Ctx) {
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
