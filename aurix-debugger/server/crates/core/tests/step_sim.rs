//! Instruction steps against the simulator: the registers read along with the halt, and the
//! `halted` handlers of instruction steps deferred to the end of a source-level step but run on
//! every direct step and for a run inside it.

use std::path::PathBuf;

use aurix_core::{ops, Server};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize().unwrap()
}

fn tcl_path(p: PathBuf) -> String {
    p.to_string_lossy().trim_start_matches(r"\\?\").replace('\\', "/")
}

/// The board cfgs, initialised, CPU0 halted, its halted handler counting into `::halts`.
fn server() -> Server {
    aurix_core::log::set_debug_level(0);
    let s = Server::new();
    s.cx().with(|c| {
        c.adapter_override = Some("sim".into());
        c.search_dirs.push(PathBuf::from(openocd_tcl::TCL_DIR));
    });
    s.interp.eval_global("script board/infineon/kit_a2g_tc375_lite.cfg").unwrap();
    s.interp.eval_global(aurix_core::sim::SIM_BOARD_SETTINGS).unwrap();
    s.interp
        .eval_global(&format!("script {}", jim::quote(&tcl_path(repo().join("tc37x_fs26_example.cfg")))))
        .unwrap();
    s.interp.eval_global("hsm ready_ms 0").unwrap();
    s.interp.eval_global("set sbc_rstb_wired 0; set sbc_disarmed 1").unwrap();
    s.interp.eval_global("init").unwrap();
    s.interp.eval_global("targets tc37x.cpu0").unwrap();
    s.interp.eval_global("halt").unwrap();
    tcl(&s, "set halts 0; set h [tc37x.cpu0 cget -event halted]; tc37x.cpu0 configure -event halted \"incr ::halts; $h\"");
    s
}

fn tcl(s: &Server, cmd: &str) -> String {
    s.interp.eval_global(cmd).unwrap_or_else(|e| panic!("{cmd}: {e}"))
}

fn halts(s: &Server) -> u32 {
    tcl(s, "set ::halts").parse().unwrap()
}

/// PC as the debugger reports it (cached with the step's halt) and as the chip holds it.
fn pcs(s: &Server) -> (u32, u32) {
    let shown = tcl(s, "reg pc");
    let shown = u32::from_str_radix(shown.rsplit("0x").next().unwrap().trim(), 16).unwrap();
    let chip = tcl(s, "read_memory 0xF881FE08 32 1");
    (shown, u32::from_str_radix(chip.trim_start_matches("0x"), 16).unwrap())
}

#[test]
fn direct_steps_run_the_halted_handler_every_time() {
    let s = server();
    let pc0 = pcs(&s).1;
    for n in 1..=3 {
        tcl(&s, "step");
        assert_eq!(halts(&s), n, "halted handler after direct step {n}");
        let (shown, chip) = pcs(&s);
        assert_eq!(shown, chip, "PC read with the halt differs from the chip's");
        assert_ne!(chip, pc0, "the step did not move the PC");
        assert_eq!(ops::state(&s.cx(), 0).1.name(), "single-step");
    }
}

#[test]
fn a_source_step_runs_the_halted_handler_once_at_its_end() {
    let s = server();
    let cx = s.cx();
    ops::begin_source_step(&cx);
    for _ in 0..3 {
        ops::step_as(&cx, 0, None, aurix_core::target::RunMode::StepOver).unwrap();
    }
    assert_eq!(halts(&s), 0, "halted handler ran inside the source step");
    ops::end_source_step(&cx);
    assert_eq!(halts(&s), 1, "halted handler not run once at the end");
    // Direct steps afterwards are not deferred.
    tcl(&s, "step");
    assert_eq!(halts(&s), 2);
}

#[test]
fn a_source_step_that_ends_running_runs_no_halted_handler() {
    let s = server();
    let cx = s.cx();
    ops::begin_source_step(&cx);
    ops::step(&cx, 0, None).unwrap();
    tcl(&s, "resume");
    ops::end_source_step(&cx);
    assert_eq!(halts(&s), 0);
}

/// The halt of a run inside a source step (`run_to` over a call) is handled at once: its handler
/// restores what the resume-end handler undid (suspend-out with frozen timers) before the
/// instruction steps after it; their halts still wait for the step's end.
#[test]
fn a_run_inside_a_source_step_runs_the_halted_handler_at_once() {
    let s = server();
    let cx = s.cx();
    ops::begin_source_step(&cx);
    ops::step(&cx, 0, None).unwrap();
    assert_eq!(halts(&s), 0);
    tcl(&s, "resume");
    tcl(&s, "halt");
    assert_eq!(halts(&s), 1, "halted handler of the run's halt deferred");
    ops::step(&cx, 0, None).unwrap();
    assert_eq!(halts(&s), 1, "halted handler of an instruction step ran inside the source step");
    ops::end_source_step(&cx);
    assert_eq!(halts(&s), 2);
}
