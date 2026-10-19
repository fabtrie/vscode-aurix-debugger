//! Times winIDEA's own source-level steps: `srcstep_probe <workspace>`, starting at the
//! function AURIX_TEST_FUNCTION (default `main`).
//! Expects cpu0 halted; leaves it running at the end.
use std::time::Instant;
use winidea_sdk::{AddressController, ConnectionMgr, ExecutionController, WinIdea};
fn main() {
    let ws = std::env::args().nth(1).unwrap();
    let w = WinIdea::load(&winidea_sdk::default_search_paths()).unwrap();
    let norm = |s: &str| s.replace(char::from(92u8), "/").to_ascii_lowercase();
    let inst = ConnectionMgr::new(&w).unwrap().enumerate_instances().unwrap().into_iter()
        .find(|i| norm(&i.workspace) == norm(&ws)).unwrap();
    let mut c = ConnectionMgr::new(&w).unwrap();
    c.connect_tcp("", inst.tcp_port).unwrap();
    {
        let mut e = ExecutionController::new(&w, &c).unwrap();
        let mut a = AddressController::new(&w, &c).unwrap();
        let mut show = |label: &str, t0: Instant, e: &ExecutionController| {
            let ms = t0.elapsed().as_secs_f64() * 1e3;
            let pc = e.cpu_status().map(|s| s.execution_point).unwrap_or(0);
            let line = a.source_line_at_address(pc).ok().flatten()
                .map(|l| format!("{l}")).unwrap_or_default();
            println!("  {label:<22} {ms:>8.1} ms  pc={pc:#010x} {line}");
        };
        let s = e.cpu_status().unwrap();
        if !s.is_stopped { e.stop().unwrap(); }
        let t0 = Instant::now();
        e.run_until_function(&std::env::var("AURIX_TEST_FUNCTION").unwrap_or_else(|_| "main".into())).unwrap();
        println!("  reached: {:?}", e.wait_until_stopped(5000));
        println!("  run until function      {:>8.1} ms", t0.elapsed().as_secs_f64() * 1e3);
        show("start", Instant::now(), &e);
        for _ in 0..5 { let t0 = Instant::now(); e.step_over().unwrap(); show("step over (source)", t0, &e); }
        for _ in 0..2 { let t0 = Instant::now(); e.step_into().unwrap(); show("step into (source)", t0, &e); }
        let t0 = Instant::now(); e.run_until_return().unwrap(); show("step out", t0, &e);
        for _ in 0..3 { let t0 = Instant::now(); e.step_instruction().unwrap(); show("step instruction", t0, &e); }
        let t0 = Instant::now(); e.run_with_timeout(winidea_sdk::TimeoutMode::None).unwrap();
        println!("  run(NONE)              {:>8.1} ms", t0.elapsed().as_secs_f64() * 1e3);
    }
    let _ = c.disconnect_keep();
}
