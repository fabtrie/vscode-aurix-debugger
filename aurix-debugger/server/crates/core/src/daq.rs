//! Periodic sampling (DAQ) of target memory.
//!
//! A frontend opens a session with a set of points, each with its own period, and receives
//! [`DaqMsg`]es over a bounded channel. Everything here runs on the core thread: [`tick`] is called
//! from the server loop whenever [`Registry::next_due`] has come due.
//!
//! The registry knows nothing about symbols or types - a point is an absolute address and a byte
//! length, resolved once by whoever created it. That keeps name lookup, DWARF and JSON off the core
//! thread, which only moves bytes.
//!
//! **Timestamps are host-side.** They are taken after the batch returns, so they lag the actual bus
//! access by the batch duration (0.1..0.2 ms per packet plus USB/TasServer turnaround). Points
//! inside one batch are sequential PL0 transactions microseconds apart: the guarantee is
//! "consistent within one batch", not atomic. Wake jitter is the OS timer resolution, which
//! `Server::new` raises to 1 ms on Windows (the default ~15.6 ms would put a floor under every
//! rate). A value that lives in a core register or a dirty write-back cache line is not visible
//! on the bus at all.
//!
//! **A probe with a sampler of its own** ([`crate::adapter::ProbeDaq`], the BlueBox) takes the
//! plain bus points off the host: the registry configures them as probe items (naturally aligned
//! 1/2/4/8-byte pieces, at the probe's tier for the point's period) and drains the probe's buffer
//! instead of reading them. Everything else - core-relative points, points over
//! [`MAX_PROBE_POINT_LEN`], what does not fit the probe's item budget, items the probe never
//! answered - stays on the host path above, unchanged. Without such a probe (TAS, winIDEA, sim)
//! no point ever leaves the host path: [`Source::Polled`] is the only state a point gets.
//!
//! The probe sends a value when it changed (and once after each configure), stamped with its own
//! clock, so an `OnChange` point is delivered straight from the drain with that timestamp; an
//! `EveryTick` point is delivered at its period with the value the probe last sent. The items of
//! a point over 8 bytes arrive separately: such a point is consistent per item, not as a whole.

use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::time::{Duration, Instant, SystemTime};

use tas::{Batch, OpResult};

use crate::ctx::{Ctx, Cx};
use crate::error::{Error, Result};

pub type SessionId = u64;
pub type PointId = u32;

/// Shortest period a point can be given, and the grid every period is rounded up to.
pub const MIN_INTERVAL_US: u32 = 100;
pub const MAX_SESSIONS: usize = 8;
pub const MAX_POINTS_PER_SESSION: usize = 256;
/// Largest single point, and the longest coalesced read: the TAS block piece size.
pub const MAX_POINT_LEN: u32 = 1024;
pub const MAX_BYTES_PER_TICK: usize = 8 * 1024;
/// Response packets one tick may cost. Each is a round trip, so this bounds the latency the
/// sampler adds to every other job on the core thread.
pub const MAX_PKTS_PER_TICK: usize = 2;
pub const MAX_POINT_FAILS: u32 = 5;
pub const DEFAULT_QUEUE: usize = 256;
/// Backoff after a connection-level batch error, so a dead link is not hammered at 1 kHz.
pub const RETRY_BACKOFF: Duration = Duration::from_millis(200);
/// How far past the end of a read the sampler may reach to pull the next point into it, instead
/// of making a second bus access.
///
/// Measured on a TC377 over a miniWiggler: a bus access costs ~123 µs for the first and ~57 µs for
/// every one after it, while an extra *byte* inside a read already being made costs well under a
/// microsecond (a 256-byte read is 196 µs against 123 µs for four bytes). So one avoided access
/// pays for well over a hundred bytes, and reading a small hole nobody asked for is the cheaper
/// trade. 12 points 16 bytes apart: 765 µs as twelve reads, 229 µs as one.
///
/// The break-even was measured at a hole of ~105 bytes: 12 points 92 bytes apart are 1.24x faster
/// merged, 124 bytes apart 0.92x, i.e. a small loss. The default sits below that so merging can
/// only help, and it is never applied in peripheral space - see [`coalesce_gap`].
pub const DEFAULT_COALESCE_GAP: u32 = 96;
/// Start of the TC3xx peripheral space. A read here can have side effects (clear-on-read), so the
/// sampler never touches a byte inside it that nobody asked for.
const PERIPHERAL_BASE: u32 = 0xF000_0000;
/// An interval this many times the requested period is not the scheduler running late - it is a
/// stall: a flash run, a long Tcl script, a reset. The controller ignores such an interval instead
/// of correcting for it, and the sampler reports the ticks it cost as a `Gap`.
const STALL_PERIODS: u32 = 4;
/// Report a `Gap` once a session missed this many of its own ticks.
const GAP_REPORT_TICKS: u64 = 3;
/// Largest point a probe's sampler takes (eight 8-byte items); a larger one is read by the host.
pub const MAX_PROBE_POINT_LEN: u32 = 64;
/// Changes to the point set wait this long before the probe is configured again, so a frontend
/// adding twenty points costs one configure (each one restarts the probe's sampling).
const PROBE_DEBOUNCE: Duration = Duration::from_millis(50);
/// How often the probe's buffer is drained while an item is checked every millisecond or faster,
/// and otherwise. winIDEA drains every 5 to 7 ms; the BlueBox buffers 4096 samples.
const PROBE_DRAIN_FAST: Duration = Duration::from_millis(5);
const PROBE_DRAIN_SLOW: Duration = Duration::from_millis(20);
/// A probe item sends its first value right after the configure, unless the probe cannot read it
/// (it then sends nothing and says nothing). Waited for at least this long, or three check periods.
const PROBE_FIRST_SAMPLE: Duration = Duration::from_millis(200);

/// Which memory view a point is read through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Space {
    /// System bus: a plain read, mergeable into the shared batch.
    Host,
    /// As target `t` sees it (HSM-private memory, MCS program RAM). Read one by one.
    Core(usize),
}

/// When a read turns into a delivered sample.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleMode {
    /// Deliver every time the point is read.
    EveryTick,
    /// Deliver only when the masked bytes differ from the last delivered ones. The point is still
    /// *read* every period - this saves stream bandwidth, not target accesses.
    OnChange,
}

/// Where a point's values come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Read by the host, every period.
    Polled,
    /// Sampled by the probe; the host drains its buffer.
    Probe,
    /// A `probe_only` point not configured on the probe yet.
    Pending,
    /// A `probe_only` point the probe cannot take (no sampler, not eligible, never answered). It
    /// is neither sampled nor read: its owner reads it itself.
    Refused,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Polled => "host",
            Source::Probe => "probe",
            Source::Pending => "pending",
            Source::Refused => "refused",
        }
    }
}

/// State of a session, reported whenever it changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaqState {
    Running,
    /// The device reset; delivery resumes by itself once it is reachable again.
    Reset,
    /// No adapter, or a batch failed at the connection level.
    Unavailable,
    /// A tick did not fit in the per-tick budget, so some points were skipped.
    Stalled,
}

/// A point to sample, as its creator resolved it.
#[derive(Clone, Debug)]
pub struct PointCfg {
    pub addr: u32,
    pub len: u32,
    pub space: Space,
    pub interval_us: u32,
    pub mode: SampleMode,
    /// Bytes that take part in the change comparison; `None` compares all of them. Set it for a
    /// bit field, so a neighbouring field in the same storage word does not trigger a delivery.
    pub cmp_mask: Option<Vec<u8>>,
    /// Deliver an unchanged `OnChange` point again after this long. 0: never.
    pub resend_after_us: u32,
    /// Sample this point on the probe or not at all, never by host reads: for a consumer that
    /// reads it itself when the probe cannot take it ([`Source::Refused`]).
    pub probe_only: bool,
}

impl PointCfg {
    /// A point read every tick, with no change filter.
    pub fn new(addr: u32, len: u32, space: Space, interval_us: u32) -> PointCfg {
        PointCfg { addr, len, space, interval_us, mode: SampleMode::EveryTick, cmp_mask: None, resend_after_us: 0, probe_only: false }
    }
}

/// A point on the probe: the items serving it and the bytes they last delivered.
#[derive(Clone, Debug)]
struct ProbeSlot {
    /// Per item: its index in the probe's list, and where its bytes go in the point.
    items: Vec<(usize, u32)>,
    buf: Vec<u8>,
    seen: Vec<bool>,
    /// Every item must have answered by then, or the point leaves the probe.
    first_by: Instant,
}

impl ProbeSlot {
    fn complete(&self) -> bool {
        self.seen.iter().all(|&s| s)
    }
}

#[derive(Clone, Debug)]
pub struct Point {
    pub id: PointId,
    /// Resolved once when the point is added; never looked up again.
    pub addr: u32,
    pub len: u32,
    pub space: Space,
    /// The period the client asked for.
    pub period: Duration,
    pub next_due: Instant,
    /// The period the scheduler actually uses, driven down from `period` until the *delivered*
    /// rate reaches the request. See [`retune`].
    eff: Duration,
    /// Smoothed interval actually achieved, which is what the correction closes the loop on.
    ///
    /// Over steady intervals only - a stall is left out by design (see [`retune`]) - so it says
    /// how well the scheduler hits its deadlines, not what a client receives. That is
    /// [`Point::delivered_us`].
    achieved: Option<Duration>,
    last_tick: Option<Instant>,
    /// The first tick and how many there have been since, for [`Point::delivered_us`].
    first_tick: Option<Instant>,
    ticks: u64,
    pub mode: SampleMode,
    pub cmp_mask: Option<Vec<u8>>,
    pub resend_after: Option<Duration>,
    /// Consecutive real read errors; at [`MAX_POINT_FAILS`] the point is muted.
    pub fails: u32,
    pub muted: bool,
    /// Last *delivered* bytes. `None` makes the next read a baseline delivery.
    last: Option<Vec<u8>>,
    last_sent: Instant,
    /// Where the values come from; only a probe with a sampler moves it off [`Source::Polled`].
    pub source: Source,
    pub probe_only: bool,
    probe: Option<ProbeSlot>,
}

impl Point {
    pub fn interval_us(&self) -> u32 {
        self.period.as_micros() as u32
    }

    /// The period the scheduler settled on, which is shorter than the one asked for by however
    /// much the wait overshoots.
    pub fn eff_us(&self) -> u32 {
        self.eff.as_micros() as u32
    }

    /// Smoothed interval actually achieved, once there is one.
    pub fn achieved_us(&self) -> Option<u32> {
        self.achieved.map(|d| d.as_micros() as u32)
    }

    /// Mean interval between the samples actually taken, stalls and all: what a client gets.
    ///
    /// `achieved` cannot answer that, because it skips every interval it calls a stall. A
    /// sampler that kept missing whole periods - 15.6 ms holes at 1 kHz, on a host whose timer
    /// requests were being ignored - reported "achieved 903 us" while delivering ~190 samples/s.
    pub fn delivered_us(&self) -> Option<u32> {
        let (first, last) = (self.first_tick?, self.last_tick?);
        (self.ticks > 1).then(|| (last.saturating_duration_since(first).as_micros() as u64 / (self.ticks - 1)) as u32)
    }

    /// Forgets the change-detection baseline, so the next read is delivered.
    fn rebase(&mut self) {
        self.last = None;
    }

    /// The bytes the probe last sent for this point, once every item answered.
    pub fn probe_value(&self) -> Option<&[u8]> {
        self.probe.as_ref().filter(|s| s.complete()).map(|s| s.buf.as_slice())
    }

    /// A point on the probe only needs a tick of its own to deliver at its period (`EveryTick`)
    /// or to resend; `OnChange` deliveries come from the drain.
    fn ticks(&self) -> bool {
        match self.source {
            Source::Polled => true,
            Source::Probe => self.mode == SampleMode::EveryTick || self.resend_after.is_some(),
            Source::Pending | Source::Refused => false,
        }
    }

    /// Back to where a point starts when no probe sampler serves it.
    fn leave_probe(&mut self, now: Instant) {
        self.probe = None;
        self.source = if self.probe_only { Source::Refused } else { Source::Polled };
        self.next_due = self.next_due.min(now);
    }
}

/// One sampled value: the point and its raw bytes.
pub type Sample = (PointId, Vec<u8>);

#[derive(Clone, Debug)]
pub enum DaqMsg {
    /// One tick: every point that was due and passed its change filter.
    Samples { seq: u64, t_us: u64, values: Vec<Sample> },
    /// Messages discarded since the last report because the consumer did not keep up.
    Overrun { dropped: u64, t_us: u64 },
    /// Ticks skipped because the core thread was busy (flashing, a long Tcl script).
    Gap { missed: u64, t_us: u64 },
    /// A point could not be read. `muted` means it is no longer scheduled.
    PointError { point: PointId, err: String, muted: bool },
    State { state: DaqState, t_us: u64 },
}

#[derive(Clone, Copy, Debug)]
pub struct SessionCfg {
    /// Messages buffered for the consumer. Clamped to 16..=4096.
    pub queue: usize,
}

impl Default for SessionCfg {
    fn default() -> Self {
        SessionCfg { queue: DEFAULT_QUEUE }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SessionStats {
    pub points: usize,
    pub seq: u64,
    pub dropped: u64,
    pub missed: u64,
}

pub struct Session {
    pub id: SessionId,
    pub epoch: Instant,
    /// Unix microseconds of `epoch`, so a consumer can map `t_us` to wall clock once.
    pub epoch_unix_us: u64,
    pub points: Vec<Point>,
    pub paused: bool,
    next_point: PointId,
    seq: u64,
    dropped: u64,
    missed: u64,
    state: DaqState,
    tx: SyncSender<DaqMsg>,
    closing: bool,
}

impl Session {
    fn t_us(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.epoch).as_micros() as u64
    }

    fn point_mut(&mut self, p: PointId) -> Result<&mut Point> {
        self.points.iter_mut().find(|x| x.id == p).ok_or_else(|| Error::Usage(format!("no DAQ point {p}")))
    }

    /// Queues a message, counting it as dropped when the consumer is behind.
    fn send(&mut self, msg: DaqMsg) {
        match self.tx.try_send(msg) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => self.dropped += 1,
            // The consumer is gone: reaped at the end of the tick. This is the safety net that
            // stops a crashed frontend from sampling forever.
            Err(TrySendError::Disconnected(_)) => self.closing = true,
        }
    }

    fn set_state(&mut self, state: DaqState, at: Instant) {
        if self.state != state {
            self.state = state;
            let t_us = self.t_us(at);
            self.send(DaqMsg::State { state, t_us });
        }
    }

    /// Reports the overrun that accumulated, then the samples of this tick.
    fn deliver(&mut self, values: Vec<Sample>, at: Instant) {
        let t_us = self.t_us(at);
        if self.dropped > 0 {
            let d = std::mem::take(&mut self.dropped);
            match self.tx.try_send(DaqMsg::Overrun { dropped: d, t_us }) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => self.dropped = d,
                Err(TrySendError::Disconnected(_)) => {
                    self.closing = true;
                    return;
                }
            }
        }
        if self.missed >= GAP_REPORT_TICKS {
            let missed = std::mem::take(&mut self.missed);
            self.send(DaqMsg::Gap { missed, t_us });
        }
        if !values.is_empty() {
            self.seq += 1;
            let seq = self.seq;
            self.send(DaqMsg::Samples { seq, t_us, values });
        }
    }

    pub fn stats(&self) -> SessionStats {
        SessionStats { points: self.points.len(), seq: self.seq, dropped: self.dropped, missed: self.missed }
    }
}

/// All DAQ sessions. Lives in [`Ctx`]; only the core thread touches it.
#[derive(Default)]
pub struct Registry {
    sessions: Vec<Session>,
    next_id: SessionId,
    /// Earliest deadline over all schedulable points. `None`: nothing to do, and the server loop
    /// pays only one `Option` comparison per iteration.
    next_due: Option<Instant>,
    /// Where the next truncated tick starts reading, so the per-tick budget is shared instead of
    /// always falling on the same (highest-addressed) points.
    rotate: usize,
    retry_after: Option<Instant>,
    probe: ProbeState,
}

/// The registry's side of a probe sampler. Untouched (all `None`/empty) unless the adapter offers
/// one.
#[derive(Default)]
struct ProbeState {
    /// The last tick found a probe sampler.
    capable: bool,
    /// The point set changed: configure the probe at this instant.
    dirty_at: Option<Instant>,
    generation: Option<u64>,
    /// Per configured item: the session and point it serves.
    owners: Vec<(SessionId, PointId)>,
    next_drain: Option<Instant>,
    drain_every: Duration,
    /// Points whose items never answered: read by the host (or refused) from now on.
    demoted: Vec<(SessionId, PointId)>,
    overflow_logged: bool,
}

impl Registry {
    pub fn next_due(&self) -> Option<Instant> {
        self.next_due
    }

    /// Where a point's values come from.
    pub fn source(&self, id: SessionId, p: PointId) -> Option<Source> {
        self.get(id)?.points.iter().find(|x| x.id == p).map(|x| x.source)
    }

    /// The point set changed: configure the probe again after [`PROBE_DEBOUNCE`].
    fn probe_dirty(&mut self, now: Instant) {
        self.probe.dirty_at.get_or_insert(now + PROBE_DEBOUNCE);
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    pub fn sessions(&self) -> impl Iterator<Item = &Session> {
        self.sessions.iter()
    }

    pub fn open(&mut self, cfg: SessionCfg, now: Instant) -> Result<(SessionId, Receiver<DaqMsg>)> {
        if self.sessions.len() >= MAX_SESSIONS {
            return Err(Error::Usage(format!("too many DAQ sessions (max {MAX_SESSIONS})")));
        }
        let (tx, rx) = sync_channel(cfg.queue.clamp(16, 4096));
        self.next_id += 1;
        let id = self.next_id;
        let epoch_unix_us = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_micros() as u64).unwrap_or(0);
        self.sessions.push(Session {
            id,
            epoch: now,
            epoch_unix_us,
            points: Vec::new(),
            paused: false,
            next_point: 0,
            seq: 0,
            dropped: 0,
            missed: 0,
            state: DaqState::Running,
            tx,
            closing: false,
        });
        Ok((id, rx))
    }

    pub fn close(&mut self, id: SessionId) -> bool {
        let n = self.sessions.len();
        self.sessions.retain(|s| s.id != id);
        let removed = self.sessions.len() != n;
        if removed {
            self.probe.demoted.retain(|(s, _)| *s != id);
            self.probe_dirty(Instant::now());
        }
        self.reseat();
        removed
    }

    pub fn get(&self, id: SessionId) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    fn session_mut(&mut self, id: SessionId) -> Result<&mut Session> {
        self.sessions.iter_mut().find(|s| s.id == id).ok_or_else(|| Error::Usage(format!("no DAQ session {id}")))
    }

    pub fn stats(&self, id: SessionId) -> Option<SessionStats> {
        self.get(id).map(Session::stats)
    }

    /// Adds points, returning each one with the period it was actually granted.
    pub fn add_points(&mut self, id: SessionId, pts: &[PointCfg], now: Instant) -> Result<Vec<(PointId, u32)>> {
        let s = self.session_mut(id)?;
        if s.points.len() + pts.len() > MAX_POINTS_PER_SESSION {
            return Err(Error::Usage(format!("too many DAQ points (max {MAX_POINTS_PER_SESSION} per session)")));
        }
        let mut out = Vec::with_capacity(pts.len());
        for cfg in pts {
            if cfg.len == 0 || cfg.len > MAX_POINT_LEN {
                return Err(Error::Usage(format!("DAQ point length {} must be 1..={MAX_POINT_LEN}", cfg.len)));
            }
            if let Some(m) = &cfg.cmp_mask {
                if m.len() != cfg.len as usize {
                    return Err(Error::Usage("DAQ comparison mask must be as long as the point".into()));
                }
            }
            let interval_us = quantize(cfg.interval_us);
            s.next_point += 1;
            let pid = s.next_point;
            s.points.push(Point {
                id: pid,
                addr: cfg.addr,
                len: cfg.len,
                space: cfg.space,
                period: Duration::from_micros(interval_us as u64),
                next_due: phase(s.epoch, now, interval_us),
                // Start at the requested period and let retune() find the shorter one it needs.
                eff: Duration::from_micros(interval_us as u64),
                achieved: None,
                first_tick: None,
                ticks: 0,
                last_tick: None,
                mode: cfg.mode,
                cmp_mask: cfg.cmp_mask.clone(),
                resend_after: (cfg.resend_after_us > 0).then(|| Duration::from_micros(cfg.resend_after_us as u64)),
                fails: 0,
                muted: false,
                last: None,
                last_sent: now,
                source: if cfg.probe_only { Source::Pending } else { Source::Polled },
                probe_only: cfg.probe_only,
                probe: None,
            });
            out.push((pid, interval_us));
        }
        self.probe_dirty(now);
        self.reseat();
        Ok(out)
    }

    pub fn remove_points(&mut self, id: SessionId, pts: &[PointId]) -> Result<()> {
        let s = self.session_mut(id)?;
        s.points.retain(|p| !pts.contains(&p.id));
        self.probe.demoted.retain(|(s, p)| *s != id || !pts.contains(p));
        self.probe_dirty(Instant::now());
        self.reseat();
        Ok(())
    }

    /// Retimes a point, returning the period actually granted.
    pub fn set_interval(&mut self, id: SessionId, p: PointId, interval_us: u32, now: Instant) -> Result<u32> {
        let interval_us = quantize(interval_us);
        let pt = self.session_mut(id)?.point_mut(p)?;
        pt.period = Duration::from_micros(interval_us as u64);
        pt.next_due = now;
        // The probe's tier follows the period.
        self.probe_dirty(now);
        self.reseat();
        Ok(interval_us)
    }

    /// Switches a delivery mode. The baseline is dropped, so the next read is delivered.
    pub fn set_mode(&mut self, id: SessionId, p: PointId, mode: SampleMode) -> Result<()> {
        let pt = self.session_mut(id)?.point_mut(p)?;
        pt.mode = mode;
        pt.rebase();
        Ok(())
    }

    /// Re-arms a point that was muted after repeated read errors.
    pub fn rearm(&mut self, id: SessionId, p: PointId, now: Instant) -> Result<()> {
        let pt = self.session_mut(id)?.point_mut(p)?;
        pt.muted = false;
        pt.fails = 0;
        pt.next_due = now;
        pt.rebase();
        self.probe_dirty(now);
        self.reseat();
        Ok(())
    }

    pub fn set_paused(&mut self, id: SessionId, paused: bool, now: Instant) -> Result<()> {
        let s = self.session_mut(id)?;
        s.paused = paused;
        if !paused {
            // Resuming re-sends a baseline: a client must not keep believing a pre-pause value.
            for p in &mut s.points {
                p.next_due = now;
                p.rebase();
            }
        }
        // A paused session's points come off the probe, and go back on when it resumes.
        self.probe_dirty(now);
        self.reseat();
        Ok(())
    }

    /// Recomputes the cached deadline and reaps sessions whose consumer is gone.
    fn reseat(&mut self) {
        let n = self.sessions.len();
        self.sessions.retain(|s| !s.closing);
        if self.sessions.len() != n {
            self.probe_dirty(Instant::now());
        }
        let live = self.sessions.iter().filter(|s| !s.paused).flat_map(|s| s.points.iter().filter(|p| !p.muted && p.ticks()));
        self.next_due = live.map(|p| p.next_due).min();
        // The probe's own deadlines: only once a tick found a probe sampler (or a point that
        // wants one is waiting), so a host-only probe never wakes for them.
        let waiting = self.sessions.iter().any(|s| s.points.iter().any(|p| p.source == Source::Pending));
        if self.probe.capable || waiting {
            let drain = (!self.probe.owners.is_empty()).then_some(self.probe.next_drain).flatten();
            for d in [self.probe.dirty_at, drain].into_iter().flatten() {
                self.next_due = Some(self.next_due.map_or(d, |n| n.min(d)));
            }
        }
        if let (Some(due), Some(retry)) = (self.next_due, self.retry_after) {
            self.next_due = Some(due.max(retry));
        }
    }
}

/// The first deadline of a point, on the session's own grid of `interval_us`.
///
/// Phase matters: without it every point would start at the instant it happened to be added, so
/// points subscribed together (resolving a name takes a moment each) would each land on their own
/// tick and produce one message apiece instead of sharing one. Aligning to the epoch also keeps
/// harmonic rates - 1 kHz and 100 Hz - sharing every tenth tick forever.
fn phase(epoch: Instant, now: Instant, interval_us: u32) -> Instant {
    let step = interval_us.max(1) as u64;
    let since = now.saturating_duration_since(epoch).as_micros() as u64;
    epoch + Duration::from_micros(since.div_ceil(step) * step)
}

/// Rounds a requested period up to the [`MIN_INTERVAL_US`] grid.
pub fn quantize(interval_us: u32) -> u32 {
    let g = MIN_INTERVAL_US;
    interval_us.max(g).div_ceil(g).saturating_mul(g)
}

/// The sampling tick, called from the server loop when [`Registry::next_due`] has come due.
pub fn tick(cx: &Cx, now: Instant) {
    tick_at(cx, now, None)
}

/// [`tick`] with an explicit sample timestamp. Production passes `None`, which reads the clock
/// after the batch returns; tests pass their simulated instant so the output is deterministic.
pub fn tick_at(cx: &Cx, now: Instant, stamp: Option<Instant>) {
    cx.with(|c| tick_inner(c, now, stamp));
}

/// Clears every change-detection baseline and backs the sampler off, so a client is not left
/// believing a pre-reset value. Called where the core notices a device reset.
pub fn on_reset(c: &mut Ctx, now: Instant) {
    let retry = now + RETRY_BACKOFF;
    for s in &mut c.daq.sessions {
        s.set_state(DaqState::Reset, now);
        for p in &mut s.points {
            p.rebase();
            p.next_due = p.next_due.max(retry);
        }
    }
    c.daq.retry_after = Some(retry);
    // The probe may have lost its item list with the target connection.
    if c.daq.probe.capable {
        c.daq.probe.dirty_at = Some(retry);
    }
    c.daq.reseat();
}

/// Re-aims a point from the interval it actually achieved.
///
/// Everything between the deadline and the sample - the timer overshoot, the probe round trip, a
/// run-state poll that got in the way - shows up as one number here, so the correction needs no
/// model of any of them. Schedule short, watch what comes out, close the gap.
///
/// Returns the interval that was measured, or `None` on the first tick and after a stall.
fn retune(p: &mut Point, now: Instant) -> Option<Duration> {
    let prev = p.last_tick.replace(now)?;
    let actual = now.saturating_duration_since(prev);
    // Ignore an interval that is orders out: a flash run or a reset, not the steady state. It is
    // reported as a gap instead of being corrected for.
    if actual > p.period * STALL_PERIODS {
        return None;
    }
    let a = match p.achieved {
        Some(e) => (e * 7 + actual) / 8,
        None => actual,
    };
    p.achieved = Some(a);
    let (target, got) = (p.period.as_micros() as i64, a.as_micros().max(1) as i64);
    let eff = p.eff.as_micros() as i64;
    // Proportional, damped by four, so a noisy interval cannot make the period oscillate.
    let step = (got - target) / 4;
    let next = (eff - step).clamp(MIN_INTERVAL_US as i64, target);
    p.eff = Duration::from_micros(next as u64);
    Some(actual)
}

/// Advances a point past `now`, returning the periods a stall cost. Deadlines are absolute, so the
/// long-run rate stays steady; a stall produces a gap and never a burst of catch-up samples.
fn advance(p: &mut Point, now: Instant) -> u64 {
    let prev = p.last_tick;
    p.first_tick.get_or_insert(now);
    p.ticks += 1;
    let steady = retune(p, now).is_some();
    p.next_due += p.eff;
    if p.next_due <= now {
        let period = p.eff.as_nanos().max(1);
        let skipped = ((now - p.next_due).as_nanos() / period) as u64 + 1;
        p.next_due += p.eff * skipped.min(u32::MAX as u64) as u32;
    }
    // Landing past the *scheduled* deadline is the normal case, not a miss: the schedule is
    // deliberately shorter than the period the client asked for and retune() is closing the gap.
    // Only a real stall is worth reporting, and in the client's own periods rather than the
    // shorter ones the scheduler happens to be running.
    match prev {
        Some(prev) if !steady => {
            let gap = now.saturating_duration_since(prev).as_micros();
            (gap / p.period.as_micros().max(1)).saturating_sub(1) as u64
        }
        _ => 0,
    }
}

/// Whether `new` differs from the last delivered bytes, over `mask` if there is one.
fn changed(last: Option<&Vec<u8>>, new: &[u8], mask: Option<&Vec<u8>>) -> bool {
    match (last, mask) {
        (None, _) => true,
        (Some(prev), None) => prev.as_slice() != new,
        (Some(prev), Some(m)) => prev.iter().zip(new).zip(m).any(|((a, b), m)| (a ^ b) & m != 0),
    }
}

/// How far a run starting at `run_addr` may reach to swallow a point at `addr..addr + len`.
///
/// `gap` everywhere the merged read stays in plain memory, and zero the moment either end is in
/// peripheral space: a byte read there that nobody asked for can clear a flag. Points that are
/// genuinely adjacent still merge in peripheral space, because that reads only what was asked for.
fn coalesce_gap(run_addr: u32, addr: u32, len: u32, gap: u32) -> u32 {
    let end = addr.saturating_add(len);
    if run_addr >= PERIPHERAL_BASE || end > PERIPHERAL_BASE {
        0
    } else {
        gap
    }
}

/// A run of coalesced reads: one batch operation serving one or more points.
struct Run {
    addr: u32,
    len: u32,
    handle: usize,
}

/// What one tick decided to read, handed from [`queue_due`] to [`apply`].
///
/// The two halves are separate so the reads can ride in a batch somebody else already owns. That
/// is what [`crate::ops::poll_all`] does: a DAQ point that falls due just as the run-state poll
/// runs is served by the poll's own round trip instead of waiting behind it.
pub struct Plan {
    host: Vec<(usize, usize)>,
    core_pts: Vec<(usize, usize)>,
    runs: Vec<Run>,
    /// Per entry of `host`: the run serving it and the offset into that run. `None` once the
    /// per-tick budget ran out.
    of_point: Vec<Option<(usize, u32)>>,
    truncated: bool,
}

/// Queues the reads every due point needs into `b`, returning what [`apply`] has to do with the
/// results.
///
/// `None` means there is nothing to sample. That covers the backoff and device-unavailable cases
/// too: they report themselves and reschedule, so the caller has nothing left to do.
///
/// A returned `Plan` has mutated nothing - no deadline has moved and no baseline has changed - so
/// dropping it because the batch failed simply leaves the points due and the next tick retries.
/// That is what makes it safe to hand these reads to a batch whose owner has its own error paths.
pub fn queue_due(c: &mut Ctx, now: Instant, b: &mut Batch) -> Option<Plan> {
    if c.daq.sessions.is_empty() {
        c.daq.reseat();
        return None;
    }
    if let Some(t) = c.daq.retry_after {
        if now < t {
            return None;
        }
        c.daq.retry_after = None;
    }
    if c.in_reset || !c.adapter.as_ref().is_some_and(|a| a.initialized()) {
        for s in &mut c.daq.sessions {
            s.set_state(DaqState::Unavailable, now);
        }
        c.daq.retry_after = Some(now + RETRY_BACKOFF);
        c.daq.reseat();
        return None;
    }

    // Everything due this tick, split by memory view.
    let mut host: Vec<(usize, usize)> = Vec::new();
    let mut core_pts: Vec<(usize, usize)> = Vec::new();
    for (si, s) in c.daq.sessions.iter().enumerate() {
        if s.paused || s.closing {
            continue;
        }
        for (pi, p) in s.points.iter().enumerate() {
            if p.muted || now < p.next_due || p.source != Source::Polled {
                continue;
            }
            match p.space {
                Space::Host => host.push((si, pi)),
                Space::Core(_) => core_pts.push((si, pi)),
            }
        }
    }
    if host.is_empty() && core_pts.is_empty() {
        c.daq.reseat();
        return None;
    }

    let limits = c.adapter.as_ref().map(|a| a.limits()).unwrap_or_default();
    let max_runs = limits.max_num_rw.max(1) * MAX_PKTS_PER_TICK;

    // Sort by address, then merge neighbours into one read. Two points merge when the hole
    // between them is small enough to be worth reading: see `coalesce_gap`.
    let mut order: Vec<usize> = (0..host.len()).collect();
    order.sort_by_key(|&i| {
        let p = &c.daq.sessions[host[i].0].points[host[i].1];
        (p.addr, p.len)
    });
    // Truncation must not always fall on the same points. Starting where the last truncated tick
    // stopped shares the budget round-robin instead of permanently starving the high addresses -
    // which is where the peripherals are, so they were exactly what got starved.
    let n_host = order.len();
    if n_host > 0 {
        order.rotate_left(c.daq.rotate % n_host);
    }
    let gap = c.daq_gap;
    let mut runs: Vec<Run> = Vec::new();
    let mut of_point: Vec<Option<(usize, u32)>> = vec![None; host.len()];
    let mut bytes = 0usize;
    let mut truncated = false;
    let mut cur: Option<usize> = None;
    for &i in &order {
        let (addr, len) = {
            let p = &c.daq.sessions[host[i].0].points[host[i].1];
            (p.addr, p.len)
        };
        if let Some(ci) = cur {
            let (ra, rl) = (runs[ci].addr, runs[ci].len);
            let reach = ra as u64 + rl as u64 + coalesce_gap(ra, addr, len, gap) as u64;
            if addr >= ra && addr as u64 <= reach {
                let new_len = ((addr as u64 + len as u64 - ra as u64) as u32).max(rl);
                let grow = (new_len - rl) as usize;
                if new_len <= MAX_POINT_LEN && bytes + grow <= MAX_BYTES_PER_TICK {
                    runs[ci].len = new_len;
                    bytes += grow;
                    of_point[i] = Some((ci, addr - ra));
                    continue;
                }
            }
        }
        if runs.len() >= max_runs || bytes + len as usize > MAX_BYTES_PER_TICK {
            truncated = true;
            continue;
        }
        bytes += len as usize;
        runs.push(Run { addr, len, handle: 0 });
        cur = Some(runs.len() - 1);
        of_point[i] = Some((runs.len() - 1, 0));
    }
    // Only a truncated tick rotates; otherwise every tick would start at a different address and
    // split one run in two for nothing. Resume at the first point this tick could not serve.
    let unserved = order.iter().position(|&i| of_point[i].is_none());
    c.daq.rotate = match (truncated, unserved) {
        (true, Some(k)) => (c.daq.rotate + k) % n_host.max(1),
        _ => 0,
    };

    // Handles are indices into the whole batch, so they stay valid when the batch is shared.
    for r in runs.iter_mut() {
        r.handle = b.read(r.addr as u64, r.len as usize).0;
    }
    Some(Plan { host, core_pts, runs, of_point, truncated })
}

/// Turns the results of the batch [`queue_due`] filled into delivered samples: change filter,
/// error accounting, deadlines.
///
/// `stamp` is the sample timestamp; `None` reads the clock here, which is right after the batch
/// returned. Tests pass their simulated instant so the output is deterministic.
pub fn apply(c: &mut Ctx, plan: Plan, now: Instant, results: &[OpResult], stamp: Option<Instant>) {
    let Plan { host, core_pts, runs, of_point, truncated } = plan;

    // Core-relative points cannot join the batch: the HSM window and MCS program RAM need their
    // own access sequence. Each one costs a round trip, so keep them slow.
    let mut core_data: Vec<std::result::Result<Vec<u8>, String>> = Vec::with_capacity(core_pts.len());
    for &(si, pi) in &core_pts {
        let p = &c.daq.sessions[si].points[pi];
        let (t, addr, len) = match p.space {
            Space::Core(t) => (t, p.addr, p.len as usize),
            Space::Host => continue,
        };
        let r = match c.target_io(t) {
            Ok((mut io, e)) => e.core.read_memory(&mut io, addr, len).map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        core_data.push(r);
    }

    let at = stamp.unwrap_or_else(Instant::now);

    let n = c.daq.sessions.len();
    let mut per_session: Vec<Vec<Sample>> = (0..n).map(|_| Vec::new()).collect();
    let mut missed_by_session: Vec<u64> = vec![0; n];
    let mut errors: Vec<(usize, PointId, String, bool)> = Vec::new();

    for (i, &(si, pi)) in host.iter().enumerate() {
        let slot = of_point[i];
        let p = &mut c.daq.sessions[si].points[pi];
        missed_by_session[si] += advance(p, now);
        // Skipped by the per-tick budget: the deadline still moved, so it is not replayed.
        let Some((ri, off)) = slot else { continue };
        let res = &results[runs[ri].handle];
        if !res.ok() {
            // "Consequential" only means an earlier access in the same packet failed: not this
            // point's own fault, and it must never count towards muting.
            if res.err != tas::PL0_ERR_CONSEQUENTIAL {
                p.fails += 1;
                let muted = p.fails >= MAX_POINT_FAILS;
                p.muted = muted;
                if p.fails == 1 || muted {
                    errors.push((si, p.id, tas::pl_err_name(res.err).to_string(), muted));
                }
            }
            continue;
        }
        p.fails = 0;
        let (lo, hi) = (off as usize, off as usize + p.len as usize);
        if hi > res.data.len() {
            continue;
        }
        let new = &res.data[lo..hi];
        let deliver = p.mode == SampleMode::EveryTick
            || changed(p.last.as_ref(), new, p.cmp_mask.as_ref())
            || p.resend_after.is_some_and(|d| at.saturating_duration_since(p.last_sent) >= d);
        if deliver {
            p.last = Some(new.to_vec());
            p.last_sent = at;
            let id = p.id;
            per_session[si].push((id, new.to_vec()));
        }
    }

    for (k, &(si, pi)) in core_pts.iter().enumerate() {
        let p = &mut c.daq.sessions[si].points[pi];
        missed_by_session[si] += advance(p, now);
        match &core_data[k] {
            Err(e) => {
                p.fails += 1;
                let muted = p.fails >= MAX_POINT_FAILS;
                p.muted = muted;
                if p.fails == 1 || muted {
                    errors.push((si, p.id, e.clone(), muted));
                }
            }
            Ok(new) => {
                p.fails = 0;
                let deliver = p.mode == SampleMode::EveryTick
                    || changed(p.last.as_ref(), new, p.cmp_mask.as_ref())
                    || p.resend_after.is_some_and(|d| at.saturating_duration_since(p.last_sent) >= d);
                if deliver {
                    p.last = Some(new.clone());
                    p.last_sent = at;
                    let id = p.id;
                    per_session[si].push((id, new.clone()));
                }
            }
        }
    }

    for (si, values) in per_session.into_iter().enumerate() {
        let s = &mut c.daq.sessions[si];
        s.missed += missed_by_session[si];
        s.set_state(if truncated { DaqState::Stalled } else { DaqState::Running }, at);
        s.deliver(values, at);
    }
    for (si, point, err, muted) in errors {
        c.daq.sessions[si].send(DaqMsg::PointError { point, err, muted });
    }
    c.daq.reseat();
}

/// Backs every session off after a connection-level batch failure, so a dead link is not hammered
/// at the point rate.
pub fn batch_failed(c: &mut Ctx, now: Instant) {
    for s in &mut c.daq.sessions {
        s.set_state(DaqState::Unavailable, now);
        for p in &mut s.points {
            advance(p, now);
        }
    }
    c.daq.retry_after = Some(now + RETRY_BACKOFF);
    c.daq.reseat();
}

/// Splits `len` bytes at `addr` into naturally aligned items of at most `max` bytes: (offset, size).
fn split_items(addr: u32, len: u32, max: u8) -> Vec<(u32, u8)> {
    let max = (max.clamp(1, 8) as u32).next_power_of_two().min(8);
    let mut out = Vec::new();
    let mut off = 0;
    while off < len {
        let a = addr.wrapping_add(off);
        let mut size = max;
        while size > 1 && (a % size != 0 || size > len - off) {
            size /= 2;
        }
        out.push((off, size as u8));
        off += size;
    }
    out
}

/// The instant the probe read a sample stamped `t_us`, given that its clock read `now_us` at `at`.
fn probe_instant(at: Instant, now_us: u64, t_us: u64) -> Instant {
    at.checked_sub(Duration::from_micros(now_us.saturating_sub(t_us))).unwrap_or(at)
}

/// The probe's sampler went away (`bluebox daq_offload off`): its points go back to the host.
fn probe_withdrawn(c: &mut Ctx, now: Instant) {
    let reg = &mut c.daq;
    for s in &mut reg.sessions {
        for p in &mut s.points {
            if matches!(p.source, Source::Probe | Source::Pending) {
                p.leave_probe(now);
            }
        }
    }
    reg.probe = ProbeState::default();
    reg.reseat();
}

/// The probe's share of a tick: configure it when the point set changed, drain its buffer,
/// deliver `EveryTick` probe points at their period, and take points whose items never answered
/// off it. Does nothing without a probe sampler.
fn probe_step(c: &mut Ctx, now: Instant, stamp: Option<Instant>) {
    let has = c.adapter.as_deref_mut().filter(|a| a.initialized()).is_some_and(|a| a.probe_daq().is_some());
    if !has {
        let waiting = c.daq.sessions.iter().any(|s| s.points.iter().any(|p| matches!(p.source, Source::Probe | Source::Pending)));
        if c.daq.probe.capable || waiting {
            probe_withdrawn(c, now);
        }
        return;
    }
    if c.in_reset {
        return;
    }
    c.daq.probe.capable = true;
    let gen = c.adapter.as_deref_mut().and_then(|a| a.probe_daq()).map(|pd| pd.generation()).unwrap_or(0);
    if c.daq.probe.generation.is_some_and(|g| g != gen) && !c.daq.probe.owners.is_empty() {
        // Reconnected: the probe may have dropped its list. Configure at once, no debounce.
        c.daq.probe.dirty_at = Some(now);
    }
    c.daq.probe.generation = Some(gen);

    if c.daq.probe.dirty_at.is_some_and(|t| now >= t) {
        probe_configure(c, now);
    }
    if !c.daq.probe.owners.is_empty() && c.daq.probe.next_drain.is_some_and(|t| now >= t) {
        probe_drain(c, now, stamp);
    }
    probe_deliver_held(c, now, stamp);
    probe_demote_silent(c, now);
    c.daq.reseat();
}

/// Puts every eligible point on the probe and the rest on the host, and configures the probe.
fn probe_configure(c: &mut Ctx, now: Instant) {
    let Some(pd) = c.adapter.as_deref_mut().and_then(|a| a.probe_daq()) else { return };
    let reg = &mut c.daq;
    let limits = pd.limits();
    let mut items: Vec<crate::adapter::ProbeItem> = Vec::new();
    let mut owners: Vec<(SessionId, PointId)> = Vec::new();
    let mut fastest = u32::MAX;
    for s in &mut reg.sessions {
        if s.closing || s.paused {
            for p in &mut s.points {
                p.probe = None;
            }
            continue;
        }
        let sid = s.id;
        for p in &mut s.points {
            let pieces = split_items(p.addr, p.len, limits.max_item_size);
            let eligible = !p.muted
                && p.space == Space::Host
                && p.len <= MAX_PROBE_POINT_LEN
                && !reg.probe.demoted.contains(&(sid, p.id))
                && items.len() + pieces.len() <= limits.max_items;
            if !eligible {
                if matches!(p.source, Source::Probe | Source::Pending) || (p.probe_only && p.source != Source::Refused) {
                    p.leave_probe(now);
                }
                continue;
            }
            let period = p.period.as_micros().min(u32::MAX as u128) as u32;
            let check = pd.check_period_us(period);
            fastest = fastest.min(check);
            let wait = PROBE_FIRST_SAMPLE.max(Duration::from_micros(3 * check as u64));
            let mut slot = ProbeSlot { items: Vec::with_capacity(pieces.len()), buf: vec![0; p.len as usize], seen: vec![false; pieces.len()], first_by: now + wait };
            for (off, size) in pieces {
                slot.items.push((items.len(), off));
                items.push(crate::adapter::ProbeItem { addr: p.addr.wrapping_add(off), size, period_us: period });
                owners.push((sid, p.id));
            }
            if p.source != Source::Probe {
                // The host's change baseline stays: the probe's first values are delivered only
                // if they differ from what was delivered last.
                p.source = Source::Probe;
            }
            p.probe = Some(slot);
        }
    }
    reg.probe.dirty_at = None;
    if items.is_empty() && reg.probe.owners.is_empty() {
        return;
    }
    match pd.configure(&items) {
        Ok(()) => {
            crate::log_debug!("DAQ: {} probe item(s) for {} point(s)", items.len(), owners.iter().map(|o| (o.0, o.1)).collect::<std::collections::BTreeSet<_>>().len());
            reg.probe.owners = owners;
            reg.probe.drain_every = if fastest <= 1_000 { PROBE_DRAIN_FAST } else { PROBE_DRAIN_SLOW };
            reg.probe.next_drain = (!reg.probe.owners.is_empty()).then(|| now + reg.probe.drain_every);
        }
        Err(e) => {
            crate::log_debug!("DAQ: configuring the probe failed: {e}");
            probe_off(reg, now);
            reg.probe.dirty_at = Some(now + RETRY_BACKOFF);
        }
    }
}

/// The probe holds no items any more: its points go back to the host until it is configured again.
fn probe_off(reg: &mut Registry, now: Instant) {
    for s in &mut reg.sessions {
        for p in &mut s.points {
            if p.source == Source::Probe {
                p.leave_probe(now);
                if p.probe_only {
                    p.source = Source::Pending;
                }
            }
        }
    }
    reg.probe.owners.clear();
    reg.probe.next_drain = None;
}

/// Takes the probe's buffer and delivers what changed.
fn probe_drain(c: &mut Ctx, now: Instant, stamp: Option<Instant>) {
    let Some(pd) = c.adapter.as_deref_mut().and_then(|a| a.probe_daq()) else { return };
    let reg = &mut c.daq;
    let d = match pd.drain() {
        Ok(d) => d,
        Err(e) => {
            crate::log_debug!("DAQ: draining the probe failed: {e}");
            for s in &mut reg.sessions {
                if s.points.iter().any(|p| p.source == Source::Probe) {
                    s.set_state(DaqState::Unavailable, now);
                }
            }
            probe_off(reg, now);
            reg.probe.dirty_at = Some(now + RETRY_BACKOFF);
            return;
        }
    };
    let at = stamp.unwrap_or_else(Instant::now);
    if d.overflow && !reg.probe.overflow_logged {
        reg.probe.overflow_logged = true;
        crate::log_warn!("DAQ: the probe's buffer overflowed; values were lost");
    }
    reg.probe.next_drain = Some(if d.more { now } else { now + reg.probe.drain_every });
    // Per session: the deliveries, by the instant the probe read them.
    let mut out: Vec<std::collections::BTreeMap<Instant, Vec<Sample>>> = (0..reg.sessions.len()).map(|_| Default::default()).collect();
    for smp in d.samples {
        let Some(&(sid, pid)) = reg.probe.owners.get(smp.item) else { continue };
        let Some(si) = reg.sessions.iter().position(|s| s.id == sid) else { continue };
        let Some(p) = reg.sessions[si].points.iter_mut().find(|p| p.id == pid) else { continue };
        let Some(slot) = p.probe.as_mut() else { continue };
        let Some(k) = slot.items.iter().position(|&(i, _)| i == smp.item) else { continue };
        let off = slot.items[k].1 as usize;
        let n = smp.data.len().min(slot.buf.len().saturating_sub(off));
        slot.buf[off..off + n].copy_from_slice(&smp.data[..n]);
        slot.seen[k] = true;
        if !slot.complete() || p.muted {
            continue;
        }
        p.fails = 0;
        if p.mode == SampleMode::OnChange {
            let new = slot.buf.clone();
            if changed(p.last.as_ref(), &new, p.cmp_mask.as_ref()) {
                let t = probe_instant(at, d.time_us, smp.time_us);
                p.last = Some(new.clone());
                p.last_sent = t;
                out[si].entry(t).or_default().push((p.id, new));
            }
        }
    }
    for (si, by_time) in out.into_iter().enumerate() {
        let s = &mut reg.sessions[si];
        if s.points.iter().any(|p| p.source == Source::Probe) {
            s.set_state(DaqState::Running, at);
        }
        for (t, values) in by_time {
            s.deliver(values, t);
        }
    }
}

/// `EveryTick` probe points (and resends) at their period, with the value the probe last sent.
fn probe_deliver_held(c: &mut Ctx, now: Instant, stamp: Option<Instant>) {
    let at = stamp.unwrap_or(now);
    let reg = &mut c.daq;
    for s in &mut reg.sessions {
        if s.paused || s.closing {
            continue;
        }
        let mut values = Vec::new();
        let mut missed = 0;
        for p in &mut s.points {
            if p.source != Source::Probe || p.muted || !p.ticks() || now < p.next_due {
                continue;
            }
            missed += advance(p, now);
            let Some(v) = p.probe_value().map(|v| v.to_vec()) else { continue };
            let resend = p.resend_after.is_some_and(|d| at.saturating_duration_since(p.last_sent) >= d);
            if p.mode == SampleMode::EveryTick || resend {
                p.last = Some(v.clone());
                p.last_sent = at;
                values.push((p.id, v));
            }
        }
        s.missed += missed;
        if !values.is_empty() {
            s.deliver(values, at);
        }
    }
}

/// A probe item that never sent its first value cannot be read by the probe (it says nothing
/// else): the point goes to the host, whose read reports why, and the probe is configured without
/// it - a failing item slows every pass of the probe's loop.
fn probe_demote_silent(c: &mut Ctx, now: Instant) {
    let reg = &mut c.daq;
    let mut any = false;
    for s in &mut reg.sessions {
        let sid = s.id;
        for p in &mut s.points {
            let late = p.source == Source::Probe && p.probe.as_ref().is_some_and(|sl| !sl.complete() && now >= sl.first_by);
            if late {
                crate::log_debug!("DAQ: the probe never answered for 0x{:08x} ({} B); reading it from the host", p.addr, p.len);
                reg.probe.demoted.push((sid, p.id));
                p.leave_probe(now);
                any = true;
            }
        }
    }
    if any {
        reg.probe.dirty_at = Some(now);
    }
}

fn tick_inner(c: &mut Ctx, now: Instant, stamp: Option<Instant>) {
    probe_step(c, now, stamp);
    let mut b = Batch::new();
    let Some(plan) = queue_due(c, now, &mut b) else { return };
    let exec = if b.is_empty() {
        Ok(Vec::new())
    } else {
        match c.adapter.as_deref_mut() {
            Some(a) => a.execute(&b),
            None => Err(Error::Usage("the debug adapter disappeared".into())),
        }
    };
    match exec {
        Ok(results) => apply(c, plan, now, &results, stamp),
        Err(e) => {
            // Connection-level failure: back off instead of retrying at the point rate.
            crate::log_debug!("DAQ batch failed: {e}");
            batch_failed(c, now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(period_ms: u64, t0: Instant) -> Point {
        Point {
            id: 1,
            addr: 0,
            len: 4,
            space: Space::Host,
            period: Duration::from_millis(period_ms),
            next_due: t0,
            eff: Duration::from_millis(period_ms),
            achieved: None,
            first_tick: None,
            ticks: 0,
            last_tick: None,
            mode: SampleMode::EveryTick,
            cmp_mask: None,
            resend_after: None,
            fails: 0,
            muted: false,
            last: None,
            last_sent: t0,
            source: Source::Polled,
            probe_only: false,
            probe: None,
        }
    }

    #[test]
    fn items_are_naturally_aligned_and_at_most_the_probes_size() {
        assert_eq!(split_items(0x7000_0000, 4, 8), vec![(0, 4)]);
        assert_eq!(split_items(0x7000_0000, 16, 8), vec![(0, 8), (8, 8)]);
        assert_eq!(split_items(0x7000_0004, 12, 8), vec![(0, 4), (4, 8)]);
        assert_eq!(split_items(0x7000_0001, 7, 8), vec![(0, 1), (1, 2), (3, 4)]);
        assert_eq!(split_items(0x7000_0000, 3, 8), vec![(0, 2), (2, 1)]);
        assert_eq!(split_items(0x7000_0000, 16, 4), vec![(0, 4), (4, 4), (8, 4), (12, 4)]);
    }

    #[test]
    fn a_probe_timestamp_maps_back_from_the_drain() {
        let at = Instant::now();
        assert_eq!(probe_instant(at, 1_000_500, 1_000_000), at - Duration::from_micros(500));
        // A sample stamped after the drain's clock (it cannot be) lands at the drain.
        assert_eq!(probe_instant(at, 1_000, 2_000), at);
    }

    #[test]
    fn quantize_rounds_up_to_the_grid() {
        assert_eq!(quantize(0), MIN_INTERVAL_US);
        assert_eq!(quantize(1), MIN_INTERVAL_US);
        assert_eq!(quantize(1000), 1000);
        assert_eq!(quantize(1001), 1100);
        assert_eq!(quantize(9999), 10000);
        // The grid is what a requested rate is really rounded to. It is 100 us rather than 1 ms
        // so that ordinary rates come out exact instead of being quietly pulled down: 400 Hz is
        // 2500 us, which on a 1 ms grid would have become 3000 us, i.e. 333 Hz.
        assert_eq!(quantize(1_000_000 / 400), 2500);
        assert_eq!(quantize(1_000_000 / 3), 333_400);
    }

    #[test]
    fn change_detection_honours_the_mask() {
        let prev = vec![0x00u8, 0x0f];
        assert!(changed(None, &[0, 0], None));
        assert!(!changed(Some(&prev), &[0x00, 0x0f], None));
        assert!(changed(Some(&prev), &[0x00, 0x1f], None));
        // Only the low nibble of byte 1 belongs to the field: a change anywhere else is ignored.
        let mask = vec![0x00u8, 0x0f];
        assert!(!changed(Some(&prev), &[0xff, 0xff], Some(&mask)));
        assert!(changed(Some(&prev), &[0x00, 0x0e], Some(&mask)));
    }

    #[test]
    fn advance_skips_instead_of_replaying() {
        let t0 = Instant::now();
        let mut p = point(1, t0);
        assert_eq!(advance(&mut p, t0), 0);
        assert_eq!(p.next_due, t0 + Duration::from_millis(1));
        // A 100 ms stall reports the gap once and lands in the future, not 100 samples behind.
        let missed = advance(&mut p, t0 + Duration::from_millis(100));
        assert!(missed >= 98, "missed {missed}");
        assert!(p.next_due > t0 + Duration::from_millis(100));
    }

    /// The hardware symptom, pinned: a 1 ms point that gets three samples ~0.35 ms apart and then
    /// a 15.6 ms hole. `achieved` leaves the holes out (they are stalls to the controller), so it
    /// looked healthy; `delivered` is what a client actually receives.
    #[test]
    fn delivered_counts_the_holes_achieved_leaves_out() {
        let t0 = Instant::now();
        let mut p = point(1, t0);
        assert_eq!(p.delivered_us(), None, "no interval before the second tick");
        let mut t = t0;
        for _ in 0..50 {
            for _ in 0..3 {
                advance(&mut p, t);
                t += Duration::from_micros(350);
            }
            t += Duration::from_micros(15_600 - 3 * 350);
        }
        let delivered = p.delivered_us().unwrap();
        assert!((5_000..5_400).contains(&delivered), "delivered every {delivered} us");
        let achieved = p.achieved_us().unwrap();
        assert!(achieved < 1_000, "achieved {achieved} us only sees the bursts");
    }

    #[test]
    fn an_idle_registry_has_no_deadline() {
        let r = Registry::default();
        assert!(r.next_due().is_none());
        assert!(r.is_empty());
    }

    #[test]
    fn open_add_and_close_maintain_the_deadline() {
        let t0 = Instant::now();
        let mut r = Registry::default();
        let (id, rx) = r.open(SessionCfg::default(), t0).unwrap();
        assert!(r.next_due().is_none(), "a session with no points schedules nothing");

        let got = r.add_points(id, &[PointCfg::new(0x7000_0000, 4, Space::Host, 1000)], t0).unwrap();
        assert_eq!(got, vec![(1, 1000)]);
        assert_eq!(r.next_due(), Some(t0));

        // A period below the grid is rounded up, and the granted value is reported back.
        let got = r.add_points(id, &[PointCfg::new(0x7000_0010, 4, Space::Host, 1)], t0).unwrap();
        assert_eq!(got, vec![(2, MIN_INTERVAL_US)]);

        r.remove_points(id, &[1, 2]).unwrap();
        assert!(r.next_due().is_none());

        assert!(r.close(id));
        assert!(r.is_empty());
        drop(rx);
    }

    #[test]
    fn a_point_longer_than_the_block_size_is_rejected() {
        let t0 = Instant::now();
        let mut r = Registry::default();
        let (id, _rx) = r.open(SessionCfg::default(), t0).unwrap();
        assert!(r.add_points(id, &[PointCfg::new(0, MAX_POINT_LEN + 1, Space::Host, 1000)], t0).is_err());
        assert!(r.add_points(id, &[PointCfg::new(0, 0, Space::Host, 1000)], t0).is_err());
        // A mask must cover exactly the point.
        let bad = PointCfg { cmp_mask: Some(vec![0xff; 2]), ..PointCfg::new(0, 4, Space::Host, 1000) };
        assert!(r.add_points(id, &[bad], t0).is_err());
    }

    #[test]
    fn a_dropped_consumer_reaps_the_session() {
        let t0 = Instant::now();
        let mut r = Registry::default();
        let (id, rx) = r.open(SessionCfg::default(), t0).unwrap();
        r.add_points(id, &[PointCfg::new(0x7000_0000, 4, Space::Host, 1000)], t0).unwrap();
        drop(rx);
        // The send fails with Disconnected, which marks the session and reseat() removes it.
        r.session_mut(id).unwrap().send(DaqMsg::State { state: DaqState::Running, t_us: 0 });
        r.reseat();
        assert!(r.is_empty());
        assert!(r.next_due().is_none());
    }
}
