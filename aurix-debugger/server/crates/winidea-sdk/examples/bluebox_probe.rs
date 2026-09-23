//! Hardware probe for the aurix-dbg `winidea` backend.
//!
//! Attaches to an already running winIDEA (never launches one) and answers the
//! questions the backend could not settle without hardware:
//!
//! * what a single SDK call costs (status, small and large reads, a register),
//!   which decides the run-state poll design and the DAQ ceiling;
//! * which memory area ids the target reports;
//! * how the secondary cores are reached.
//!
//! Nothing is written to the target and nothing is run or halted.
//!
//! ```text
//! cargo run -p winidea-sdk --example bluebox_probe
//! ```

use std::time::Instant;

use winidea_sdk::config::{ConnectionConfig, StartMode};
use winidea_sdk::memareas::MemAreas;
use winidea_sdk::{AccessFlags, ConnectionMgr, DataController, DebugFacade, ExecutionController, WinIdea};

fn time<T>(label: &str, n: u32, mut f: impl FnMut() -> T) -> T {
    let mut last = None;
    let t0 = Instant::now();
    for _ in 0..n {
        last = Some(f());
    }
    let us = t0.elapsed().as_secs_f64() * 1e6 / n as f64;
    println!("  {label:<40} {us:>9.1} us/call  (n={n})");
    last.unwrap()
}

fn main() {
    let winidea = WinIdea::load(&winidea_sdk::default_search_paths()).expect("load isystemConnect.dll");
    let mut cfg = ConnectionConfig::new(&winidea).unwrap();
    cfg.start(StartMode::Existing).unwrap();
    let mut conn = ConnectionMgr::new(&winidea).unwrap();
    conn.connect_with(&cfg).expect("attach to the running winIDEA");
    println!("winIDEA {}", conn.version().unwrap().to_string_lossy());

    let exec = ExecutionController::new(&winidea, &conn).unwrap();
    let st = exec.cpu_status().unwrap();
    println!(
        "cpu status: running={} stopped={} halted={} reset={} attached={} pc={:#x} area={}",
        st.is_running, st.is_stopped, st.is_halted, st.is_reset, st.is_attached, st.execution_point,
        st.execution_area
    );

    let mut data = DataController::new(&winidea, &conn).unwrap();
    match MemAreas::read(&winidea, &mut data) {
        Ok(a) => println!("memory areas: {a:?}"),
        Err(e) => println!("memory areas: {e}"),
    }

    println!("\n[B1] per-call cost");
    let _ = time("cpu_status()", 50, || exec.cpu_status().map(|_| ()));
    let rt = AccessFlags::REAL_TIME | AccessFlags::DIRECT;
    for (label, addr, len) in [
        ("read 4 B   DSPR0 0x70000000", 0x7000_0000u64, 4u64),
        ("read 4 KB  DSPR0 0x70000000", 0x7000_0000, 4096),
        ("read 4 B   SFR   0xF0000480 (OSTATE)", 0xF000_0480, 4),
        ("read 4 B   DBGSR 0xF881FD00", 0xF881_FD00, 4),
    ] {
        match time(label, 20, || data.read_memory_maus(rt, 0, addr, len, 1)) {
            Ok(b) => println!(
                "      -> readable={} first={:02x?}",
                b.is_fully_readable(),
                &b.data[..b.data.len().min(8)]
            ),
            Err(e) => println!("      -> {e}"),
        }
    }
    let mut dbg = DebugFacade::new(&winidea, &conn).unwrap();
    for reg in ["PC", "A10", "D0", "PSW"] {
        match time(&format!("read_register({reg})"), 20, || dbg.read_register(reg)) {
            Ok(v) => println!("      -> {v}"),
            Err(e) => println!("      -> {e}"),
        }
    }

    println!("\n[B1b] does the access mode explain the ~4 ms?");
    for (label, flags) in [
        ("DEFAULT", AccessFlags::DEFAULT),
        ("DIRECT", AccessFlags::DIRECT),
        ("REAL_TIME", AccessFlags::REAL_TIME),
        ("REAL_TIME|DIRECT", rt),
    ] {
        let _ = time(&format!("read 4 B   {label}"), 20, || {
            data.read_memory_maus(flags, 0, 0x7000_0000, 4, 1)
        });
    }
    let _ = time("read 64 KB DEFAULT", 5, || {
        data.read_memory_maus(AccessFlags::DEFAULT, 0, 0x7000_0000, 0x10000, 1)
    });

    println!("\n[B2] multi-core (read-only check first)");
    for i in 0..4 {
        match conn.is_core_launched(i) {
            Ok(v) => println!("  is_core_launched({i}) = {v}"),
            Err(e) => println!("  is_core_launched({i}): {e}"),
        }
    }
    drop((exec, data, dbg));

    // Further cores: see examples/core_launch_probe.rs.
    let _ = conn.disconnect_keep();
}
