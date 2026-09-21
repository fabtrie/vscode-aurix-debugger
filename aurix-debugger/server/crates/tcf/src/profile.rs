//! Sampled task profiling: what ran on each core, over time.
//!
//! This is **not** a trace. There is no MCDS on a miniWiggler, so the only way to see task
//! activity is to read `RUNNINGTASK` often and note when it changes. A switch shorter than the
//! sampling period is missed entirely, and one that straddles a sample is attributed to whichever
//! task the sample caught - so the result is a statistical profile, and every consumer is told so.
//!
//! What makes it cheap enough to be worth having is the sampler's on-change mode: the point is
//! still *read* every period, but only *delivered* when the value differs. A 100 µs period over
//! two cores is two reads per tick - one coalesced bus access - and the stream carries only the
//! switches.
//!
//! The agent accumulates here rather than streaming every switch to the UI: a frontend polling a
//! few times a second then gets the transitions since it last asked, plus occupancy totals it
//! would otherwise have to integrate itself.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use aurix_core::daq::{self, DaqMsg, PointId, SampleMode, SessionCfg, SessionId, Space};
use orti::{Attribute, Awareness};

use crate::Shared;

/// Transitions kept before the oldest are dropped. At a 1 ms switch rate this is about a minute
/// per core, which is far more than any timeline shows.
pub const MAX_TRANSITIONS: usize = 32_768;

/// One observed change of the running task on one core.
#[derive(Clone, Debug)]
pub struct Transition {
    /// Microseconds since the profiling session started.
    pub t_us: u64,
    pub core: u32,
    pub task: String,
}

/// Time one task occupied one core.
#[derive(Clone, Debug)]
pub struct Occupancy {
    pub core: u32,
    pub task: String,
    pub us: u64,
}

#[derive(Default)]
struct State {
    transitions: VecDeque<Transition>,
    /// Oldest transition index still held, so a poller can tell it fell behind.
    dropped_transitions: u64,
    totals: Vec<Occupancy>,
    /// Per core: the task running since `since_us`.
    current: Vec<Option<(String, u64)>>,
    last_t_us: u64,
    /// Samples the consumer could not keep up with, and ticks the core thread missed.
    dropped: u64,
    missed: u64,
    error: Option<String>,
}

impl State {
    /// Books the time one core spent in the task it was in, then records the new one.
    fn switch(&mut self, core: u32, task: String, t_us: u64) {
        let i = core as usize;
        if self.current.len() <= i {
            self.current.resize(i + 1, None);
        }
        if let Some((prev, since)) = self.current[i].take() {
            if prev == task {
                // Nothing changed after all (a resend, or a baseline redelivery).
                self.current[i] = Some((prev, since));
                return;
            }
            let us = t_us.saturating_sub(since);
            match self.totals.iter_mut().find(|o| o.core == core && o.task == prev) {
                Some(o) => o.us += us,
                None => self.totals.push(Occupancy { core, task: prev, us }),
            }
        }
        self.current[i] = Some((task.clone(), t_us));
        if self.transitions.len() >= MAX_TRANSITIONS {
            self.transitions.pop_front();
            self.dropped_transitions += 1;
        }
        self.transitions.push_back(Transition { t_us, core, task });
        self.last_t_us = self.last_t_us.max(t_us);
    }
}

/// A running profiling session.
pub struct Profile {
    pub session: SessionId,
    pub period_us: u32,
    /// Which core each DAQ point belongs to.
    points: Vec<(PointId, u32)>,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    started: Instant,
}

impl Profile {
    /// Transitions after `since_us`, oldest first, and how many were dropped before them.
    pub fn since(&self, since_us: u64) -> (Vec<Transition>, u64) {
        let s = self.state.lock().unwrap();
        (s.transitions.iter().filter(|t| t.t_us > since_us).cloned().collect(), s.dropped_transitions)
    }

    /// Occupancy so far, including the time each core has spent in its current task.
    pub fn totals(&self, now_us: u64) -> Vec<Occupancy> {
        let s = self.state.lock().unwrap();
        let mut out = s.totals.clone();
        for (i, cur) in s.current.iter().enumerate() {
            let Some((task, since)) = cur else { continue };
            let us = now_us.saturating_sub(*since);
            match out.iter_mut().find(|o| o.core == i as u32 && o.task == *task) {
                Some(o) => o.us += us,
                None => out.push(Occupancy { core: i as u32, task: task.clone(), us }),
            }
        }
        out
    }

    pub fn elapsed_us(&self) -> u64 {
        self.started.elapsed().as_micros() as u64
    }

    pub fn stats(&self) -> (u64, u64, Option<String>) {
        let s = self.state.lock().unwrap();
        (s.dropped, s.missed, s.error.clone())
    }

    pub fn point_count(&self) -> usize {
        self.points.len()
    }

    /// Stops sampling now, rather than at the collector thread.s next wake.
    ///
    /// Dropping alone is not enough: the collector only notices the stop flag when its receive
    /// times out, and until then the sampler keeps reading the target - so two profiles briefly
    /// overlap on a restart, and the session cap is only eight.
    pub fn shutdown(&self, sh: &Shared) {
        self.stop.store(true, Ordering::Relaxed);
        let id = self.session;
        sh.handle.call(move |cx| cx.with(|c| c.daq.close(id)));
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Starts profiling `RUNNINGTASK` on every core the ORTI file describes.
///
/// Fails rather than degrading if an attribute is not a single address: a `RUNNINGTASK` that has
/// to be computed cannot be sampled, and quietly profiling only some cores would be worse than
/// saying so.
pub fn start(sh: &Shared, period_us: u32) -> Result<Arc<Profile>, String> {
    let aw: Arc<Awareness> = sh.orti.lock().unwrap().clone().ok_or("no ORTI file loaded")?;
    let os = aw.objects_of("OS").next().ok_or("the ORTI file has no OS object")?;
    let period_us = period_us.max(daq::MIN_INTERVAL_US);

    let mut cfgs = Vec::new();
    let mut attrs = Vec::new();
    for core in 0..aw.orti.core_count() {
        let a = os
            .attrs
            .iter()
            .find(|a| a.name == "RUNNINGTASK" && (a.index == Some(core) || (a.index.is_none() && core == 0)))
            .ok_or_else(|| format!("the ORTI file has no RUNNINGTASK for core {core}"))?;
        let orti::Plan::Direct { addr, size, .. } = a.plan else {
            return Err(format!("RUNNINGTASK for core {core} is not a single address, so it cannot be sampled"));
        };
        cfgs.push(daq::PointCfg {
            addr,
            len: size,
            space: Space::Host,
            interval_us: period_us,
            // The whole point: read every period, deliver only the switches.
            mode: SampleMode::OnChange,
            cmp_mask: None,
            resend_after_us: 0,
        });
        attrs.push((core, a.clone()));
    }
    if cfgs.is_empty() {
        return Err("the ORTI file describes no cores".into());
    }

    let opened = sh.handle.call(move |cx| cx.with(|c| c.daq.open(SessionCfg::default(), Instant::now())));
    let (session, rx) = match opened {
        Some(Ok(r)) => r,
        Some(Err(e)) => return Err(e.to_string()),
        None => return Err("the debug server stopped".into()),
    };
    let added = sh
        .handle
        .call(move |cx| cx.with(|c| c.daq.add_points(session, &cfgs, Instant::now())))
        .ok_or("the debug server stopped")?
        .map_err(|e| e.to_string())?;

    let points: Vec<(PointId, u32)> = added.iter().zip(attrs.iter()).map(|((id, _), (core, _))| (*id, *core)).collect();
    let state = Arc::new(Mutex::new(State::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let profile = Arc::new(Profile {
        session,
        period_us,
        points: points.clone(),
        state: state.clone(),
        stop: stop.clone(),
        started: Instant::now(),
    });

    // Decoding runs here, off the core thread: it walks an enumerator table per sample, and the
    // core thread has a target to service.
    let handle = sh.handle.clone();
    let by_point: Vec<(PointId, u32, Attribute)> = points.iter().map(|(id, c)| (*id, *c, attrs.iter().find(|(x, _)| x == c).unwrap().1.clone())).collect();
    thread::Builder::new()
        .name(format!("orti-profile-{session}"))
        .spawn(move || collect(rx, by_point, state, stop, handle, session))
        .map_err(|e| e.to_string())?;
    Ok(profile)
}

/// Consumes the sample stream until the profile is dropped or the channel closes.
fn collect(
    rx: Receiver<DaqMsg>,
    points: Vec<(PointId, u32, Attribute)>,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    handle: aurix_core::ServerHandle,
    session: SessionId,
) {
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(DaqMsg::Samples { t_us, values, .. }) => {
                let mut s = state.lock().unwrap();
                for (id, bytes) in values {
                    let Some((_, core, attr)) = points.iter().find(|(p, _, _)| *p == id) else { continue };
                    s.switch(*core, attr.show(&bytes).text, t_us);
                }
            }
            Ok(DaqMsg::Overrun { dropped, .. }) => state.lock().unwrap().dropped += dropped,
            Ok(DaqMsg::Gap { missed, .. }) => state.lock().unwrap().missed += missed,
            Ok(DaqMsg::PointError { err, muted, .. }) => {
                let mut s = state.lock().unwrap();
                s.error = Some(if muted { format!("{err} (stopped sampling this point)") } else { err });
            }
            Ok(DaqMsg::State { .. }) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    // Whatever ended the loop, the session must not outlive it.
    let _ = handle.call(move |cx| cx.with(|c| c.daq.close(session)));
}

// --- The TCF surface -------------------------------------------------------------------------

/// Starts profiling, replacing any session already running.
pub fn start_cmd(sh: &Shared, period_us: u32) -> Result<Value, String> {
    // Stopping first keeps two samplers off the same points, which would only halve the rate each
    // of them achieves.
    if let Some(old) = sh.profile.lock().unwrap().take() {
        old.shutdown(sh);
    }
    let p = start(sh, period_us)?;
    let out = json!({ "PeriodUs": p.period_us, "Points": p.point_count(), "Cores": p.point_count() });
    *sh.profile.lock().unwrap() = Some(p);
    Ok(out)
}

/// Stops profiling. Stopping when nothing runs is not an error: a panel closing says so blindly.
pub fn stop_cmd(sh: &Shared) -> Value {
    let old = sh.profile.lock().unwrap().take();
    if let Some(p) = &old {
        p.shutdown(sh);
    }
    json!({ "Stopped": old.is_some() })
}

/// Everything observed after `since_us`, plus the occupancy totals.
pub fn poll_cmd(sh: &Shared, since_us: u64) -> Value {
    let Some(p) = sh.profile.lock().unwrap().clone() else {
        return json!({ "Running": false });
    };
    let now_us = p.elapsed_us();
    let (transitions, dropped_transitions) = p.since(since_us);
    let (dropped, missed, error) = p.stats();
    let mut m = Map::new();
    m.insert("Running".into(), json!(true));
    m.insert("PeriodUs".into(), json!(p.period_us));
    m.insert("NowUs".into(), json!(now_us));
    m.insert(
        "Transitions".into(),
        Value::Array(transitions.iter().map(|t| json!({ "T": t.t_us, "Core": t.core, "Task": t.task })).collect()),
    );
    m.insert(
        "Totals".into(),
        Value::Array(p.totals(now_us).iter().map(|o| json!({ "Core": o.core, "Task": o.task, "Us": o.us })).collect()),
    );
    m.insert("DroppedTransitions".into(), json!(dropped_transitions));
    m.insert("Dropped".into(), json!(dropped));
    m.insert("Missed".into(), json!(missed));
    if let Some(e) = error {
        m.insert("Error".into(), json!(e));
    }
    Value::Object(m)
}
