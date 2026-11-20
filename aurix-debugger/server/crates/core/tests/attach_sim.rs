//! The server's attach modes (`attach_mode`, `halt_check_hook`, `halt_all`) against the simulated
//! TC37x, with the OpenOCD board cfg only: no tc3xx or SBC scripts, as on a board that needs nothing
//! else.

use std::path::PathBuf;

use aurix_core::sim::SimAdapter;
use aurix_core::Server;

const STM0_OCS: u32 = 0xF000_10E8;

/// The board cfg, TL1 sync and the STMs suspended; `settings` before `init`. Resets are counted
/// through a `reset_hook` (`::resets`).
fn server(settings: &str) -> Server {
    aurix_core::log::set_debug_level(1);
    let s = Server::new();
    s.cx().with(|c| {
        c.adapter_override = Some("sim".into());
        c.search_dirs.push(PathBuf::from(openocd_tcl::TCL_DIR));
    });
    s.interp.eval_global("script board/infineon/kit_a2g_tc375_lite.cfg").unwrap();
    s.interp
        .eval_global("ocds sync tl1; suspend add cores; set ::resets 0; proc count_reset {mode} { incr ::resets; reset $mode }; reset_hook count_reset")
        .unwrap();
    s.interp.eval_global(settings).unwrap();
    s.interp.eval("init").unwrap();
    s
}

fn with_sim<R>(s: &Server, f: impl FnOnce(&mut SimAdapter) -> R) -> R {
    s.cx().with(|c| f(c.adapter.as_mut().unwrap().as_any_mut().downcast_mut::<SimAdapter>().unwrap()))
}

fn resets(s: &Server) -> String {
    s.interp.eval("set ::resets").unwrap()
}

fn state(s: &Server) -> String {
    s.interp.eval("tc37x.cpu0 curstate").unwrap()
}

#[test]
fn passive_writes_nothing_and_refuses_resets_and_halts() {
    let s = server("attach_mode passive");
    assert_eq!(with_sim(&s, |sim| sim.writes.clone()), vec![]);
    assert_eq!(with_sim(&s, |sim| sim.ostate & 1), 0, "OCDS enabled");
    assert!(s.interp.eval("mdw 0x70000000").is_ok(), "memory reads work");
    assert!(s.interp.eval("reset run").unwrap_err().contains("passive"));
    assert!(s.interp.eval("halt_all").unwrap_err().contains("passive"));
    aurix_core::ops::poll_all(&s.cx());
    assert_eq!(with_sim(&s, |sim| sim.writes.clone()), vec![]);
    assert!(s.interp.eval("attach_mode hot").is_err(), "the mode is set before init");
}

#[test]
fn hot_attach_neither_resets_nor_halts_and_reports_locked_modules() {
    let s = server("");
    assert_eq!(resets(&s), "0");
    assert_eq!(with_sim(&s, |sim| sim.halted), [false; 3]);
    let st = s.interp.eval("attach_status").unwrap();
    assert!(st.contains("mode hot") && st.contains("locked {STM0 STM1}"), "{st}");
}

#[test]
fn halt_attach_resets_first_when_modules_are_locked() {
    let s = server("attach_mode halt");
    assert_eq!(resets(&s), "1", "no reset before the halt");
    assert_eq!(state(&s), "halted");
    assert_eq!(with_sim(&s, |sim| sim.halted), [true; 3], "not all cores halted together");
    assert_eq!((with_sim(&s, |sim| sim.mem_word(STM0_OCS)) >> 24) & 0xF, 2, "STM0 not suspended by the reset");
    // In the application, after its start-up (it locked STM0 again), not at the reset vector.
    assert_eq!(with_sim(&s, |sim| sim.mem_word(0xF000_10FC)), 0x4);
}

#[test]
fn halt_attach_without_a_reason_only_halts() {
    // No STM suspend wanted: nothing is locked.
    let s = server("suspend clear; attach_mode halt");
    assert_eq!(resets(&s), "0");
    assert_eq!(with_sim(&s, |sim| sim.halted), [true; 3]);
}

#[test]
fn halt_check_hook_makes_the_halt_attach_reset_first() {
    let s = server("suspend clear; proc board_check {} { return \"the board says so\" }; halt_check_hook board_check; attach_mode halt");
    assert_eq!(resets(&s), "1");
    assert_eq!(state(&s), "halted");
    assert!(s.interp.eval("attach_status").unwrap().contains("the board says so"));
}

#[test]
fn reset_modes_go_through_the_reset_hook() {
    let s = server("attach_mode reset_halt");
    assert_eq!(resets(&s), "1");
    assert_eq!(state(&s), "halted");
    assert_eq!(with_sim(&s, |sim| sim.pc[0]), 0x800A_8000, "not at the reset vector");
    let s = server("attach_mode reset_run");
    assert_eq!(resets(&s), "1");
    assert_eq!(with_sim(&s, |sim| sim.halted[0]), false);
}

#[test]
fn halt_all_halts_every_core_without_tl1_sync_too() {
    let s = server("ocds sync none");
    assert_eq!(s.interp.eval("halt_all").unwrap(), "1");
    assert_eq!(with_sim(&s, |sim| sim.halted), [true; 3]);
    assert_eq!(state(&s), "halted");
}
