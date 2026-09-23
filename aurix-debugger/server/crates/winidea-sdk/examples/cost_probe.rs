#![allow(unused_must_use)]
//! What does each SDK call aurix-dbg makes actually cost?
//! `cost_probe <primary workspace>`. Halts cpu0 briefly, steps it, resumes it.

use std::time::Instant;

use winidea_sdk::{
    breakpoints::BreakpointController, AccessFlags, ConnectionMgr, DataController, DebugFacade,
    ExecutionController, WinIdea,
};

fn t<T>(label: &str, n: u32, mut f: impl FnMut() -> T) -> T {
    let mut last = None;
    let t0 = Instant::now();
    for _ in 0..n {
        last = Some(f());
    }
    println!("  {label:<46} {:>9.1} us", t0.elapsed().as_secs_f64() * 1e6 / n as f64);
    last.unwrap()
}

fn main() {
    let ws = std::env::args().nth(1).expect("usage: cost_probe <workspace>");
    let w = WinIdea::load(&winidea_sdk::default_search_paths()).unwrap();
    let norm = |s: &str| s.replace('\\', "/").to_ascii_lowercase();
    let inst = ConnectionMgr::new(&w).unwrap().enumerate_instances().unwrap().into_iter()
        .find(|i| norm(&i.workspace) == norm(&ws)).expect("instance");
    let mut c = ConnectionMgr::new(&w).unwrap();
    c.connect_tcp("", inst.tcp_port).unwrap();

    if std::env::args().nth(2).as_deref() == Some("nowait") {
        nowait(&w, &c);
        let _ = c.disconnect_keep();
        return;
    }
    println!("controller construction (aurix-dbg builds one per call):");
    t("ExecutionController::new + drop", 20, || drop(ExecutionController::new(&w, &c).unwrap()));
    t("DebugFacade::new + drop", 20, || drop(DebugFacade::new(&w, &c).unwrap()));
    t("DataController::new + drop", 20, || drop(DataController::new(&w, &c).unwrap()));
    t("BreakpointController::new + drop", 20, || drop(BreakpointController::new(&w, &c).unwrap()));

    let mut exec = ExecutionController::new(&w, &c).unwrap();
    let mut dbg = DebugFacade::new(&w, &c).unwrap();
    let mut data = DataController::new(&w, &c).unwrap();

    println!("running target:");
    t("cpu_status (reused controller)", 50, || exec.cpu_status().map(|_| ()));
    t("read 4 B, REAL_TIME|DIRECT", 10, || data.read_memory_maus(AccessFlags::REAL_TIME | AccessFlags::DIRECT, 0, 0x7000_0000, 4, 1).map(|_| ()));

    println!("stop / step / run:");
    t("stop()", 1, || exec.stop());
    t("wait_until_stopped(1000)", 1, || exec.wait_until_stopped(1000));
    t("cpu_status (halted)", 20, || exec.cpu_status().map(|_| ()));
    t("read_register(PC)", 20, || dbg.read_register("PC").map(|_| ()));
    t("read_register(A10)", 20, || dbg.read_register("A10").map(|_| ()));
    t("read 4 B,  REAL_TIME|DIRECT (halted)", 10, || data.read_memory_maus(AccessFlags::REAL_TIME | AccessFlags::DIRECT, 0, 0x7000_0000, 4, 1).map(|_| ()));
    t("read 4 B,  DEFAULT (halted)", 10, || data.read_memory_maus(AccessFlags::DEFAULT, 0, 0x7000_0000, 4, 1).map(|_| ()));
    t("read 64 B, DEFAULT (halted, CSA-sized)", 10, || data.read_memory_maus(AccessFlags::DEFAULT, 0, 0x7000_0000, 64, 1).map(|_| ()));
    t("read 4 B CSFR window (ICR) DEFAULT", 10, || data.read_memory_maus(AccessFlags::DEFAULT, 0, 0xF881_FE2C, 4, 1).map(|_| ()));
    t("step_instruction", 10, || exec.step_instruction().map(|_| ()));
    t("step_over (source line, winIDEA)", 5, || exec.step_over().map(|_| ()));
    t("run()", 1, || exec.run().map(|_| ()));
    drop((exec, dbg, data));
    let _ = c.disconnect_keep();
}

/// Blocking vs non-blocking run control: `cost_probe <ws> nowait`.
#[allow(dead_code)]
fn nowait(w: &WinIdea, c: &ConnectionMgr) {
    use winidea_sdk::TimeoutMode;
    let mut exec = ExecutionController::new(w, c).unwrap();
    let wait_stopped = |exec: &ExecutionController, label: &str, t0: Instant| {
        let mut polls = 0;
        loop {
            polls += 1;
            let s = exec.cpu_status().unwrap();
            if s.is_stopped || s.is_halted {
                println!("  {label:<46} {:>9.1} us to stopped ({polls} status polls)", t0.elapsed().as_secs_f64() * 1e6);
                return;
            }
            if t0.elapsed().as_secs() > 5 {
                println!("  {label}: not stopped after 5 s");
                return;
            }
        }
    };
    let t0 = Instant::now();
    exec.stop().unwrap();
    wait_stopped(&exec, "stop()", t0);
    for polling in [true, false] {
        exec.set_polling_enabled(polling).unwrap();
        println!("winIDEA polling {}:", if polling { "on" } else { "off" });
        for _ in 0..3 {
            let t0 = Instant::now();
            exec.step_instruction_with(TimeoutMode::None).unwrap();
            let issued = t0.elapsed().as_secs_f64() * 1e6;
            wait_stopped(&exec, &format!("stepInst(NONE) [call {issued:.0} us]"), t0);
        }
        let t0 = Instant::now();
        exec.step_over_with(TimeoutMode::None).unwrap();
        let issued = t0.elapsed().as_secs_f64() * 1e6;
        wait_stopped(&exec, &format!("stepOverHigh(NONE) [call {issued:.0} us]"), t0);
        let t0 = Instant::now();
        exec.step_instruction().unwrap();
        println!("  {:<46} {:>9.1} us", "stepInst() blocking, for comparison", t0.elapsed().as_secs_f64() * 1e6);
    }
    exec.set_polling_enabled(true).unwrap();
    let t0 = Instant::now();
    exec.run_with_timeout(TimeoutMode::None).unwrap();
    println!("  {:<46} {:>9.1} us", "run(NONE)", t0.elapsed().as_secs_f64() * 1e6);
}
