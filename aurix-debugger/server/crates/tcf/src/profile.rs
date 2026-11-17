//! Task profiling: what ran on each core, over time.
//!
//! Each lane is one OS variable of the ORTI file (`RUNNINGTASK` per core, and the `RUNNINGISR*`
//! attributes where they are plain addresses), with its own source:
//!
//! - **sampled** at a period: the variable is read every period and a change noted. A switch
//!   shorter than the period is missed entirely, and one that straddles a sample is attributed to
//!   whichever task the sample caught - a statistical profile, and every consumer is told so.
//!   The sampler's on-change mode keeps it cheap: read every period, delivered only on a change.
//! - **traced**: every write recorded by the miniMCDS data trace, with the target's time stamp.
//!   Exact, but one data trace (CPU0's stores, one address range) at a time; a lane that cannot
//!   be traced (another core's variable, outside the range) is sampled at its period instead and
//!   says why.
//! - **off**.
//!
//! The agent accumulates here rather than streaming every switch to the UI: a frontend polling a
//! few times a second then gets the transitions since it last asked, plus occupancy totals it
//! would otherwise have to integrate itself.
//!
//! Traced times are target times (nanoseconds since the trace started); they are put on the
//! session's host time base with the smallest host-minus-target offset seen, which removes the
//! drain latency without letting the clocks drift apart over a session.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use aurix_core::daq::{self, DaqMsg, PointId, SampleMode, SessionCfg, SessionId, Source, Space};
use orti::{Attribute, Awareness};

use crate::Shared;

/// Transitions kept before the oldest are dropped. At a 1 ms switch rate this is about a minute
/// per lane, which is far more than any timeline shows.
pub const MAX_TRANSITIONS: usize = 32_768;

/// One observed change of a lane's value.
#[derive(Clone, Debug)]
pub struct Transition {
    /// Microseconds since the profiling session started.
    pub t_us: u64,
    pub lane: u32,
    pub core: u32,
    pub task: String,
}

/// Time one task occupied one lane.
#[derive(Clone, Debug)]
pub struct Occupancy {
    pub lane: u32,
    pub core: u32,
    pub task: String,
    pub us: u64,
}

/// How one lane is recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneMode {
    Off,
    Sample(u32),
    Trace,
}

/// One lane: an OS variable of one core.
#[derive(Clone, Debug)]
pub struct Lane {
    pub idx: u32,
    pub core: u32,
    pub attr: String,
    pub mode: LaneMode,
    /// The period used while the lane is read (sampled, or traced but refused).
    pub period_us: u32,
    pub point: Option<PointId>,
    /// "trace", "host" or "off".
    pub source: &'static str,
    pub reason: Option<String>,
}

#[derive(Default)]
struct State {
    transitions: VecDeque<Transition>,
    /// Oldest transition index still held, so a poller can tell it fell behind.
    dropped_transitions: u64,
    totals: Vec<Occupancy>,
    /// Per lane: the task running since `since_us`.
    current: Vec<Option<(String, u64)>>,
    last_t_us: u64,
    /// Samples the consumer could not keep up with, and ticks the core thread missed.
    dropped: u64,
    missed: u64,
    /// Data trace messages and paragraphs lost.
    trace_lost: u64,
    error: Option<String>,
    lanes: Vec<Lane>,
    /// Host time minus target time, smallest seen (µs).
    trace_offset_us: Option<i64>,
}

impl State {
    /// Books the time one lane spent in the task it was in, then records the new one.
    fn switch(&mut self, lane: u32, core: u32, task: String, t_us: u64) {
        let i = lane as usize;
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
            match self.totals.iter_mut().find(|o| o.lane == lane && o.task == prev) {
                Some(o) => o.us += us,
                None => self.totals.push(Occupancy { lane, core, task: prev, us }),
            }
        }
        self.current[i] = Some((task.clone(), t_us));
        if self.transitions.len() >= MAX_TRANSITIONS {
            self.transitions.pop_front();
            self.dropped_transitions += 1;
        }
        self.transitions.push_back(Transition { t_us, lane, core, task });
        self.last_t_us = self.last_t_us.max(t_us);
    }
}

/// A running profiling session.
pub struct Profile {
    pub session: SessionId,
    /// The default period (lanes without one of their own).
    pub period_us: u32,
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

    /// Occupancy so far, including the time each lane has spent in its current task.
    pub fn totals(&self, now_us: u64) -> Vec<Occupancy> {
        let s = self.state.lock().unwrap();
        let mut out = s.totals.clone();
        for (i, cur) in s.current.iter().enumerate() {
            let Some((task, since)) = cur else { continue };
            let us = now_us.saturating_sub(*since);
            let core = s.lanes.get(i).map_or(i as u32, |l| l.core);
            match out.iter_mut().find(|o| o.lane == i as u32 && o.task == *task) {
                Some(o) => o.us += us,
                None => out.push(Occupancy { lane: i as u32, core, task: task.clone(), us }),
            }
        }
        out
    }

    pub fn elapsed_us(&self) -> u64 {
        self.started.elapsed().as_micros() as u64
    }

    pub fn stats(&self) -> (u64, u64, u64, Option<String>) {
        let s = self.state.lock().unwrap();
        (s.dropped, s.missed, s.trace_lost, s.error.clone())
    }

    pub fn lanes(&self) -> Vec<Lane> {
        self.state.lock().unwrap().lanes.clone()
    }

    pub fn point_count(&self) -> usize {
        self.state.lock().unwrap().lanes.iter().filter(|l| l.point.is_some()).count()
    }

    /// Stops sampling now, rather than at the collector thread's next wake.
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

/// A possible lane: core, attribute name, the attribute or why it cannot be recorded.
type Candidate = (u32, String, Result<Attribute, String>);

/// The OS variables of the ORTI file that can be lanes: per core `RUNNINGTASK`, and the
/// `RUNNINGISR*` attributes. Each with its attribute, or the reason it cannot be recorded.
fn discover(aw: &Awareness) -> Result<Vec<Candidate>, String> {
    let os = aw.objects_of("OS").next().ok_or("the ORTI file has no OS object")?;
    let mut out = Vec::new();
    for core in 0..aw.orti.core_count() {
        let mine = |a: &&Attribute| a.index == Some(core) || (a.index.is_none() && core == 0);
        let task = os.attrs.iter().filter(mine).find(|a| a.name == "RUNNINGTASK");
        match task {
            Some(a) => out.push((core, a.name.clone(), Ok(a.clone()))),
            None => out.push((core, "RUNNINGTASK".into(), Err(format!("the ORTI file has no RUNNINGTASK for core {core}")))),
        }
        for a in os.attrs.iter().filter(mine).filter(|a| a.name.contains("RUNNINGISR")) {
            out.push((core, a.name.clone(), Ok(a.clone())));
        }
    }
    if out.is_empty() {
        return Err("the ORTI file describes no cores".into());
    }
    Ok(out)
}

/// Lane settings from the frontend: `[{"Core":0,"Attr":"RUNNINGTASK","Mode":"trace"|"off"|<period µs>}]`.
/// Lanes without an entry: `RUNNINGTASK` sampled at `period_us`, the ISR lanes off.
pub fn parse_lanes(v: Option<&Value>) -> Result<Vec<(u32, String, LaneMode)>, String> {
    let Some(Value::Array(a)) = v else { return Ok(Vec::new()) };
    let mut out = Vec::new();
    for e in a {
        let core = e.get("Core").and_then(Value::as_u64).ok_or("each lane needs Core")? as u32;
        let attr = e.get("Attr").and_then(Value::as_str).unwrap_or("RUNNINGTASK").to_string();
        let mode = match e.get("Mode") {
            Some(Value::String(s)) if s == "trace" => LaneMode::Trace,
            Some(Value::String(s)) if s == "off" => LaneMode::Off,
            Some(Value::Number(n)) => LaneMode::Sample(n.as_u64().unwrap_or(1000) as u32),
            Some(other) => return Err(format!("lane Mode must be \"trace\", \"off\" or a period in µs, not {other}")),
            None => LaneMode::Sample(0),
        };
        out.push((core, attr, mode));
    }
    Ok(out)
}

/// Starts profiling the lanes of the ORTI file's OS object.
///
/// A lane whose variable is not a single address cannot be recorded and says so; the profile
/// fails only if no lane can be recorded at all.
pub fn start(sh: &Shared, period_us: u32, settings: &[(u32, String, LaneMode)]) -> Result<Arc<Profile>, String> {
    let aw: Arc<Awareness> = sh.orti.lock().unwrap().clone().ok_or("no ORTI file loaded")?;
    let period_us = period_us.max(daq::MIN_INTERVAL_US);
    let found = discover(&aw)?;

    let mut lanes = Vec::new();
    let mut cfgs = Vec::new();
    let mut attrs: Vec<(u32, Attribute)> = Vec::new();
    for (i, (core, attr, a)) in found.into_iter().enumerate() {
        let set = settings.iter().find(|(c, n, _)| *c == core && *n == attr).map(|x| x.2);
        let mode = match set {
            Some(LaneMode::Sample(0)) | None if attr == "RUNNINGTASK" => LaneMode::Sample(period_us),
            None => LaneMode::Off,
            Some(LaneMode::Sample(0)) => LaneMode::Sample(period_us),
            Some(m) => m,
        };
        let lane_period = match mode {
            LaneMode::Sample(p) => p.max(daq::MIN_INTERVAL_US),
            _ => period_us,
        };
        // The data trace sees the stores of the observed core only (CPU0): another core's OS
        // variable would be traced incompletely, so it is sampled instead.
        let refused = (mode == LaneMode::Trace && core != 0).then(|| format!("only CPU0's writes can be traced; core {core} is sampled"));
        let traced = mode == LaneMode::Trace && refused.is_none();
        let mut lane = Lane { idx: i as u32, core, attr: attr.clone(), mode, period_us: lane_period, point: None, source: "off", reason: refused };
        match (&a, mode) {
            (_, LaneMode::Off) => {}
            (Err(e), _) => lane.reason = Some(e.clone()),
            (Ok(at), _) => match at.plan {
                orti::Plan::Direct { addr, size, .. } => {
                    cfgs.push((
                        i,
                        daq::PointCfg {
                            addr,
                            len: size,
                            space: Space::Host,
                            interval_us: lane_period,
                            // Read every period, deliver only the switches.
                            mode: SampleMode::OnChange,
                            cmp_mask: None,
                            resend_after_us: 0,
                            probe_only: false,
                            trace: traced,
                        },
                    ));
                    attrs.push((i as u32, at.clone()));
                    lane.source = "host";
                }
                _ => lane.reason = Some(format!("{attr} of core {core} is not a single address, so it cannot be recorded")),
            },
        }
        lanes.push(lane);
    }
    if cfgs.is_empty() {
        let why = lanes.iter().find_map(|l| l.reason.clone()).unwrap_or_else(|| "every lane is off".into());
        return Err(why);
    }

    let opened = sh.handle.call(move |cx| cx.with(|c| c.daq.open(SessionCfg::default(), Instant::now())));
    let (session, rx) = match opened {
        Some(Ok(r)) => r,
        Some(Err(e)) => return Err(e.to_string()),
        None => return Err("the debug server stopped".into()),
    };
    let only: Vec<daq::PointCfg> = cfgs.iter().map(|(_, c)| c.clone()).collect();
    let added = sh
        .handle
        .call(move |cx| cx.with(|c| c.daq.add_points(session, &only, Instant::now())))
        .ok_or("the debug server stopped")?
        .map_err(|e| e.to_string())?;
    for ((lane_idx, _), (pid, _)) in cfgs.iter().zip(added.iter()) {
        lanes[*lane_idx].point = Some(*pid);
    }

    let by_point: Vec<(PointId, u32, u32, Attribute)> = lanes
        .iter()
        .filter_map(|l| Some((l.point?, l.idx, l.core, attrs.iter().find(|(x, _)| *x == l.idx)?.1.clone())))
        .collect();
    let state = Arc::new(Mutex::new(State { lanes, ..Default::default() }));
    let stop = Arc::new(AtomicBool::new(false));
    let profile = Arc::new(Profile { session, period_us, state: state.clone(), stop: stop.clone(), started: Instant::now() });

    // Decoding runs here, off the core thread: it walks an enumerator table per sample, and the
    // core thread has a target to service.
    let handle = sh.handle.clone();
    thread::Builder::new()
        .name(format!("orti-profile-{session}"))
        .spawn(move || collect(rx, by_point, state, stop, handle, session))
        .map_err(|e| e.to_string())?;
    Ok(profile)
}

/// Consumes the sample stream until the profile is dropped or the channel closes.
fn collect(
    rx: Receiver<DaqMsg>,
    points: Vec<(PointId, u32, u32, Attribute)>,
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
                    let Some((_, lane, core, attr)) = points.iter().find(|p| p.0 == id) else { continue };
                    s.switch(*lane, *core, attr.show(&bytes).text, t_us);
                }
            }
            Ok(DaqMsg::Trace { t_us, values }) => {
                let mut s = state.lock().unwrap();
                for (id, t_ns, bytes) in values {
                    let Some((_, lane, core, attr)) = points.iter().find(|p| p.0 == id) else { continue };
                    let t = match t_ns {
                        Some(ns) => {
                            let target_us = (ns / 1000) as i64;
                            let off = s.trace_offset_us.map_or(t_us as i64 - target_us, |o| o.min(t_us as i64 - target_us));
                            s.trace_offset_us = Some(off);
                            (target_us + off).max(0) as u64
                        }
                        None => t_us,
                    };
                    s.switch(*lane, *core, attr.show(&bytes).text, t);
                }
            }
            Ok(DaqMsg::TraceLost { messages, laps, .. }) => state.lock().unwrap().trace_lost += messages + laps,
            Ok(DaqMsg::SourceChanged { point, source, reason }) => {
                let mut s = state.lock().unwrap();
                if let Some(l) = s.lanes.iter_mut().find(|l| l.point == Some(point)) {
                    l.source = if source == Source::Trace { "trace" } else { "host" };
                    l.reason = reason;
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

/// The lanes the ORTI file offers, for a frontend to show before profiling starts.
pub fn lanes_cmd(sh: &Shared) -> Result<Value, String> {
    let aw: Arc<Awareness> = sh.orti.lock().unwrap().clone().ok_or("no ORTI file loaded")?;
    let found = discover(&aw)?;
    Ok(Value::Array(
        found
            .iter()
            .map(|(core, attr, a)| {
                let reason = match a {
                    Err(e) => Some(e.clone()),
                    Ok(at) if !matches!(at.plan, orti::Plan::Direct { .. }) => Some(format!("{attr} is not a single address")),
                    Ok(_) => None,
                };
                json!({ "Core": core, "Attr": attr, "Reason": reason })
            })
            .collect(),
    ))
}

/// Starts profiling, replacing any session already running.
pub fn start_cmd(sh: &Shared, period_us: u32, lanes: Option<&Value>) -> Result<Value, String> {
    let settings = parse_lanes(lanes)?;
    // Stopping first keeps two samplers off the same points, which would only halve the rate each
    // of them achieves - and frees the data trace for this profile.
    if let Some(old) = sh.profile.lock().unwrap().take() {
        old.shutdown(sh);
    }
    let p = start(sh, period_us, &settings)?;
    let out = json!({ "PeriodUs": p.period_us, "Points": p.point_count(), "Cores": p.point_count(), "Lanes": lanes_json(&p.lanes()) });
    *sh.profile.lock().unwrap() = Some(p);
    Ok(out)
}

fn lanes_json(lanes: &[Lane]) -> Value {
    Value::Array(
        lanes
            .iter()
            .map(|l| {
                json!({
                    "Lane": l.idx, "Core": l.core, "Attr": l.attr,
                    "Mode": match l.mode { LaneMode::Off => json!("off"), LaneMode::Trace => json!("trace"), LaneMode::Sample(p) => json!(p) },
                    "PeriodUs": l.period_us, "Source": l.source, "Reason": l.reason,
                })
            })
            .collect(),
    )
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
    let (dropped, missed, trace_lost, error) = p.stats();
    let mut m = Map::new();
    m.insert("Running".into(), json!(true));
    m.insert("PeriodUs".into(), json!(p.period_us));
    m.insert("NowUs".into(), json!(now_us));
    m.insert("Lanes".into(), lanes_json(&p.lanes()));
    m.insert(
        "Transitions".into(),
        Value::Array(transitions.iter().map(|t| json!({ "T": t.t_us, "Lane": t.lane, "Core": t.core, "Task": t.task })).collect()),
    );
    m.insert(
        "Totals".into(),
        Value::Array(p.totals(now_us).iter().map(|o| json!({ "Lane": o.lane, "Core": o.core, "Task": o.task, "Us": o.us })).collect()),
    );
    m.insert("DroppedTransitions".into(), json!(dropped_transitions));
    m.insert("Dropped".into(), json!(dropped));
    m.insert("Missed".into(), json!(missed));
    m.insert("TraceLost".into(), json!(trace_lost));
    if let Some(e) = error {
        m.insert("Error".into(), json!(e));
    }
    Value::Object(m)
}
