//! The core thread: owns the interpreter and the target state, serves
//! frontend requests and polls the targets.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant};

use jim::{CmdResult, Interp};

use crate::commands;
use crate::ctx::{Ctx, Cx, Shared};
use crate::daq;
use crate::events::{self, Event};
use crate::log;
use crate::ops;

/// Work sent from frontend threads to the core thread.
pub type Job = Box<dyn for<'a, 'b> FnOnce(&'b Cx<'a>) + Send>;

/// Thread-safe handle frontends use to talk to the core.
#[derive(Clone)]
pub struct ServerHandle {
    tx: Sender<Job>,
    status: Arc<Mutex<Published>>,
}

/// What the core looks like from outside, published by the core thread so a frontend can answer
/// a liveness probe without a round trip: one that queues behind a flash job or a long Tcl script
/// would make a busy server look dead.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    /// The driver in use (`adapter name`), once one is configured.
    pub adapter: Option<String>,
    /// The adapter is connected to the probe (`init` ran successfully).
    pub connected: bool,
    pub targets: Vec<TargetStatus>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TargetStatus {
    pub name: String,
    pub coreid: u32,
    pub examined: bool,
    pub reachable: bool,
}

struct Published {
    status: Status,
    /// When the core thread last finished a job or a poll.
    seen: Instant,
}

impl ServerHandle {
    /// Runs `f` on the core thread and returns its result.
    pub fn call<R, F>(&self, f: F) -> Option<R>
    where
        R: Send + 'static,
        F: for<'a, 'b> FnOnce(&'b Cx<'a>) -> R + Send + 'static,
    {
        let (rtx, rrx) = channel();
        let job: Job = Box::new(move |cx| {
            let _ = rtx.send(f(cx));
        });
        self.tx.send(job).ok()?;
        rrx.recv().ok()
    }

    /// Evaluates a Tcl command line at global level.
    pub fn tcl(&self, script: &str) -> CmdResult {
        let s = script.to_string();
        self.call(move |cx| cx.interp.eval_global(&s)).unwrap_or_else(|| Err("server stopped".into()))
    }

    /// Like [`tcl`](Self::tcl), also returning the log output (e.g. `echo`)
    /// produced while the command ran.
    pub fn tcl_with_output(&self, script: &str) -> (CmdResult, Vec<String>) {
        let s = script.to_string();
        self.call(move |cx| log::capture(|| cx.interp.eval_global(&s)))
            .unwrap_or_else(|| (Err("server stopped".into()), Vec::new()))
    }

    pub fn subscribe(&self) -> Receiver<Event> {
        events::subscribe()
    }

    /// The last published [`Status`] and how long ago the core thread last finished a job or a
    /// poll. That age stays below the polling period while the core thread is responsive, and
    /// grows while one long operation (flashing, a reset, a Tcl script) holds it.
    pub fn status(&self) -> (Status, Duration) {
        let p = self.status.lock().unwrap();
        (p.status.clone(), p.seen.elapsed())
    }
}

/// Publishes the state behind [`ServerHandle::status`]. Compared before it is copied, because
/// this runs after every job: a changed field is rare, an allocation per job would not be.
fn publish(cx: &Cx, out: &Mutex<Published>) {
    let mut p = out.lock().unwrap();
    p.seen = Instant::now();
    let same = cx.with(|c| {
        let adapter = c.adapter.as_deref().map(|a| a.driver_name()).or(c.adapter_name.as_deref());
        let connected = c.adapter.as_deref().is_some_and(|a| a.initialized());
        let s = &p.status;
        s.adapter.as_deref() == adapter
            && s.connected == connected
            && s.targets.len() == c.targets.len()
            && s.targets.iter().zip(&c.targets).all(|(a, t)| {
                a.examined == t.core.examined() && a.reachable == !t.unreachable && a.coreid == t.core.coreid() && a.name == t.name
            })
    });
    if same {
        return;
    }
    p.status = cx.with(|c| Status {
        adapter: c.adapter.as_deref().map(|a| a.driver_name().to_string()).or_else(|| c.adapter_name.clone()),
        connected: c.adapter.as_deref().is_some_and(|a| a.initialized()),
        targets: c
            .targets
            .iter()
            .map(|t| TargetStatus { name: t.name.clone(), coreid: t.core.coreid(), examined: t.core.examined(), reachable: !t.unreachable })
            .collect(),
    });
}

pub struct Server {
    pub interp: Interp,
    pub sh: Shared,
    tx: Sender<Job>,
    rx: Receiver<Job>,
    status: Arc<Mutex<Published>>,
}

static LOG_TO_EVENTS: Once = Once::new();
static TIMER_RESOLUTION: Once = Once::new();

/// Windows schedules timers at ~15.6 ms by default, which would put a floor under both the run
/// state poll and DAQ. This belongs to the server, not to the executable: an embedder that drives
/// `Server::run` gets the same 1 ms resolution the sampler promises. The process keeps it until
/// it exits.
///
/// `timeBeginPeriod` alone is not enough on Windows 11. A process that leaves its power
/// throttling to the system - which every process does unless it says otherwise - can have its
/// timer requests ignored whenever the OS decides it is in the background, and a debug server
/// started hidden by VS Code is exactly that. The request then holds while the server is busy and
/// lapses when it goes quiet: on a TC38x, a 1 kHz point came out at ~190 samples/s, in bursts of
/// three with a 15.6 ms hole after each, while a `sleep 1` on the console still took 1.5 ms. So the
/// process opts out explicitly: its timer requests are always honoured, and it is never run as
/// EcoQoS - a sampler that promises a rate is not background work.
#[cfg(windows)]
fn request_timer_resolution() {
    #[link(name = "winmm")]
    extern "system" {
        fn timeBeginPeriod(period: u32) -> u32;
    }
    /// `PROCESS_POWER_THROTTLING_STATE`.
    #[repr(C)]
    struct PowerThrottling {
        version: u32,
        control_mask: u32,
        state_mask: u32,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> isize;
        fn SetProcessInformation(process: isize, class: i32, info: *const PowerThrottling, size: u32) -> i32;
    }
    const PROCESS_POWER_THROTTLING: i32 = 4;
    const EXECUTION_SPEED: u32 = 0x1;
    const IGNORE_TIMER_RESOLUTION: u32 = 0x4;
    TIMER_RESOLUTION.call_once(|| unsafe {
        timeBeginPeriod(1);
        // In the control mask with a zero state: "decide this for me, and the answer is never
        // throttle". Fails harmlessly before Windows 10 1709 (no such class) and only matters on
        // Windows 11, so the result is not checked.
        let state = PowerThrottling { version: 1, control_mask: EXECUTION_SPEED | IGNORE_TIMER_RESOLUTION, state_mask: 0 };
        SetProcessInformation(GetCurrentProcess(), PROCESS_POWER_THROTTLING, &state, std::mem::size_of::<PowerThrottling>() as u32);
    });
}

#[cfg(not(windows))]
fn request_timer_resolution() {}

impl Default for Server {
    fn default() -> Self {
        Self::new()
    }
}

impl Server {
    pub fn new() -> Server {
        request_timer_resolution();
        LOG_TO_EVENTS.call_once(|| {
            log::add_sink(Box::new(|_lvl, line| events::emit(Event::Log { text: line.to_string() })));
        });
        let interp = Interp::new();
        let sh: Shared = Rc::new(RefCell::new(Ctx::new()));
        commands::register_all(&interp, &sh);
        interp.eval_global(commands::PRELUDE).expect("prelude must evaluate");
        let (tx, rx) = channel();
        let status = Arc::new(Mutex::new(Published { status: Status::default(), seen: Instant::now() }));
        Server { interp, sh, tx, rx, status }
    }

    pub fn cx(&self) -> Cx<'_> {
        Cx { interp: &self.interp, sh: &self.sh }
    }

    pub fn handle(&self) -> ServerHandle {
        ServerHandle { tx: self.tx.clone(), status: self.status.clone() }
    }

    /// Serves requests and polls targets until `shutdown`.
    pub fn run(&self) {
        let cx = self.cx();
        let mut next_poll = Instant::now();
        loop {
            // One borrow for both: with no DAQ session this costs a single Option comparison.
            let (stop, daq_due) = cx.with(|c| (c.shutdown, c.daq.next_due()));
            if stop {
                break;
            }
            // The loop always sleeps. A sleep cannot resolve better than the OS timer tick
            // (~512 us on Windows even with timeBeginPeriod(1)), so a deadline shorter than that
            // is always met late - which is exactly what the sampler's own controller corrects
            // for, by scheduling short and watching what comes out. Busy-waiting here would buy
            // even spacing for about eight times the CPU, and it is not worth it: at 1 kHz the
            // sleeping loop delivered *more* samples (989/s) than the spinning one (868/s).
            let wait = deadline(next_poll, daq_due).saturating_duration_since(Instant::now());
            let mut changed = match self.rx.recv_timeout(wait) {
                Ok(job) => {
                    job(&cx);
                    true
                }
                Err(RecvTimeoutError::Timeout) => false,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            // The run-state poll goes first when it is due, because it carries any DAQ point that
            // is due with it: one round trip instead of two, and the sample lands at the front of
            // the poll's batch instead of queueing behind it. That used to be the other way round
            // precisely because the poll pushed the deadline out - merging is what removed the
            // reason.
            if Instant::now() >= next_poll {
                ops::poll_all(&cx);
                let period = Duration::from_millis(cx.with(|c| c.poll_period_ms).max(1));
                next_poll = Instant::now() + period;
                changed = true;
            }
            // Not after a DAQ tick: that only moves bytes and runs up to ~1.5 kHz.
            if changed {
                publish(&cx, &self.status);
            }
            // Whatever the poll did not take: it only runs once a period, and it returns early
            // when polling is disabled or the device is not up, so this stays the primary path.
            let now = Instant::now();
            if cx.with(|c| c.daq.next_due().is_some_and(|d| now >= d)) {
                daq::tick(&cx, now);
            }
        }
        cx.with(|c| {
            // Dropping the senders lets every DAQ consumer see the end of its stream.
            c.daq = daq::Registry::default();
            if let Some(a) = c.adapter.as_deref_mut() {
                a.quit();
            }
        });
        events::emit(Event::Shutdown);
    }
}

/// When the server loop must wake next: the run-state poll, or a DAQ point if one is due sooner.
fn deadline(next_poll: Instant, daq_due: Option<Instant>) -> Instant {
    match daq_due {
        Some(d) => d.min(next_poll),
        None => next_poll,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_idle_daq_leaves_the_poll_deadline_alone() {
        let t = Instant::now();
        assert_eq!(deadline(t, None), t);
        assert_eq!(deadline(t, Some(t + Duration::from_millis(5))), t);
        let sooner = t - Duration::from_millis(5);
        assert_eq!(deadline(t, Some(sooner)), sooner);
    }
}
