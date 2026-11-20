//! The server's HSM start after a reset (`hsm ...`) against the simulated TC37x: the board cfg and an
//! HSM target, no tc3xx scripts.

use std::path::PathBuf;
use std::time::Instant;

use aurix_core::cortex_m::{fp_comp, DHCSR};
use aurix_core::sim::SimAdapter;
use aurix_core::Server;

const HSM_DBGBASE: u32 = 0xF004_1010;
const HSM2HTS: u32 = 0xF004_0034;
const CBS_TRHSM: u32 = 0xF000_0438;

fn server(hsm: bool, settings: &str) -> Server {
    aurix_core::log::set_debug_level(1);
    let s = Server::new();
    s.cx().with(|c| {
        c.adapter_override = Some("sim".into());
        c.search_dirs.push(PathBuf::from(openocd_tcl::TCL_DIR));
    });
    s.interp.eval_global("script board/infineon/kit_a2g_tc375_lite.cfg").unwrap();
    if hsm {
        s.interp.eval_global("target create tc37x.hsm cortex_m -coreid 6 -ocmts [tc37x.cpu0 cget -ocmts]; targets tc37x.cpu0").unwrap();
    }
    s.interp.eval_global(settings).unwrap();
    s
}

fn with_sim<R>(s: &Server, f: impl FnOnce(&mut SimAdapter) -> R) -> R {
    s.cx().with(|c| f(c.adapter.as_mut().unwrap().as_any_mut().downcast_mut::<SimAdapter>().unwrap()))
}

fn hsm_writes(s: &Server) -> Vec<(u32, u32)> {
    with_sim(s, |sim| std::mem::take(&mut sim.hsm.writes))
}

#[test]
fn halting_reset_releases_the_hsm_then_latches_tl1() {
    let s = server(true, "ocds sync tl1");
    s.interp.eval("init").unwrap();
    hsm_writes(&s);
    s.interp.eval("reset halt").unwrap();
    let w = hsm_writes(&s);
    assert!(w.contains(&(fp_comp(0), 0)), "boot ROM breakpoint not deleted: {w:08x?}");
    assert!(w.contains(&(DHCSR, 0xA05F_0001)), "HSM not run: {w:08x?}");
    assert!(with_sim(&s, |sim| sim.tl1_held), "TL1 not latched after the halting reset");
    assert_eq!(s.interp.eval("tc37x.cpu0 curstate").unwrap(), "halted");
}

#[test]
fn autostart_off_leaves_the_hsm_at_its_boot_breakpoint() {
    let s = server(true, "hsm autostart off");
    s.interp.eval("init").unwrap();
    hsm_writes(&s);
    s.interp.eval("reset halt").unwrap();
    assert!(!hsm_writes(&s).contains(&(fp_comp(0), 0)));
    assert!(with_sim(&s, |sim| sim.hsm.halted));
}

#[test]
fn an_hsm_that_does_not_boot_is_left_alone_without_waiting() {
    let s = server(true, "ocds sync tl1");
    with_sim(&s, |sim| sim.hsm_boots = false);
    s.interp.eval("init").unwrap();
    hsm_writes(&s);
    let t0 = Instant::now();
    s.interp.eval("reset halt").unwrap();
    s.interp.eval("reset run").unwrap();
    assert!(t0.elapsed().as_millis() < 1000, "waited for an HSM that does not boot");
    assert!(!hsm_writes(&s).contains(&(fp_comp(0), 0)), "released an HSM that was not held");
    assert!(!with_sim(&s, |sim| sim.writes.contains(&(CBS_TRHSM, 0))), "HSM unrouted from TL1 although not halted");
}

#[test]
fn an_hsm_with_debugging_closed_is_left_alone() {
    let s = server(true, "ocds sync tl1");
    with_sim(&s, |sim| sim.hsm.debug_enabled = false);
    s.interp.eval("init").unwrap();
    let t0 = Instant::now();
    s.interp.eval("reset halt").unwrap();
    assert!(t0.elapsed().as_millis() < 1000);
    assert!(!with_sim(&s, |sim| sim.writes.contains(&(CBS_TRHSM, 0))), "unrouted: the release was not even possible");
}

#[test]
fn without_an_hsm_target_the_hsm_is_never_accessed() {
    let s = server(false, "ocds sync tl1");
    s.interp.eval("init").unwrap();
    s.interp.eval("reset halt").unwrap();
    s.interp.eval("reset run").unwrap();
    let w = with_sim(&s, |sim| sim.writes.clone());
    assert!(!w.iter().any(|&(a, _)| a == HSM_DBGBASE || a == CBS_TRHSM), "HSM accessed without an HSM target: {w:08x?}");
    // TL1 suspends no HSM either.
    assert_eq!(with_sim(&s, |sim| sim.mem_word(0xF000_0494)) & (1 << 31), 0);
}

#[test]
fn wait_ready_follows_hsm2hts() {
    let s = server(true, "");
    s.interp.eval("init").unwrap();
    let t0 = Instant::now();
    assert_eq!(s.interp.eval("hsm wait_ready 50").unwrap(), "0");
    assert!(t0.elapsed().as_millis() >= 50);
    with_sim(&s, |sim| sim.set_mem(HSM2HTS, 3));
    assert_eq!(s.interp.eval("hsm wait_ready 50").unwrap(), "1");
}
