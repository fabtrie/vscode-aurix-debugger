//! The `mcds` command against the simulator's MCDS model: data trace of the debugger's own
//! stores, program trace stopped when full and on halt, a reset ending the recording.

use aurix_core::sim::SimAdapter;
use aurix_core::Server;
use serde_json::Value;

fn server() -> Server {
    aurix_core::log::set_debug_level(0);
    let s = Server::new();
    s.cx().with(|c| c.adapter_override = Some("sim".into()));
    s.interp.eval_global("adapter driver sim").unwrap();
    s.interp.eval_global("init").unwrap();
    s
}

fn tcl(s: &Server, cmd: &str) -> Result<String, String> {
    s.interp.eval_global(cmd)
}

fn json(s: &Server, cmd: &str) -> Value {
    serde_json::from_str(&tcl(s, cmd).unwrap_or_else(|e| panic!("{cmd}: {e}"))).unwrap()
}

fn sim<R>(s: &Server, f: impl FnOnce(&mut SimAdapter) -> R) -> R {
    s.cx().with(|c| f(c.adapter.as_mut().unwrap().as_any_mut().downcast_mut::<SimAdapter>().unwrap()))
}

#[test]
fn data_trace_of_stores() {
    let s = server();
    assert_eq!(json(&s, "mcds status -json")["armed"], Value::Bool(false));
    let a = json(&s, "mcds arm -data 0x70000100 16 -json");
    assert_eq!(a["config"]["kind"], "data");
    assert_eq!(a["resources"]["dtu_comparators"], 1);
    tcl(&s, "mww 0x70000100 0x11").unwrap();
    tcl(&s, "mww 0x70000104 0x22").unwrap();
    tcl(&s, "mww 0x70000200 0x33").unwrap(); // outside the range
    assert_eq!(sim(&s, |a| a.mcds.recorded), 2);
    let st = json(&s, "mcds status -json");
    assert_eq!(st["armed"], Value::Bool(true));
    assert_eq!(st["owner"], "snapshot");
    // Two writes: a ring that has not gone round is as full as its write pointer says.
    let f = st["fill"].as_f64().unwrap();
    assert!(f > 0.0 && f < 0.1, "{st}");
    let r = json(&s, "mcds stop -json");
    assert_eq!(r["ring"], Value::Bool(true));
    let d = json(&s, "mcds decode -json");
    let w: Vec<&Value> = d["events"].as_array().unwrap().iter().filter(|e| e["type"] == "write").collect();
    assert_eq!(w.len(), 2, "{d}");
    assert_eq!(w[0]["addr"], "0x70000100");
    assert_eq!(w[0]["value"], 0x11);
    assert_eq!(w[1]["value"], 0x22);
    assert!(w[1]["t_ns"].as_u64().unwrap() >= w[0]["t_ns"].as_u64().unwrap());
    assert_eq!(d["undecodable"], 0);
}

#[test]
fn program_trace_full_and_on_halt() {
    let s = server();
    json(&s, "mcds arm -program -stop full -json");
    let st = json(&s, "mcds status -json");
    assert_eq!(st["done"], Value::Bool(true), "{st}");
    assert!(st["fill"].as_f64().unwrap() > 0.99, "{st}");
    let r = json(&s, "mcds stop -json");
    assert_eq!(r["done"], Value::Bool(true));
    assert_eq!(r["paragraphs"], 8);
    let d = json(&s, "mcds decode -json 100000");
    assert!(d["events"].as_array().unwrap().iter().any(|e| e["type"] == "address"));
    assert_eq!(d["undecodable"], 0);

    // Stop on halt: a ring the debugger stops when the poll sees the core halt (after a flush,
    // see mcds_trace::on_halted); without targets in this test, stopped by hand.
    json(&s, "mcds arm -program -stop halt -json");
    let st = json(&s, "mcds status -json");
    assert_eq!(st["done"], Value::Bool(false));
    // The simulated ring went round one and a half times: full, with the writer in the middle.
    assert_eq!(st["fill"], 1.0, "{st}");
    assert!(tcl(&s, "mcds status").unwrap().contains("100 % full"));
    let r = json(&s, "mcds stop -json");
    assert_eq!(r["ring"], Value::Bool(true));
    assert!(r["paragraphs"].as_u64().unwrap() >= 7, "{r}");
}

#[test]
fn reset_ends_the_recording_and_plan_needs_no_target() {
    let s = server();
    let p = json(&s, "mcds plan -program -filter 0x80001000 0x200 -stop halt -json");
    assert_eq!(p["resources"]["ptu_comparators"], 1);
    assert!(p["register_writes"].as_array().unwrap().iter().any(|w| w == "0xfb718800=0xffffffbf"), "{p}");
    assert!(tcl(&s, "mcds arm -core 3 -program").unwrap_err().contains("CPU3"));
    // The observed core: winIDEA's POB_X = CPU1 writes 0x82.
    let p = json(&s, "mcds plan -core 1 -program -json");
    assert_eq!(p["config"]["core"], 1);
    assert!(p["register_writes"].as_array().unwrap().iter().any(|w| w == "0xfb718014=0x82"), "{p}");
    // A finished recording, then a reset: it says it is from before the reset (no longer the
    // trace up to where the core stands).
    json(&s, "mcds arm -program -stop full -json");
    json(&s, "mcds stop -json");
    assert_eq!(json(&s, "mcds status -json")["last"]["ran_since"], Value::Null);
    json(&s, "mcds arm -program -stop manual -json");
    sim(&s, |a| a.mcds.reset());
    s.cx().with(aurix_core::mcds_trace::on_reset);
    let st = json(&s, "mcds status -json");
    assert_eq!(st["armed"], Value::Bool(false));
    assert_eq!(st["lost_by_reset"], Value::Bool(true));
    assert_eq!(st["last"]["ran_since"], "reset", "{st}");
    // Arming again switches the MCDS on again.
    json(&s, "mcds arm -program -stop full -json");
    assert_eq!(json(&s, "mcds status -json")["done"], Value::Bool(true));
}

#[test]
fn daq_traced_points() {
    use aurix_core::daq::{self, DaqMsg, PointCfg, SessionCfg, Source, Space};
    use std::time::{Duration, Instant};
    let s = server();
    let t0 = Instant::now();
    let (id, rx) = s.cx().with(|c| c.daq.open(SessionCfg { queue: 256 }, t0).unwrap());
    let traced = PointCfg { trace: true, ..PointCfg::new(0x7000_0100, 4, Space::Host, 100_000) };
    let far = PointCfg { trace: true, ..PointCfg::new(0x7001_0000, 4, Space::Host, 100_000) };
    let pts = s.cx().with(|c| c.daq.add_points(id, &[traced, far], t0).unwrap());
    let (p1, p2) = (pts[0].0, pts[1].0);
    daq::tick_at(&s.cx(), t0 + Duration::from_millis(60), Some(t0));
    assert_eq!(s.cx().with(|c| c.daq.source(id, p1)), Some(Source::Trace));
    assert_eq!(s.cx().with(|c| c.daq.source(id, p2)), Some(Source::Polled));
    assert_eq!(s.cx().with(|c| c.mcds.armed.as_ref().map(|a| a.owner)), Some(aurix_core::mcds_trace::Owner::Daq));
    // A snapshot cannot take the trace while the DAQ owns it.
    assert!(tcl(&s, "mcds arm -program").unwrap_err().contains("in use"));
    s.interp.eval_global("mww 0x70000100 0x1234").unwrap();
    s.interp.eval_global("mwh 0x70000102 0xabcd").unwrap();
    daq::tick_at(&s.cx(), t0 + Duration::from_millis(70), Some(t0));
    let msgs: Vec<DaqMsg> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    let reason = msgs.iter().find_map(|m| match m {
        DaqMsg::SourceChanged { point, reason, .. } if *point == p2 => reason.clone(),
        _ => None,
    });
    assert!(reason.unwrap_or_default().contains("outside the traced range"), "{msgs:?}");
    let values: Vec<&(u32, Option<u64>, Vec<u8>)> = msgs.iter().filter_map(|m| if let DaqMsg::Trace { values, .. } = m { Some(values) } else { None }).flatten().collect();
    assert!(!values.is_empty(), "{msgs:?}");
    let last = values.last().unwrap();
    assert_eq!(last.0, p1);
    assert!(last.1.is_some(), "target time");
    // The halfword store merged into the word.
    assert_eq!(u32::from_le_bytes(last.2.clone().try_into().unwrap()), 0xabcd_1234, "{values:?}");
    // Closing the session releases the trace.
    s.cx().with(|c| c.daq.close(id));
    daq::tick_at(&s.cx(), t0 + Duration::from_millis(200), Some(t0));
    assert!(s.cx().with(|c| c.mcds.armed.is_none()));
}

/// The Lite Kit board cfg on the simulator, initialised, CPU0 selected and running.
fn board_running() -> Server {
    aurix_core::log::set_debug_level(0);
    let s = Server::new();
    s.cx().with(|c| {
        c.adapter_override = Some("sim".into());
        c.search_dirs.push(std::path::PathBuf::from(openocd_tcl::TCL_DIR));
    });
    s.interp.eval_global("script board/infineon/kit_a2g_tc375_lite.cfg").unwrap();
    s.interp.eval_global(aurix_core::sim::SIM_BOARD_SETTINGS).unwrap();
    s.interp.eval_global("hsm ready_ms 0").unwrap();
    s.interp.eval_global("init").unwrap();
    s.interp.eval_global("targets tc37x.cpu0").unwrap();
    s
}

/// `mcds auto` set while the core runs (a launch sets it after the reset started the
/// application) arms at once: the run to the first halt is recorded, not only the runs after it.
#[test]
fn auto_set_while_running_arms_at_once() {
    let s = board_running();
    assert_eq!(tcl(&s, "tc37x.cpu0 curstate").unwrap(), "running");
    json(&s, "mcds auto -program -stop halt -json");
    let st = json(&s, "mcds status -json");
    assert_eq!(st["armed"], Value::Bool(true), "{st}");
    assert_eq!(st["owner"], "snapshot");
    // The halt reads it.
    tcl(&s, "halt").unwrap();
    let st = json(&s, "mcds status -json");
    assert_eq!(st["armed"], Value::Bool(false), "{st}");
    assert_eq!(st["last"]["ran_since"], Value::Null, "{st}");
    assert!(st["last"]["paragraphs"].as_u64().unwrap() >= 7, "{st}");
    // Set again while halted: armed on the next run only.
    json(&s, "mcds auto -program -stop halt -json");
    assert_eq!(json(&s, "mcds status -json")["armed"], Value::Bool(false));
    tcl(&s, "resume").unwrap();
    assert_eq!(json(&s, "mcds status -json")["armed"], Value::Bool(true));
    // A full ring keeps no earlier recording (one trace RAM in all).
    tcl(&s, "halt").unwrap();
    let st = json(&s, "mcds status -json");
    assert_eq!(st["last"]["earlier_paragraphs"], 0, "{st}");
    json(&s, "mcds auto off -json");
}

/// Another core's trace: the observation block follows CPU1, the CPU1 halt (not CPU0's) reads
/// it, and only a CPU1 run arms it again.
#[test]
fn auto_trace_of_cpu1() {
    let s = board_running();
    tcl(&s, "targets tc37x.cpu1").unwrap();
    assert_eq!(tcl(&s, "tc37x.cpu1 curstate").unwrap(), "running");
    let v = json(&s, "mcds auto -core 1 -program -stop halt -json");
    assert_eq!(v["auto"]["core"], 1, "{v}");
    let st = json(&s, "mcds status -json");
    assert_eq!(st["armed"], Value::Bool(true), "{st}");
    assert_eq!(sim(&s, |a| a.mcds.observed_core()), 1);
    tcl(&s, "tc37x.cpu1 arp_halt").unwrap();
    tcl(&s, "tc37x.cpu1 arp_poll").unwrap();
    let st = json(&s, "mcds status -json");
    assert_eq!(st["armed"], Value::Bool(false), "{st}");
    assert!(st["last"]["paragraphs"].as_u64().unwrap() >= 7, "{st}");
    json(&s, "mcds auto off -json");
}

#[test]
fn auto_with_steps_records_steps_and_a_source_step_as_one() {
    let s = board_running();
    tcl(&s, "halt").unwrap();
    // Without -steps: a step leaves the last recording behind (it no longer ends at the core).
    let v = json(&s, "mcds auto -program -stop halt -json");
    assert_eq!(v["steps"], Value::Bool(false), "{v}");
    tcl(&s, "resume").unwrap();
    tcl(&s, "halt").unwrap();
    tcl(&s, "step").unwrap();
    let st = json(&s, "mcds status -json");
    assert_eq!(st["armed"], Value::Bool(false), "{st}");
    assert_eq!(st["last"]["ran_since"], "step", "{st}");

    // With -steps: the step is recorded and read at its halt.
    let v = json(&s, "mcds auto -program -stop halt -steps -json");
    assert_eq!(v["steps"], Value::Bool(true), "{v}");
    let before = json(&s, "mcds status -json")["last"]["stopped_ms"].clone();
    tcl(&s, "step").unwrap();
    let st = json(&s, "mcds status -json");
    assert_eq!(st["armed"], Value::Bool(false), "{st}");
    assert_eq!(st["last"]["ran_since"], Value::Null, "{st}");
    assert_ne!(st["last"]["stopped_ms"], before, "a new recording: {st}");

    // A source step: one recording over all its instruction steps, read at its end.
    s.cx().with(|c| {
        c.source_steps += 1;
        aurix_core::mcds_trace::begin_step(c, 0);
    });
    tcl(&s, "step").unwrap();
    assert_eq!(json(&s, "mcds status -json")["armed"], Value::Bool(true), "still recording inside the source step");
    tcl(&s, "step").unwrap();
    s.cx().with(|c| {
        c.source_steps -= 1;
        aurix_core::mcds_trace::end_step(c, 0);
    });
    let st = json(&s, "mcds status -json");
    assert_eq!(st["armed"], Value::Bool(false), "{st}");
    assert_eq!(st["last"]["ran_since"], Value::Null, "{st}");

    let v = json(&s, "mcds auto off -json");
    assert_eq!((v["auto"].clone(), v["steps"].clone()), (Value::Null, Value::Bool(false)), "{v}");
}
