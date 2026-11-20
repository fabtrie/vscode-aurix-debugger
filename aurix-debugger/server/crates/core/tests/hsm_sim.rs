//! The server's HSM start after a reset (`hsm ...`) against the simulated TC37x: the board cfg and an
//! HSM target, no tc3xx scripts.

use std::path::PathBuf;
use std::time::Instant;

use aurix_core::cortex_m::{fp_comp, DHCSR};
use aurix_core::sim::SimAdapter;
use aurix_core::Server;

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
    let s = server(true, "ocds sync tl1; hsm ready_ms 300");
    s.interp.eval("init").unwrap();
    hsm_writes(&s);
    // The HSM reports ready right away: the latch comes after that, without waiting the 300 ms.
    with_sim(&s, |sim| sim.set_mem(HSM2HTS, 3));
    let t0 = Instant::now();
    s.interp.eval("reset halt").unwrap();
    assert!(t0.elapsed().as_millis() < 250, "waited although the HSM was ready");
    let w = hsm_writes(&s);
    assert!(w.contains(&(fp_comp(0), 0)), "boot ROM breakpoint not deleted: {w:08x?}");
    assert!(w.contains(&(DHCSR, 0xA05F_0001)), "HSM not run: {w:08x?}");
    assert!(with_sim(&s, |sim| sim.tl1_held), "TL1 not latched after the halting reset");
    assert_eq!(s.interp.eval("tc37x.cpu0 curstate").unwrap(), "halted");
}

/// The latch waits for the HSM to report ready: latched before, the HSM (halted by TL1) reports
/// ready only after continue, and a bootloader that checks once loops.
#[test]
fn halting_reset_waits_for_the_released_hsm_before_the_latch() {
    let s = server(true, "ocds sync tl1; hsm ready_ms 100");
    s.interp.eval("init").unwrap();
    let t0 = Instant::now();
    s.interp.eval("reset halt").unwrap();
    assert!(t0.elapsed().as_millis() >= 100, "latched without waiting for the HSM");
    let w = with_sim(&s, |sim| sim.writes.clone());
    let release = w.iter().rposition(|&(a, v)| a == 0xF005_EDF0 && v == 0xA05F_0001).expect("no release");
    let latch = w.iter().rposition(|&x| x == (0xF000_0490, 0x30)).expect("no latch");
    assert!(release < latch, "{w:08x?}");
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
    assert_eq!(s.interp.eval("hsm status").unwrap(), "boots 0 ssw_waits 0 target 1 last not_held");
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

/// Without an HSM target and with an HSM that does not boot (PROCONHSMCFG.HSMBOOTEN clear), the
/// HSM debug window is never touched.
#[test]
fn an_hsm_that_does_not_boot_is_never_accessed_without_a_target() {
    let s = server(false, "ocds sync tl1");
    with_sim(&s, |sim| sim.hsm_boots = false);
    s.interp.eval("init").unwrap();
    s.interp.eval("reset halt").unwrap();
    s.interp.eval("reset run").unwrap();
    assert_eq!(with_sim(&s, |sim| sim.hsm.dbgbase), 0, "HSM window selected");
    assert!(hsm_writes(&s).is_empty(), "HSM written");
    assert_eq!(s.interp.eval("hsm status").unwrap(), "boots 0 ssw_waits 0 target 0 last off");
    // TL1 suspends no HSM either.
    assert_eq!(with_sim(&s, |sim| sim.mem_word(0xF000_0494)) & (1 << 31), 0);
}

/// An HSM that boots is released after a halting reset without an HSM target (the bootloader waits
/// for it), and is not touched after a running reset, which does not leave it held.
#[test]
fn an_hsm_that_boots_is_released_without_a_target() {
    let s = server(false, "ocds sync tl1; hsm ready_ms 0");
    s.interp.eval("init").unwrap();
    hsm_writes(&s);
    s.interp.eval("reset halt").unwrap();
    let w = hsm_writes(&s);
    assert!(w.contains(&(fp_comp(0), 0)) && w.contains(&(DHCSR, 0xA05F_0001)), "not released: {w:08x?}");
    assert!(!with_sim(&s, |sim| sim.hsm.halted), "HSM still halted");
    assert_eq!(s.interp.eval("hsm status").unwrap(), "boots 1 ssw_waits 1 target 0 last released");
    assert!(!s.interp.eval("target names").unwrap().contains("hsm"));
    s.interp.eval("reset run").unwrap();
    assert!(!hsm_writes(&s).iter().any(|&(a, _)| a == DHCSR), "an HSM that was not held was written");
    assert_eq!(s.interp.eval("hsm status").unwrap(), "boots 1 ssw_waits 1 target 0 last not_held");
}

/// An HSM that boots but whose firmware closed its debug window cannot be released: reported as
/// blocked (with a warning after a halting reset), nothing written.
#[test]
fn an_hsm_that_boots_with_debugging_closed_is_reported() {
    let s = server(false, "ocds sync tl1");
    with_sim(&s, |sim| sim.hsm.debug_enabled = false);
    s.interp.eval("init").unwrap();
    s.interp.eval("reset halt").unwrap();
    assert_eq!(s.interp.eval("hsm status").unwrap(), "boots 1 ssw_waits 1 target 0 last blocked");
    assert!(!hsm_writes(&s).iter().any(|&(a, _)| a == DHCSR || a == fp_comp(0)));
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


/// After a halting reset the HSM thread is resumed (the frontend had it selected): through TL1 that
/// starts the TriCore cores too, and the suspend times count from that resume, not from the reset.
#[test]
fn resuming_the_hsm_after_a_halting_reset_counts_as_the_cores_resume() {
    let s = server(true, "ocds sync tl1; hsm ready_ms 0; suspend add CAN0");
    with_sim(&s, |sim| sim.app.as_mut().unwrap().enable_at.insert(0xF020_8000, Some(50)));
    s.interp.eval("init").unwrap();
    with_sim(&s, |sim| sim.set_mem(HSM2HTS, 3));
    s.interp.eval("reset halt").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(400));
    // The simulated TL1 latch holds the HSM without halting it: halted here, as the hardware's is.
    with_sim(&s, |sim| sim.hsm.halted = true);
    aurix_core::ops::poll_all(&s.cx());
    assert_eq!(s.interp.eval("tc37x.hsm curstate").unwrap(), "halted");
    assert_eq!(s.interp.eval("tc37x.cpu0 curstate").unwrap(), "halted");
    s.interp.eval("tc37x.hsm arp_resume").unwrap();
    let t0 = Instant::now();
    loop {
        aurix_core::ops::poll_all(&s.cx());
        if (with_sim(&s, |sim| sim.mem_word(0xF020_80E8)) >> 24) & 0xF == 2 {
            break;
        }
        assert!(t0.elapsed().as_millis() < 1000, "CAN0 not suspended after the resume");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let st = s.interp.eval("dict get [lindex [suspend status] 0] from").unwrap();
    assert_eq!(st, "resume");
    let ms: u64 = s.interp.eval("dict get [lindex [suspend status] 0] after_ms").unwrap().parse().unwrap();
    assert!(ms < 300, "counted from the reset: {ms} ms");
}
