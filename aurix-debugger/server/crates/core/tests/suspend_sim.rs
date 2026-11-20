//! The server's peripheral suspend (`suspend ...`) against the simulated TC37x, with the OpenOCD board
//! cfg only: no tc3xx or SBC scripts, as on a board that needs nothing else.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use aurix_core::sim::SimAdapter;
use aurix_core::{ops, Server};

const STM0_OCS: u32 = 0xF000_10E8;
const STM1_OCS: u32 = 0xF000_11E8;
const STM0_ACCEN0: u32 = 0xF000_10FC;
const EVADC_OCS: u32 = 0xF002_0028;
const CAN0_OCS: u32 = 0xF020_80E8;
const CAN0_CLC: u32 = 0xF020_8000;
const CAN1_OCS: u32 = 0xF021_80E8;
const CAN1_CLC: u32 = 0xF021_8000;
const DBGSR0: u32 = 0xF881_FD00;

fn server(settings: &str) -> Server {
    aurix_core::log::set_debug_level(1);
    let s = Server::new();
    s.cx().with(|c| {
        c.adapter_override = Some("sim".into());
        c.search_dirs.push(PathBuf::from(openocd_tcl::TCL_DIR));
    });
    s.interp.eval_global("script board/infineon/kit_a2g_tc375_lite.cfg").unwrap();
    s.interp.eval_global(settings).unwrap();
    s
}

fn with_sim<R>(s: &Server, f: impl FnOnce(&mut SimAdapter) -> R) -> R {
    s.cx().with(|c| f(c.adapter.as_mut().unwrap().as_any_mut().downcast_mut::<SimAdapter>().unwrap()))
}

fn sus(s: &Server, ocs: u32) -> u32 {
    (with_sim(s, |sim| sim.mem_word(ocs)) >> 24) & 0xF
}

fn writes_to(s: &Server, addr: u32) -> usize {
    with_sim(s, |sim| sim.writes.iter().filter(|w| w.0 == addr).count())
}

fn no_violations(s: &Server) {
    let v = with_sim(s, |sim| sim.violations.clone());
    assert!(v.is_empty(), "OCS writes the hardware refuses (they reset the board): {v:08x?}");
}

/// The module's state as `suspend status` reports it.
fn state(s: &Server, name: &str) -> String {
    let list = s.interp.eval("suspend status").unwrap();
    let items = s.interp.eval(&format!("lmap m {{{list}}} {{ if {{[dict get $m name] eq \"{name}\"}} {{ dict get $m state }} else continue }}")).unwrap();
    items.trim().to_string()
}

/// Polls as the server loop does (every 5 ms) until `f` holds, for up to `ms`.
fn wait_polling(s: &Server, what: &str, ms: u64, f: impl Fn(&Server) -> bool) {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(ms) {
        ops::poll_all(&s.cx());
        if f(s) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {what}");
}

fn running(s: &Server) -> bool {
    s.interp.eval("tc37x.cpu0 curstate").unwrap() == "running"
}

#[test]
fn module_names_are_resolved_from_the_svd_description() {
    let s = server("suspend add CAN0; suspend add cores; suspend add gpt120 hard; suspend add GTM off");
    let list = s.interp.eval("suspend list").unwrap();
    assert!(list.contains("{CAN0 soft 0xF02080E8 0xF0208000 0xF02080FC}"), "{list}");
    assert!(list.contains("{STM0 soft 0xF00010E8 0xF0001000 0xF00010FC}"), "{list}");
    assert!(list.contains("{STM2 soft"), "{list}");
    assert!(list.contains("{GPT120 hard 0xF00018E8"), "{list}");
    assert!(list.contains("{GTM off 0xF019FD38 0xF019FD00 0xF019FD10}"), "{list}");
    let e = s.interp.eval("suspend add NOSUCH").unwrap_err();
    assert!(e.contains("no such module"), "{e}");
    let e = s.interp.eval("suspend add DMA").unwrap_err();
    assert!(e.contains("DMA.OCS"), "{e}");
    s.interp.eval("suspend add RAW -ocs 0xF0001EE8 -clc 0xF0001E00 -accen 0xF0001EFC").unwrap();
    assert!(s.interp.eval("suspend list").unwrap().contains("{RAW soft 0xF0001EE8 0xF0001E00 0xF0001EFC}"));
    s.interp.eval("suspend remove CAN0").unwrap();
    assert!(!s.interp.eval("suspend list").unwrap().contains("CAN0"));
}

#[test]
fn nothing_is_written_without_a_suspend_list() {
    let s = server("");
    s.interp.eval("init").unwrap();
    s.interp.eval("reset run").unwrap();
    wait_polling(&s, "the application's start-up", 500, |s| with_sim(s, |sim| sim.mem_word(STM0_ACCEN0)) == 0x4);
    for &(ocs, ..) in aurix_core::sim::SIM_PERIPHS {
        assert_eq!(writes_to(&s, ocs), 0, "OCS 0x{ocs:08x} written without being listed");
    }
}

/// Hot attach to an application that locked STM0/1: they are not written (locked); after a
/// debugger reset they get their suspend before the application locks them again.
#[test]
fn locked_modules_are_left_alone_until_a_reset_suspends_them_in_time() {
    let s = server("suspend add cores");
    s.interp.eval("init").unwrap();
    assert_eq!(writes_to(&s, STM0_OCS), 0, "locked STM0 written at the attach");
    assert_eq!(state(&s, "STM0"), "locked");
    assert_eq!(state(&s, "STM2"), "done", "STM2 is not locked by the application");
    assert_eq!(s.interp.eval("suspend pending locked").unwrap(), "STM0 STM1");
    s.interp.eval("reset run").unwrap();
    assert_eq!(sus(&s, STM0_OCS), 2);
    assert_eq!(sus(&s, STM1_OCS), 2);
    assert_eq!(state(&s, "STM0"), "done");
    wait_polling(&s, "the application locks STM0 again", 500, |s| with_sim(s, |sim| sim.mem_word(STM0_ACCEN0)) == 0x4);
    assert!(running(&s));
    no_violations(&s);
}

/// A module the application enables late gets its suspend from the poll, without a halt; one it
/// never enables is never written.
#[test]
fn modules_enabled_later_are_suspended_by_the_poll_without_a_halt() {
    let s = server("suspend add CAN0; suspend add CAN1; suspend add EVADC");
    with_sim(&s, |sim| {
        let app = sim.app.as_mut().unwrap();
        app.enable_at.insert(CAN0_CLC, Some(150));
        app.enable_at.insert(CAN1_CLC, None);
    });
    // At the attach the application has been running with all its modules on (as the simulated
    // board starts): what counts is after the reset.
    s.interp.eval("init").unwrap();
    with_sim(&s, |sim| sim.writes.clear());
    s.interp.eval("reset run").unwrap();
    let t0 = Instant::now();
    wait_polling(&s, "EVADC suspended", 1000, |s| sus(s, EVADC_OCS) == 2);
    assert_eq!(sus(&s, CAN0_OCS), 0, "CAN0 is not enabled before 150 ms");
    wait_polling(&s, "CAN0 suspended", 1000, |s| sus(s, CAN0_OCS) == 2);
    assert!(t0.elapsed() >= Duration::from_millis(100), "CAN0 suspended before the application enabled it");
    assert!(running(&s), "the cores were halted");
    assert_eq!(state(&s, "CAN1"), "pending");
    assert_eq!(writes_to(&s, CAN1_OCS), 0, "CAN1 written while disabled");
    no_violations(&s);
}

/// A reset the debugger did not cause (a supply or SBC reset) while the cores run: the reset
/// modules are suspended again at once, the application's modules once it enables them.
#[test]
fn board_reset_while_running_suspends_everything_again_without_a_halt() {
    let s = server("suspend add cores; suspend add EVADC");
    s.interp.eval("init").unwrap();
    s.interp.eval("reset run").unwrap();
    wait_polling(&s, "EVADC suspended", 1000, |s| sus(s, EVADC_OCS) == 2);
    with_sim(&s, |sim| sim.schedule_board_reset(Duration::from_millis(5)));
    wait_polling(&s, "the board reset cleared OCS", 1000, |s| sus(s, EVADC_OCS) == 0 || sus(s, STM0_OCS) == 2 && state(s, "EVADC") == "pending");
    wait_polling(&s, "EVADC suspended again", 1000, |s| sus(s, EVADC_OCS) == 2);
    assert_eq!(sus(&s, STM0_OCS), 2, "STM0 not suspended again after the board reset");
    assert!(running(&s));
    no_violations(&s);
}

/// The debugger's pause writes what is pending before the halt request.
#[test]
fn pause_suspends_pending_modules_before_the_halt() {
    let s = server("");
    s.interp.eval("init").unwrap();
    s.interp.eval("suspend add CAN0").unwrap();
    with_sim(&s, |sim| sim.writes.clear());
    s.interp.eval("halt").unwrap();
    let w = with_sim(&s, |sim| sim.writes.clone());
    let ocs = w.iter().position(|x| x.0 == CAN0_OCS).expect("CAN0 not written at the pause");
    let halt = w.iter().position(|x| *x == (DBGSR0, 0x6)).expect("no halt request");
    assert!(ocs < halt, "suspend after the halt request: {w:08x?}");
    no_violations(&s);
}

/// A write that does not take is tried at most twice per reset.
#[test]
fn a_write_that_does_not_take_is_given_up() {
    // OCS at STM0.TIM0, which the simulated timer overwrites: the read-back never shows the suspend.
    let s = server("suspend add ODD -ocs 0xF0001010 -clc 0xF0020000 -accen 0xF002003C");
    s.interp.eval("init").unwrap();
    for _ in 0..4 {
        s.interp.eval("suspend now").unwrap();
    }
    assert_eq!(writes_to(&s, 0xF000_1010), 2);
    assert_eq!(state(&s, "ODD"), "refused");
}
