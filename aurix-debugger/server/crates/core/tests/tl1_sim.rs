//! The server's TL1 multi-core run control (`ocds ...`) against the simulated TC37x, which models the
//! TL1 suspend targets, capture-and-hold and the cores' BRKOUT. The OpenOCD board cfg only: no tc3xx
//! scripts.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use aurix_core::sim::SimAdapter;
use aurix_core::{ops, Server};

const CBS_TL1ST: u32 = 0xF000_0494;
const CBS_TLCHS: u32 = 0xF000_049C;
const DBGSR: [u32; 3] = [0xF881_FD00, 0xF883_FD00, 0xF885_FD00];

fn server(settings: &str) -> Server {
    aurix_core::log::set_debug_level(1);
    let s = Server::new();
    s.cx().with(|c| {
        c.adapter_override = Some("sim".into());
        c.search_dirs.push(PathBuf::from(openocd_tcl::TCL_DIR));
    });
    s.interp.eval_global("script board/infineon/kit_a2g_tc375_lite.cfg").unwrap();
    s.interp.eval_global(settings).unwrap();
    s.interp.eval("init").unwrap();
    s
}

fn with_sim<R>(s: &Server, f: impl FnOnce(&mut SimAdapter) -> R) -> R {
    s.cx().with(|c| f(c.adapter.as_mut().unwrap().as_any_mut().downcast_mut::<SimAdapter>().unwrap()))
}

fn take_writes(s: &Server) -> Vec<(u32, u32)> {
    with_sim(s, |sim| std::mem::take(&mut sim.writes))
}

fn halted(s: &Server) -> [bool; 3] {
    with_sim(s, |sim| sim.halted)
}

/// CBS_TRCx, CBS_TRHSM, CBS_TLC/TL1ST/TLCHE/TLCHS.
fn cbs(a: u32) -> bool {
    (0xF000_0420..=0xF000_0438).contains(&a) || (0xF000_0490..=0xF000_049C).contains(&a)
}

fn wait_polling(s: &Server, what: &str, f: impl Fn(&Server) -> bool) {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(2) {
        ops::poll_all(&s.cx());
        if f(s) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn without_sync_no_trigger_register_is_written_and_cores_run_alone() {
    let s = server("");
    assert_eq!(s.interp.eval("ocds sync").unwrap(), "none");
    s.interp.eval("halt 1000").unwrap();
    assert_eq!(halted(&s), [true, false, false], "a halt of cpu0 stopped other cores");
    s.interp.eval("step").unwrap();
    s.interp.eval("resume").unwrap();
    let w = take_writes(&s);
    assert!(!w.iter().any(|&(a, _)| cbs(a)), "trigger registers written without sync: {w:08x?}");
}

#[test]
fn pause_halts_all_cores_and_resume_starts_them_together() {
    let s = server("ocds sync tl1");
    // No HSM target: TL1 suspends the CPUs, HSSL0 and DMA.
    assert_eq!(with_sim(&s, |sim| sim.mem_word(CBS_TL1ST)), 0x3000_003F);
    s.interp.eval("halt 1000").unwrap();
    assert_eq!(halted(&s), [true; 3]);
    assert!(with_sim(&s, |sim| sim.tl1_held));
    take_writes(&s);
    s.interp.eval("resume").unwrap();
    let w = take_writes(&s);
    // The others released under the hold, cpu0 resumed, then the hold dropped: they started then.
    let others = w.iter().position(|&x| x == (DBGSR[1], 0x4)).expect("cpu1 not released");
    let own = w.iter().rposition(|&x| x == (DBGSR[0], 0x4)).expect("cpu0 not resumed");
    let drop = w.iter().position(|&x| x == (CBS_TLCHS, 2)).expect("hold not dropped");
    assert!(others < own && own < drop, "{w:08x?}");
    assert_eq!(halted(&s), [false; 3]);
    assert!(!with_sim(&s, |sim| sim.tl1_held));
    // The released cores count as running at once, before any background poll.
    for i in 0..3 {
        assert_eq!(s.interp.eval(&format!("tc37x.cpu{i} curstate")).unwrap(), "running", "cpu{i}");
    }
}

#[test]
fn reverse_debugging_sees_a_run_started_by_another_core_of_the_group() {
    let s = server("ocds sync tl1");
    s.interp.eval("halt 1000").unwrap();
    s.interp.eval("targets tc37x.cpu0").unwrap();
    s.interp.eval("reverse on -notrace -region 0x70000000 0x100").unwrap();
    s.interp.eval("mww 0x70000010 0x5").unwrap();
    assert_eq!(s.cx().with(|c| c.reverse.history.len()), 1);
    // Resuming cpu1 runs cpu0 too: its history no longer leads to its state.
    s.interp.eval("targets tc37x.cpu1; resume").unwrap();
    assert_eq!(s.interp.eval("tc37x.cpu0 curstate").unwrap(), "running");
    assert_eq!(s.cx().with(|c| c.reverse.history.len()), 0);
    assert!(s.cx().with(|c| c.reverse.cleared.clone()).is_some_and(|w| w.contains("outside")));
}

#[test]
fn step_releases_the_others_under_the_hold_or_keeps_them_halted() {
    let s = server("ocds sync tl1");
    s.interp.eval("halt 1000").unwrap();
    take_writes(&s);
    s.interp.eval("step").unwrap();
    let w = take_writes(&s);
    assert!(w.contains(&(DBGSR[1], 0x4)) && w.contains(&(CBS_TLCHS, 2)), "{w:08x?}");
    // cpu0's step trigger pulsed TL1 through its BRKOUT: all halted again, the hold latched.
    assert_eq!(halted(&s), [true; 3]);
    assert!(with_sim(&s, |sim| sim.tl1_held));

    s.interp.eval("ocds step single").unwrap();
    s.interp.eval("step").unwrap();
    let w = take_writes(&s);
    let keep = w.iter().position(|&x| x == (DBGSR[1], 0x16)).expect("cpu1 not kept halted");
    let drop = w.iter().position(|&x| x == (CBS_TLCHS, 2)).expect("hold not dropped");
    assert!(keep < drop, "{w:08x?}");
    assert_eq!(halted(&s), [true; 3]);
}

#[test]
fn sync_switched_off_after_init_lets_the_cores_halt_alone() {
    let s = server("ocds sync tl1");
    s.interp.eval("ocds sync none").unwrap();
    s.interp.eval("halt 1000").unwrap();
    assert_eq!(halted(&s), [true, false, false]);
}

#[test]
fn routing_is_written_again_after_every_reset() {
    let s = server("ocds sync tl1");
    s.interp.eval("reset halt").unwrap();
    assert_eq!(with_sim(&s, |sim| sim.mem_word(CBS_TL1ST)), 0x3000_003F, "routing not restored after the debugger's reset");
    s.interp.eval("resume").unwrap();
    with_sim(&s, |sim| sim.schedule_board_reset(Duration::from_millis(5)));
    wait_polling(&s, "routing restored after the board reset", |s| {
        with_sim(s, |sim| sim.mem_word(CBS_TL1ST) == 0x3000_003F && sim.writes.iter().filter(|w| w.0 == CBS_TL1ST).count() >= 1)
    });
    s.interp.eval("halt 1000").unwrap();
    assert_eq!(halted(&s), [true; 3], "a pause after the board reset did not stop all cores");
}
