//! Traced DAQ points: the miniMCDS data trace records every write to them.
//!
//! One data trace, one address range, one session: the first session with traced points owns
//! it, its points within [`TRACE_MAX_SPAN`] of the lowest one are traced, everything else is read
//! at its period with a reason ([`Point::trace_reason`]). Traced points are seeded with a read
//! when the trace is set up; afterwards each recorded write updates the bytes it covers and the
//! point is delivered with the write's target time ([`DaqMsg::Trace`]).
//!
//! The trace runs as a ring with TSU time stamps only along with messages (no ticks, which
//! would fill the 8 KB in milliseconds) and is drained every [`TRACE_POLL`].

use std::time::{Duration, Instant};

use super::{DaqMsg, PointId, SessionId, Source, Space, RETRY_BACKOFF};
use crate::ctx::Ctx;
use crate::mcds_trace::{self as mt, Owner};

/// How often a running data trace is drained.
pub const TRACE_POLL: Duration = Duration::from_millis(2);
/// Points subscribed together are traced together: the trace is set up this long after a change.
pub const TRACE_DEBOUNCE: Duration = Duration::from_millis(50);
/// Largest address span one data trace covers (one DTU comparator range).
pub const TRACE_MAX_SPAN: u32 = 0x1000;
/// The unit writes the trace RAM in 32-byte words: when nothing new arrived for this long, the
/// last word is pushed out ([`mt::flush`]), so the last write of a quiet variable is delivered.
pub const TRACE_FLUSH_IDLE: Duration = Duration::from_millis(100);

/// The registry's side of the data trace.
#[derive(Default)]
pub(super) struct TraceDaq {
    pub owner: Option<SessionId>,
    pub range: Option<(u32, u32)>,
    pub armed: bool,
    pub dirty_at: Option<Instant>,
    pub next_poll: Option<Instant>,
    drain: mcds::buffer::Drain,
    dec: mcds::Decoder,
    /// The write pointer at the last drain, since when it stands still, whether the last word was
    /// flushed since.
    last_wp: Option<u32>,
    quiet_since: Option<Instant>,
    flushed: bool,
}

/// The data trace's part of a tick: set it up after a change, drain it while it runs.
pub(super) fn step(c: &mut Ctx, now: Instant) {
    if c.daq.trace.dirty_at.is_some_and(|d| now >= d) {
        c.daq.trace.dirty_at = None;
        configure(c, now);
    }
    if c.daq.trace.armed && c.daq.trace.next_poll.is_none_or(|t| now >= t) {
        c.daq.trace.next_poll = Some(now + TRACE_POLL);
        drain(c, now);
    }
}

/// After a device reset: the MCDS is off. Read the traced points until it is set up again.
pub(super) fn on_reset(c: &mut Ctx, retry: Instant) {
    if !(c.daq.trace.armed || c.daq.trace.owner.is_some()) {
        return;
    }
    c.daq.trace.armed = false;
    c.daq.trace.range = None;
    for s in &mut c.daq.sessions {
        for p in &mut s.points {
            if p.source == Source::Trace {
                p.source = Source::Polled;
                p.next_due = p.next_due.max(retry);
            }
        }
    }
    c.daq.trace.dirty_at = Some(retry + TRACE_DEBOUNCE);
}

/// Points that want the trace: (session, point, addr, len), not paused, not muted, bus points.
fn candidates(c: &Ctx) -> Vec<(SessionId, PointId, u32, u32)> {
    c.daq
        .sessions
        .iter()
        .filter(|s| !s.paused && !s.closing)
        .flat_map(|s| s.points.iter().filter(|p| p.trace && !p.muted && p.space == Space::Host).map(move |p| (s.id, p.id, p.addr, p.len)))
        .collect()
}

/// Sets a point's source and reason, telling its session when they change.
fn set_source(c: &mut Ctx, sid: SessionId, pid: PointId, source: Source, reason: Option<String>, now: Instant) {
    let Some(s) = c.daq.sessions.iter_mut().find(|s| s.id == sid) else { return };
    let Some(p) = s.points.iter_mut().find(|p| p.id == pid) else { return };
    if p.source == source && p.trace_reason == reason {
        return;
    }
    if p.source == Source::Trace && source != Source::Trace {
        p.next_due = now;
    }
    p.source = source;
    p.trace_reason = reason.clone();
    s.send(DaqMsg::SourceChanged { point: pid, source, reason });
}

fn release(c: &mut Ctx) {
    if c.daq.trace.armed {
        let _ = mt::release(c, Owner::Daq);
    }
    c.daq.trace.armed = false;
    c.daq.trace.range = None;
}

/// Chooses the owner session and the range, and arms (or releases) the data trace.
fn configure(c: &mut Ctx, now: Instant) {
    let cands = candidates(c);
    let owner = c.daq.trace.owner.filter(|o| cands.iter().any(|x| x.0 == *o)).or_else(|| cands.first().map(|x| x.0));
    c.daq.trace.owner = owner;
    let Some(owner) = owner else {
        release(c);
        return;
    };
    for &(sid, pid, ..) in cands.iter().filter(|x| x.0 != owner) {
        set_source(c, sid, pid, Source::Polled, Some("another client is tracing".into()), now);
    }
    let mut mine: Vec<(PointId, u32, u32)> = cands.iter().filter(|x| x.0 == owner).map(|x| (x.1, x.2, x.3)).collect();
    if let Some(a) = c.mcds.armed.as_ref().filter(|a| a.owner != Owner::Daq) {
        let why = format!("the trace is in use ({})", a.owner.as_str());
        for &(pid, ..) in &mine {
            set_source(c, owner, pid, Source::Polled, Some(why.clone()), now);
        }
        c.daq.trace.armed = false;
        c.daq.trace.range = None;
        return;
    }
    mine.sort_by_key(|x| x.1);
    let start = mine[0].1 as u64;
    let (inside, outside): (Vec<_>, Vec<_>) = mine.into_iter().partition(|x| x.1 as u64 + x.2 as u64 - start <= TRACE_MAX_SPAN as u64);
    for &(pid, ..) in &outside {
        set_source(c, owner, pid, Source::Polled, Some(format!("outside the traced range (one range of up to {TRACE_MAX_SPAN} bytes)")), now);
    }
    let end = inside.iter().map(|x| x.1 as u64 + x.2 as u64).max().unwrap_or(start + 1);
    let range = (start as u32, (end - start) as u32);
    if !(c.daq.trace.armed && c.daq.trace.range == Some(range)) {
        let cfg = mcds::TraceConfig {
            core: 0,
            kind: mcds::Kind::Data { range: mcds::AddrRange { start: range.0, len: range.1.max(1) } },
            stop: mcds::Stop::Manual,
            timestamps: mcds::Timestamps::Messages,
        };
        release(c);
        if let Err(e) = mt::arm(c, cfg, Owner::Daq) {
            for &(pid, ..) in &inside {
                set_source(c, owner, pid, Source::Polled, Some(e.to_string()), now);
            }
            return;
        }
        c.daq.trace.armed = true;
        c.daq.trace.range = Some(range);
        c.daq.trace.drain = mcds::buffer::Drain::new();
        c.daq.trace.dec = mcds::Decoder::new();
        c.daq.trace.last_wp = None;
        c.daq.trace.quiet_since = None;
        c.daq.trace.flushed = false;
        c.daq.trace.next_poll = Some(now + TRACE_POLL);
    }
    // Seed each traced point with its current value: a write changes only the bytes it covers.
    for &(pid, addr, len) in &inside {
        let seed = c.io().and_then(|mut io| io.read(addr, len as usize)).ok();
        if let Some(p) = c.daq.sessions.iter_mut().find(|s| s.id == owner).and_then(|s| s.points.iter_mut().find(|p| p.id == pid)) {
            if seed.is_some() {
                p.last = seed;
            }
        }
        set_source(c, owner, pid, Source::Trace, None, now);
    }
}

/// Reads what the data trace wrote since the last drain and delivers the writes.
fn drain(c: &mut Ctx, now: Instant) {
    let Some(owner) = c.daq.trace.owner else { return };
    let st = match mt::status(c) {
        Ok(s) => s,
        Err(_) => {
            // Unreachable (reset, link): set up again later.
            c.daq.trace.armed = false;
            c.daq.trace.dirty_at = Some(now + RETRY_BACKOFF);
            return;
        }
    };
    // The writer standing still for a while: push the last word out (once), so the last writes
    // (and the message the drain holds back from a paragraph in progress) arrive.
    if c.daq.trace.last_wp == Some(st.write_ptr) {
        let since = *c.daq.trace.quiet_since.get_or_insert(now);
        if !c.daq.trace.flushed && now.duration_since(since) >= TRACE_FLUSH_IDLE {
            c.daq.trace.flushed = true;
            let _ = mt::flush(c);
        }
        return;
    }
    c.daq.trace.last_wp = Some(st.write_ptr);
    c.daq.trace.quiet_since = Some(now);
    c.daq.trace.flushed = false;
    let reads = c.daq.trace.drain.poll(st.write_ptr);
    if reads.is_empty() {
        return;
    }
    let Ok(chunks) = mt::read_pieces(c, &reads) else { return };
    let laps_before = c.daq.trace.drain.laps;
    let (msgs, lost) = c.daq.trace.drain.take(&chunks);
    if lost {
        c.daq.trace.dec.gap();
    }
    let laps = c.daq.trace.drain.laps - laps_before;
    let mut lost_msgs = 0u64;
    let mut writes = Vec::new();
    for m in msgs {
        match c.daq.trace.dec.feed(m) {
            Some(mcds::Event::Data(d)) if d.access == mcds::Access::Write => {
                if let (Some(a), Some(v)) = (d.addr, d.value) {
                    writes.push((a, v, d.size, d.time.map(mcds::units_to_ns)));
                }
            }
            Some(mcds::Event::Lost { count, .. }) => lost_msgs += count as u64,
            _ => {}
        }
    }
    let Some(s) = c.daq.sessions.iter_mut().find(|s| s.id == owner) else { return };
    let mut values = Vec::new();
    for (a, v, size, t) in writes {
        let bytes = v.to_le_bytes();
        for p in s.points.iter_mut().filter(|p| p.source == Source::Trace) {
            let (ps, pe) = (p.addr as u64, p.addr as u64 + p.len as u64);
            let (ws, we) = (a as u64, a as u64 + size as u64);
            if we <= ps || ws >= pe {
                continue;
            }
            let buf = p.last.get_or_insert_with(|| vec![0; p.len as usize]);
            for k in ws.max(ps)..we.min(pe) {
                buf[(k - ps) as usize] = bytes[(k - ws) as usize];
            }
            values.push((p.id, t, buf.clone()));
        }
    }
    let t_us = s.t_us(now);
    if lost_msgs > 0 || laps > 0 {
        s.send(DaqMsg::TraceLost { t_us, messages: lost_msgs, laps });
    }
    if !values.is_empty() {
        s.send(DaqMsg::Trace { t_us, values });
    }
}
