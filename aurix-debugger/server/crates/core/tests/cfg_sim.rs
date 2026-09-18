//! Loads the real OpenOCD board cfg and tc37x_ocds_init.cfg (with tc3xx_ocds.cfg) against the
//! simulated TC37x and checks the event-driven register sequences.

use std::path::PathBuf;

use aurix_core::sim::SimAdapter;
use aurix_core::{ops, Server};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize().unwrap()
}

fn tcl_path(p: PathBuf) -> String {
    p.to_string_lossy().trim_start_matches(r"\\?\").replace('\\', "/")
}

fn server() -> Server {
    aurix_core::log::set_debug_level(1);
    let s = Server::new();
    s.cx().with(|c| {
        c.adapter_override = Some("sim".into());
        c.search_dirs.push(repo().join("third_party/openocd/tcl"));
    });
    s.interp.eval_global("script board/infineon/kit_a2g_tc377tx_3v3_trb.cfg").unwrap();
    s.interp
        .eval_global(&format!("script {}", jim::quote(&tcl_path(repo().join("tc37x_ocds_init.cfg")))))
        .unwrap();
    // The simulated HSM never reports ready: don't wait for it after halting resets.
    s.interp.eval_global("set tc3xx_hsm_ready_ms 0").unwrap();
    s
}

fn with_sim<R>(s: &Server, f: impl FnOnce(&mut SimAdapter) -> R) -> R {
    s.cx().with(|c| f(c.adapter.as_mut().unwrap().as_any_mut().downcast_mut::<SimAdapter>().unwrap()))
}

fn take_writes(s: &Server) -> Vec<(u32, u32)> {
    with_sim(s, |sim| std::mem::take(&mut sim.writes))
}

const DBGSR: [u32; 3] = [0xF881_FD00, 0xF883_FD00, 0xF885_FD00];
const CBS_TLC: u32 = 0xF000_0490;
const CBS_TLCHS: u32 = 0xF000_049C;

fn pos(w: &[(u32, u32)], item: (u32, u32)) -> usize {
    w.iter().position(|x| *x == item).unwrap_or_else(|| panic!("write {item:08x?} not found in {w:08x?}"))
}

#[test]
fn loads_real_cfg_and_inits() {
    let s = server();
    let i = &s.interp;
    let names = i.eval("target names").unwrap();
    assert!(names.starts_with("tc37x.cpu0 tc37x.cpu1 tc37x.cpu2 tc37x.hsm tc37x.mcs0.ch0 tc37x.mcs0.ch1"), "{names}");
    assert!(names.ends_with("tc37x.mcs1.ch7"), "{names}");
    assert_eq!(i.eval("target current").unwrap(), "tc37x.cpu0");
    // pfls0/1 from the target cfg, dfls0/1 from tc37x_ocds_init.cfg.
    assert_eq!(i.eval("flash banks").unwrap().lines().count(), 4);
    assert_eq!(i.eval("tc37x.cpu1 cget -dbgbase").unwrap(), "0xf8820000");
    assert_eq!(i.eval("tc37x.cpu0 cget -event examine-end").unwrap().trim(), "tc3xx_on_examine");
    i.eval("init").unwrap();
    let w = take_writes(&s);
    assert!(w.contains(&(0xF000_0494, 0xB000_003F)), "CBS_TL1ST not written: {w:08x?}");
    assert!(w.contains(&(0xF000_0498, 2)), "CBS_TLCHE not written");
    assert!(w.contains(&(0xF000_0420, 0x100)), "CBS_TRC0 not written");
    assert_eq!(i.eval("tc37x.cpu2 curstate").unwrap(), "running");
    assert_eq!(i.eval("tc37x.cpu1 tricore tl1_halt").unwrap(), "tl1_halt on");
    assert_eq!(i.eval("tc37x.cpu2 tricore tl1_step").unwrap(), "tl1_step on");
    assert!(i.eval("targets").unwrap().contains("tc37x.cpu0"));
}

#[test]
fn halt_and_resume_follow_cfg_event_order() {
    let s = server();
    s.interp.eval("init").unwrap();
    take_writes(&s);

    s.interp.eval("halt 1000").unwrap();
    let w = take_writes(&s);
    // tl1_halt pulse, then the DBGSR halt request. In "run" mode TL1 alone stops the STMs:
    // the halted handler asserts no suspend-out.
    let p_tlc = pos(&w, (CBS_TLC, 0x30));
    let p_rel = pos(&w, (CBS_TLC, 0));
    let p_halt = pos(&w, (DBGSR[0], 0x6));
    assert!(p_tlc < p_rel && p_rel < p_halt);
    for d in DBGSR {
        assert!(!w.contains(&(d, 0x10)), "SUSP asserted for {d:08x} in run mode: {w:08x?}");
    }
    assert_eq!(s.interp.eval("tc37x.cpu0 curstate").unwrap(), "halted");

    s.interp.eval("resume").unwrap();
    let w = take_writes(&s);
    let p1 = pos(&w, (DBGSR[1], 0x4));
    let p2 = pos(&w, (DBGSR[2], 0x4));
    let p0 = pos(&w, (DBGSR[0], 0x4));
    // resume-end: suspend-outs cleared while TL1 still holds the cores, then the hold is dropped.
    let p_clear = pos(&w, (DBGSR[0], 0));
    let pend = pos(&w, (CBS_TLCHS, 2));
    assert!(p1 < p0 && p2 < p0 && p0 < p_clear && p_clear < pend, "resume-start / resume / resume-end order wrong: {w:08x?}");
    assert_eq!(s.interp.eval("tc37x.cpu0 curstate").unwrap(), "running");
}

#[test]
fn frozen_step_timers_keep_suspend_out_while_halted_and_stepping() {
    let s = server();
    s.interp.eval("init").unwrap();
    assert_eq!(s.interp.eval("tc3xx_step_timers frozen").unwrap(), "frozen");
    s.interp.eval("halt 1000").unwrap();
    let w = take_writes(&s);
    for d in DBGSR {
        assert!(w.contains(&(d, 0x10)), "SUSP for {d:08x} missing after halt: {w:08x?}");
    }
    s.interp.eval("step").unwrap();
    let w = take_writes(&s);
    let others = pos(&w, (DBGSR[1], 0x14));
    let own = pos(&w, (DBGSR[0], 0x10));
    let evt = pos(&w, (0xF881_F038, 0x1022)); // step trigger keeps SUSP=1
    let release = pos(&w, (DBGSR[0], 0x14));
    assert!(others < own && own < evt && evt < release, "{w:08x?}");

    s.interp.eval("resume").unwrap();
    let w = take_writes(&s);
    let p_clear = pos(&w, (DBGSR[0], 0));
    let pend = pos(&w, (CBS_TLCHS, 2));
    assert!(p_clear < pend, "suspend-out cleared after the TL1 hold was dropped: {w:08x?}");
    assert!(s.interp.eval("catch {tc3xx_step_timers slow}").unwrap() == "1");
}

#[test]
fn step_uses_non_bbm_pc_trigger() {
    let s = server();
    s.interp.eval("init").unwrap();
    s.interp.eval("halt 1000").unwrap();
    let pc: u32 = with_sim(&s, |sim| sim.pc[0]);
    take_writes(&s);
    s.interp.eval("step").unwrap();
    let w = take_writes(&s);
    // tl1_step: TL1 latched first (the sim reports it inactive), step-start releases the other
    // cores under the hold and sets every SUSP for "run" mode (clear), the core releases cpu0 and
    // then clears the hold.
    let latch = pos(&w, (CBS_TLC, 0x30));
    let others = pos(&w, (DBGSR[1], 0x4));
    let own = pos(&w, (DBGSR[0], 0));
    // TR7EVT: PC trigger, no BBM, EVTA=halt, SUSP=0 (TL1 stops the STMs with the cores)
    let evt = pos(&w, (0xF881_F038, 0x1002));
    let release = pos(&w, (DBGSR[0], 0x4));
    let start = pos(&w, (CBS_TLCHS, 2));
    assert!(latch < others && others < own && own < evt && evt < release && release < start, "wrong order: {w:08x?}");
    assert!(!w.contains(&(DBGSR[1], 0x16)), "other core put into debug halt: {w:08x?}");
    assert!(w.contains(&(0xF881_F03C, pc)));
    let (state, reason) = ops::state(&s.cx(), 0);
    assert_eq!(state.name(), "halted");
    assert_eq!(reason.name(), "single-step");
}

#[test]
fn step_without_tl1_step_runs_only_the_stepping_core() {
    let s = server();
    s.interp.eval("init").unwrap();
    s.interp.eval("halt 1000").unwrap();
    s.interp.eval("tc37x.cpu0 tricore tl1_step off").unwrap();
    take_writes(&s);
    s.interp.eval("step").unwrap();
    let w = take_writes(&s);
    // The cfg's step-start keeps the other cores in debug halt and clears the hold itself.
    let hold = pos(&w, (DBGSR[1], 0x16));
    let clear = pos(&w, (CBS_TLCHS, 2));
    let release = pos(&w, (DBGSR[0], 0x14));
    assert!(hold < clear && clear < release, "{w:08x?}");
    assert!(!w.contains(&(CBS_TLC, 0x30)), "TL1 latched although tl1_step is off");
}

const ICR0: u32 = 0xF881_FE2C;

/// ICR of cpu0 as stored on the (simulated) chip, bypassing the register view.
fn icr_hw(s: &Server) -> u32 {
    let v = s.interp.eval(&format!("read_memory {ICR0:#x} 32 1")).unwrap();
    u32::from_str_radix(v.trim_start_matches("0x"), 16).unwrap()
}

#[test]
fn irq_mask_clears_ie_while_stepping_and_restores_it_on_resume() {
    let s = server();
    s.interp.eval("init").unwrap();
    assert_eq!(s.interp.eval("tc37x.cpu0 tricore irq_mask").unwrap(), "irq_mask step step_over step_out");
    s.interp.eval("halt 1000").unwrap();
    s.interp.eval(&format!("mww {ICR0:#x} 0x8000")).unwrap();

    s.interp.eval("step").unwrap();
    assert_eq!(icr_hw(&s), 0, "IE not masked on the chip during/after the step");
    // Register reads show the application's value.
    assert_eq!(ops::read_reg(&s.cx(), 0, aurix_core::arch::REG_ICR).unwrap(), 0x8000);

    s.interp.eval("resume").unwrap();
    assert_eq!(icr_hw(&s), 0x8000, "IE not restored on resume");

    s.interp.eval("tc37x.cpu0 tricore irq_mask off").unwrap();
    s.interp.eval("halt 1000").unwrap();
    s.interp.eval("step").unwrap();
    assert_eq!(icr_hw(&s), 0x8000, "IE masked although irq_mask is off");
}

#[test]
fn irq_mask_carries_a_stepped_disable_over_to_the_application() {
    let s = server();
    s.interp.eval("init").unwrap();
    s.interp.eval("halt 1000").unwrap();
    s.interp.eval(&format!("mww {ICR0:#x} 0x8000")).unwrap();
    let pc = with_sim(&s, |sim| sim.pc[0]);
    s.interp.eval(&format!("mww {pc:#x} 0x0340000d")).unwrap(); // disable
    s.interp.eval("step").unwrap();
    assert_eq!(ops::read_reg(&s.cx(), 0, aurix_core::arch::REG_ICR).unwrap(), 0);
    s.interp.eval("resume").unwrap();
    assert_eq!(icr_hw(&s), 0, "a stepped 'disable' was undone on resume");
}

#[test]
fn breakpoint_hit_is_reported_as_breakpoint() {
    let s = server();
    s.interp.eval("init").unwrap();
    s.interp.eval("halt 1000").unwrap();
    s.interp.eval("bp 0x80001234 4 hw").unwrap();
    assert!(s.interp.eval("bp").unwrap().contains("0x80001234"));
    s.interp.eval("resume").unwrap();
    s.interp.eval("wait_halt 1000").unwrap();
    let (_, reason) = ops::state(&s.cx(), 0);
    assert_eq!(reason.name(), "breakpoint");
    assert_eq!(s.interp.eval("reg pc").unwrap(), "pc (/32): 0x80001234");
    s.interp.eval("rbp all").unwrap();
    assert_eq!(s.interp.eval("bp").unwrap(), "");
}

#[test]
fn external_reset_reexamines_and_reruns_examine_end() {
    let s = server();
    s.interp.eval("init").unwrap();
    take_writes(&s);
    with_sim(&s, |sim| sim.external_reset());
    let cx = s.cx();
    ops::poll_all(&cx);
    let w = take_writes(&s);
    assert!(w.contains(&(0xF000_0494, 0xB000_003F)), "examine-end handler did not run again");
    assert_eq!(with_sim(&s, |sim| sim.ostate & 1), 1, "OCDS not re-enabled");
    assert_eq!(s.interp.eval("tc37x.cpu0 was_examined").unwrap(), "1");
}

#[test]
fn boot_halted_core_does_not_fire_halted_handler() {
    let s = server();
    with_sim(&s, |sim| sim.bhalt[2] = true);
    s.interp.eval("init").unwrap();
    let w = take_writes(&s);
    assert!(
        !w.iter().any(|&(a, v)| DBGSR.contains(&a) && v == 0x10),
        "SUSP asserted because of a boot-halted core: {w:08x?}"
    );
    assert_eq!(s.interp.eval("tc37x.cpu2 curstate").unwrap(), "halted");
    let (_, reason) = ops::state(&s.cx(), 2);
    assert_eq!(reason.name(), "boot-halt");
    // A real debug halt of cpu0 still runs the handler (visible through its SUSP writes in "frozen").
    s.interp.eval("tc3xx_step_timers frozen").unwrap();
    s.interp.eval("halt 1000").unwrap();
    let w = take_writes(&s);
    assert!(w.contains(&(DBGSR[2], 0x10)));
}

#[test]
fn memory_commands() {
    let s = server();
    s.interp.eval("init").unwrap();
    s.interp.eval("mww 0x70000000 0x12345678 2").unwrap();
    assert_eq!(s.interp.eval("mdw 0x70000000 2").unwrap(), "0x70000000: 12345678 12345678 ");
    assert_eq!(s.interp.eval("read_memory 0x70000004 32 1").unwrap(), "0x12345678");
    s.interp.eval("write_memory 0x70000008 32 {0xAABBCCDD 1}").unwrap();
    assert_eq!(s.interp.eval("read_memory 0x70000008 32 2").unwrap(), "0xaabbccdd 0x1");
    let e = s.interp.eval("catch {mdw} err; set err").unwrap();
    assert!(e.contains("usage"), "{e}");
}

#[test]
fn reset_halt_runs_reset_sequence() {
    let s = server();
    s.interp.eval("init").unwrap();
    s.interp.eval("reset halt").unwrap();
    assert_eq!(s.interp.eval("tc37x.cpu0 curstate").unwrap(), "halted");
    assert_eq!(s.interp.eval("reg pc").unwrap(), "pc (/32): 0x800a8000");
}

#[test]
fn connect_reset_halt_leaves_cores_halted_with_setup() {
    let s = server();
    s.interp.eval("init").unwrap();
    take_writes(&s);
    s.interp.eval("tc3xx_connect_reset_halt").unwrap();
    let w = take_writes(&s);
    assert_eq!(s.interp.eval("tc37x.cpu0 curstate").unwrap(), "halted");
    assert!(w.contains(&(0xF000_10E8, 0x1200_0000)), "STM suspend not applied: {w:08x?}");
}

#[test]
fn debugger_reset_reapplies_setup_and_latches_tl1_when_halted() {
    let s = server();
    s.interp.eval("init").unwrap();
    take_writes(&s);
    s.interp.eval("reset halt").unwrap();
    let w = take_writes(&s);
    assert!(w.contains(&(0xF000_0494, 0xB000_003F)), "setup not redone after reset halt: {w:08x?}");
    // The last TL1 force (after the reset's own halt pulse) latches TL1 after the STM suspend setup.
    let ocs = pos(&w, (0xF000_10E8, 0x1200_0000));
    let latch = w.iter().rposition(|x| *x == (CBS_TLC, 0x30)).expect("TL1 not latched after reset halt");
    assert!(ocs < latch, "TL1 must be latched after the STM suspend setup: {w:08x?}");
    // After the TL1 latch (whose edge would halt the HSM again) the boot ROM's HSM breakpoint is
    // deleted and the HSM runs. (The HSM target also accesses DBGBASE earlier, when it is examined
    // after the reset.)
    let dbgbase = latch + pos(&w[latch..], (0xF004_1010, 0xE000_0000));
    let bp = pos(&w, (0xF005_2008, 0));
    let run = pos(&w, (0xF005_EDF0, 0xA05F_0001));
    assert!(latch < dbgbase && dbgbase < bp && bp < run, "HSM release order: {w:08x?}");
    assert_eq!(s.interp.eval("tc37x.cpu0 curstate").unwrap(), "halted");

    s.interp.eval("reset run").unwrap();
    let w = take_writes(&s);
    assert!(w.contains(&(0xF000_0494, 0xB000_003F)), "setup not redone after reset run: {w:08x?}");
    assert!(!w.contains(&(CBS_TLC, 0x30)), "TL1 latched after reset run: {w:08x?}");
}

// --- HSM (Cortex-M3) --------------------------------------------------------------------------

const HSM: usize = 3;
const HSM_DBGBASE: u32 = 0xF004_1010;
const HSM_DHCSR: u32 = 0xF005_EDF0;

#[test]
fn hsm_is_examined_through_the_debug_window() {
    let s = server();
    let i = &s.interp;
    assert_eq!(i.eval("tc37x.hsm cget -type").unwrap(), "cortex_m");
    assert_eq!(i.eval("tc37x.hsm cget -coreid").unwrap(), "6");
    i.eval("init").unwrap();
    assert_eq!(i.eval("tc37x.hsm was_examined").unwrap(), "1");
    assert_eq!(i.eval("tc37x.hsm curstate").unwrap(), "running");
    assert_eq!(i.eval("tc37x.hsm cortex_m tl1_halt").unwrap(), "tl1_halt on");
    // Debugging enabled (C_DEBUGEN) and DWT on (DEMCR.TRCENA), through DBGBASE + window.
    let w = take_writes(&s);
    let base = pos(&w, (HSM_DBGBASE, 0xE000_0000));
    assert!(base < pos(&w, (HSM_DHCSR, 0xA05F_0001)), "{w:08x?}");
    assert!(with_sim(&s, |sim| sim.hsm.writes.contains(&(0xE000_EDFC, 1 << 24))));
    assert!(i.eval("tc37x.hsm cortex_m triggers").unwrap().contains("FP_COMP5: off"));
}

#[test]
fn hsm_without_debug_access_stays_unexamined_and_is_retried() {
    let s = server();
    with_sim(&s, |sim| sim.hsm.debug_enabled = false);
    s.interp.eval("init").unwrap();
    assert_eq!(s.interp.eval("tc37x.hsm was_examined").unwrap(), "0");
    assert_eq!(s.interp.eval("tc37x.cpu0 was_examined").unwrap(), "1");
    // The HSM firmware enables debugging later: the background poll examines it.
    with_sim(&s, |sim| sim.hsm.debug_enabled = true);
    ops::poll_all(&s.cx());
    assert_eq!(s.interp.eval("tc37x.hsm was_examined").unwrap(), "1");
}

#[test]
fn hsm_halt_registers_step_and_resume() {
    let s = server();
    let i = &s.interp;
    i.eval("init").unwrap();
    i.eval("targets tc37x.hsm").unwrap();
    take_writes(&s);
    i.eval("halt 1000").unwrap();
    let w = take_writes(&s);
    // tl1_halt: TL1 is pulsed before the HSM halt request, so the TriCore cores stop too.
    assert!(pos(&w, (CBS_TLC, 0x30)) < pos(&w, (HSM_DHCSR, 0xA05F_0003)), "{w:08x?}");
    assert_eq!(ops::state(&s.cx(), HSM).1.name(), "debug-request");

    let pc = with_sim(&s, |sim| sim.hsm.regs[15]);
    assert_eq!(i.eval("reg pc").unwrap(), format!("pc (/32): 0x{pc:08x}"));
    i.eval("reg r4 0x1234").unwrap();
    assert_eq!(with_sim(&s, |sim| sim.hsm.regs[4]), 0x1234);
    assert_eq!(i.eval("reg r4").unwrap(), "r4 (/32): 0x00001234");
    with_sim(&s, |sim| sim.hsm.regs[20] = 0x0200_8001);
    assert_eq!(i.eval("reg basepri").unwrap(), "basepri (/32): 0x00000080");
    assert!(i.eval("reg").unwrap().starts_with("===== cortex_m registers"));

    i.eval("step").unwrap();
    assert_eq!(with_sim(&s, |sim| sim.hsm.regs[15]), pc + 2);
    assert_eq!(ops::state(&s.cx(), HSM).1.name(), "single-step");
    // Interrupts masked for the step, C_STEP cleared again afterwards.
    let w = with_sim(&s, |sim| std::mem::take(&mut sim.hsm.writes));
    let step = pos(&w, (0xE000_EDF0, 0xA05F_000D));
    let clear = w.iter().rposition(|x| *x == (0xE000_EDF0, 0xA05F_000B)).unwrap();
    assert!(step < clear, "{w:08x?}");

    take_writes(&s);
    i.eval("resume").unwrap();
    let w = take_writes(&s);
    // resume-start releases the TriCore cores, the HSM runs (DFSR cleared, C_MASKINTS dropped),
    // resume-end clears the TL1 hold.
    let others = pos(&w, (DBGSR[0], 0x4));
    let run = pos(&w, (HSM_DHCSR, 0xA05F_0001));
    let hold = pos(&w, (CBS_TLCHS, 2));
    assert!(others < run && run < hold, "{w:08x?}");
    assert!(with_sim(&s, |sim| sim.hsm.writes.contains(&(0xE000_ED30, 0x1F))));
    assert_eq!(i.eval("tc37x.hsm curstate").unwrap(), "running");
}

#[test]
fn hsm_breakpoint_above_512_mb_and_watchpoint() {
    let s = server();
    let i = &s.interp;
    i.eval("init").unwrap();
    i.eval("targets tc37x.hsm").unwrap();
    i.eval("halt 1000").unwrap();
    i.eval("bp 0x80028602 2").unwrap();
    // Highest comparator first (the boot ROM uses FP_COMP0); upper half-word, COMPH = 4.
    let w = with_sim(&s, |sim| std::mem::take(&mut sim.hsm.writes));
    assert!(w.contains(&(0xE000_203C, 4)), "FP_COMPH5: {w:08x?}");
    assert!(w.contains(&(0xE000_201C, 0x0002_8600 | 2 << 30 | 1)), "FP_COMP5: {w:08x?}");
    i.eval("resume").unwrap();
    i.eval("wait_halt 1000").unwrap();
    assert_eq!(ops::state(&s.cx(), HSM).1.name(), "breakpoint");
    assert_eq!(i.eval("reg pc").unwrap(), "pc (/32): 0x80028602");
    i.eval("rbp all").unwrap();
    assert!(with_sim(&s, |sim| sim.hsm.writes.contains(&(0xE000_201C, 0))));

    i.eval("wp 0x20001000 4 w").unwrap();
    let w = with_sim(&s, |sim| std::mem::take(&mut sim.hsm.writes));
    assert!(w.contains(&(0xE000_1020, 0x2000_1000)) && w.contains(&(0xE000_1024, 2)) && w.contains(&(0xE000_1028, 6)), "{w:08x?}");
    i.eval("rwp all").unwrap();
    assert!(with_sim(&s, |sim| sim.hsm.writes.contains(&(0xE000_1028, 0))));
}

#[test]
fn hsm_memory_goes_through_the_window_after_a_cache_clean() {
    let s = server();
    s.interp.eval("init").unwrap();
    let cx = s.cx();
    with_sim(&s, |sim| sim.hsm.mem.insert(0x2000_1000, 0x4433_2211));
    // Local RAM while the HSM runs: refused (no cache clean while running).
    let e = ops::read_memory_as(&cx, HSM, 0x2000_1000, 4).unwrap_err().to_string();
    assert!(e.contains("halted"), "{e}");
    s.interp.eval("tc37x.hsm arp_halt").unwrap();
    ops::poll_all(&cx);
    assert_eq!(ops::read_memory_as(&cx, HSM, 0x2000_1001, 2).unwrap(), vec![0x22, 0x33]);
    assert_eq!(with_sim(&s, |sim| (sim.hsm.cache_cleans, sim.hsm.uncleaned_reads)), (64, 0));
    ops::write_memory_as(&cx, HSM, 0x2000_1002, &[0xAA]).unwrap();
    assert_eq!(with_sim(&s, |sim| sim.hsm.mem[&0x2000_1000]), 0x44AA_2211);
    // Host memory has the same address for the HSM: never through the window.
    s.interp.eval("mww 0x70000000 0x12345678").unwrap();
    take_writes(&s);
    assert_eq!(ops::read_memory_as(&cx, HSM, 0x7000_0000, 4).unwrap(), vec![0x78, 0x56, 0x34, 0x12]);
    assert!(take_writes(&s).is_empty());
    // A TriCore core reads host memory at the same address.
    assert!(ops::read_memory_as(&cx, 0, 0x2000_1000, 4).unwrap() != vec![0x11, 0x22, 0xAA, 0x44]);
}

#[test]
fn tricore_resume_releases_a_halted_hsm() {
    let s = server();
    let i = &s.interp;
    i.eval("init").unwrap();
    i.eval("tc37x.hsm arp_halt").unwrap();
    i.eval("halt 1000").unwrap();
    ops::poll_all(&s.cx());
    assert_eq!(i.eval("tc37x.hsm curstate").unwrap(), "halted");
    take_writes(&s);
    i.eval("resume").unwrap();
    let w = take_writes(&s);
    assert!(pos(&w, (HSM_DHCSR, 0xA05F_0001)) < pos(&w, (CBS_TLCHS, 2)), "{w:08x?}");
    ops::poll_all(&s.cx());
    assert_eq!(i.eval("tc37x.hsm curstate").unwrap(), "running");
}

#[test]
fn reset_halt_keeps_the_hsm_breakpoints_after_the_release() {
    let s = server();
    let i = &s.interp;
    i.eval("init").unwrap();
    i.eval("targets tc37x.hsm").unwrap();
    i.eval("halt 1000").unwrap();
    i.eval("bp 0x80028700 2").unwrap();
    i.eval("targets tc37x.cpu0").unwrap();
    with_sim(&s, |sim| sim.hsm.writes.clear());
    i.eval("reset halt").unwrap();
    let w = with_sim(&s, |sim| std::mem::take(&mut sim.hsm.writes));
    // Restored in FP_COMP5 while the boot ROM breakpoint (FP_COMP0) is deleted by the release.
    assert!(w.contains(&(0xE000_201C, 0x0002_8700 | 1 << 30 | 1)), "{w:08x?}");
    assert!(w.contains(&(0xE000_2008, 0)), "{w:08x?}");
    assert_eq!(i.eval("tc37x.cpu0 curstate").unwrap(), "halted");
}

// --- GTM MCS channels -------------------------------------------------------------------------

const MCS0_CH0: usize = 4;
const MCS0_CH2: usize = 6;

fn asm(name: &str, args: &[i64]) -> u32 {
    isa::mcs::encode(isa::mcs::find_op(name).unwrap(), args).unwrap()
}

/// MCS0 test program: channel 0 counts in R0 and calls a subroutine counting in R2, channel 1
/// counts STRG bit 0 events in R1.
fn mcs_program() -> Vec<u32> {
    let mut p = vec![0u32; 0x80 / 4];
    let mut put = |addr: u32, w: u32| p[(addr / 4) as usize] = w;
    put(0x00, asm("JMP", &[0x20]));
    put(0x04, asm("JMP", &[0x40]));
    put(0x20, asm("MOVL", &[7, 0x100 - 4]));
    put(0x24, asm("MOVL", &[0, 0]));
    put(0x28, asm("ADDL", &[0, 1]));
    put(0x2C, asm("CALL", &[0x60]));
    put(0x30, asm("JMP", &[0x28]));
    put(0x40, asm("MOVL", &[1, 0]));
    put(0x44, asm("WURM", &[0, 11, 1]));
    put(0x48, asm("ADDL", &[1, 1]));
    put(0x4C, asm("JMP", &[0x44]));
    put(0x60, asm("ADDL", &[2, 1]));
    put(0x64, asm("RET", &[]));
    p
}

/// Initialized server with the GTM enabled, the MCS0 program loaded and channels 0 and 1 running.
fn mcs_server() -> Server {
    let s = server();
    s.interp.eval("init").unwrap();
    with_sim(&s, |sim| {
        sim.gtm.enabled = true;
        sim.gtm.load(0, &mcs_program());
        sim.gtm.modules[0].ch[0].en = true;
        sim.gtm.modules[0].ch[1].en = true;
    });
    ops::poll_all(&s.cx());
    s
}

fn poll_until(s: &Server, what: &str, f: impl Fn(&Server) -> bool) {
    for _ in 0..200 {
        ops::poll_all(&s.cx());
        if f(s) {
            return;
        }
    }
    panic!("timed out waiting for {what}");
}

fn ram_word(s: &Server, addr: u32) -> u32 {
    with_sim(s, |sim| sim.gtm.modules[0].ram[(addr / 4) as usize])
}

fn curstate(s: &Server, t: &str) -> String {
    s.interp.eval(&format!("{t} curstate")).unwrap()
}

#[test]
fn mcs_channels_are_examined_with_the_gtm_and_listed_once_used() {
    let s = server();
    let i = &s.interp;
    i.eval("init").unwrap();
    assert_eq!(i.eval("tc37x.mcs0.ch0 was_examined").unwrap(), "0");
    with_sim(&s, |sim| {
        sim.gtm.enabled = true;
        sim.gtm.load(0, &mcs_program());
        sim.gtm.modules[0].ch[0].en = true;
    });
    ops::poll_all(&s.cx());
    assert_eq!(i.eval("tc37x.mcs0.ch0 was_examined").unwrap(), "1");
    assert_eq!(i.eval("tc37x.mcs0.ch2 was_examined").unwrap(), "1");
    assert_eq!(curstate(&s, "tc37x.mcs0.ch0"), "running");
    s.cx().with(|c| {
        assert!(c.targets[MCS0_CH0].core.visible());
        // Never enabled and still at its reset PC: not shown.
        assert!(!c.targets[MCS0_CH2].core.visible());
        assert_eq!(c.targets[MCS0_CH2].core.state().name(), "halted");
    });
}

#[test]
fn mcs_halt_register_access_and_resume() {
    let s = mcs_server();
    let i = &s.interp;
    i.eval("tc37x.mcs0.ch0 arp_halt").unwrap();
    ops::poll_all(&s.cx());
    assert_eq!(curstate(&s, "tc37x.mcs0.ch0"), "halted");
    assert!(!with_sim(&s, |sim| sim.gtm.modules[0].ch[0].en));
    // The other channel of the module keeps running.
    assert_eq!(curstate(&s, "tc37x.mcs0.ch1"), "running");
    i.eval("targets tc37x.mcs0.ch0").unwrap();
    let r0 = with_sim(&s, |sim| sim.gtm.modules[0].ch[0].r[0]);
    assert_eq!(i.eval("reg r0").unwrap(), format!("r0 (/32): 0x{r0:08x}"));
    i.eval("reg r3 0x1234").unwrap();
    assert_eq!(with_sim(&s, |sim| sim.gtm.modules[0].ch[0].r[3]), 0x1234);
    i.eval("resume").unwrap();
    assert!(with_sim(&s, |sim| sim.gtm.modules[0].ch[0].en));
    assert_eq!(curstate(&s, "tc37x.mcs0.ch0"), "running");
}

#[test]
fn mcs_breakpoint_is_a_shared_self_jump_and_is_stepped_over_on_resume() {
    let s = mcs_server();
    let i = &s.interp;
    let orig = ram_word(&s, 0x60);
    i.eval("targets tc37x.mcs0.ch0").unwrap();
    i.eval("bp 0x60 4").unwrap();
    i.eval("targets tc37x.mcs0.ch1").unwrap();
    i.eval("bp 0x60 4").unwrap();
    assert_eq!(ram_word(&s, 0x60), 0xE000_0060);
    // Debugger reads show the instruction under the breakpoint.
    assert_eq!(ops::read_memory_as(&s.cx(), MCS0_CH0, 0x60, 4).unwrap(), orig.to_le_bytes().to_vec());

    poll_until(&s, "breakpoint hit", |s| curstate(s, "tc37x.mcs0.ch0") == "halted");
    assert_eq!(ops::state(&s.cx(), MCS0_CH0).1.name(), "breakpoint");
    i.eval("targets tc37x.mcs0.ch0").unwrap();
    assert_eq!(i.eval("reg pc").unwrap(), "pc (/32): 0x00000060");
    assert_eq!(curstate(&s, "tc37x.mcs0.ch1"), "running");

    // Removed on one channel only: still planted for the other.
    i.eval("targets tc37x.mcs0.ch1").unwrap();
    i.eval("rbp 0x60").unwrap();
    assert_eq!(ram_word(&s, 0x60), 0xE000_0060);

    // Resume executes the instruction under the breakpoint and plants it again.
    let r2 = with_sim(&s, |sim| sim.gtm.modules[0].ch[0].r[2]);
    i.eval("targets tc37x.mcs0.ch0").unwrap();
    i.eval("resume").unwrap();
    assert_eq!(with_sim(&s, |sim| sim.gtm.modules[0].ch[0].r[2]), r2 + 1);
    assert_eq!(ram_word(&s, 0x60), 0xE000_0060);
    assert_eq!(ram_word(&s, 0x64), asm("RET", &[]), "step trap removed");
    poll_until(&s, "second hit", |s| curstate(s, "tc37x.mcs0.ch0") == "halted");
    i.eval("rbp 0x60").unwrap();
    assert_eq!(ram_word(&s, 0x60), orig);
}

#[test]
fn mcs_step_into_over_and_return() {
    let s = mcs_server();
    let i = &s.interp;
    let cx = s.cx();
    i.eval("tc37x.mcs0.ch0 arp_halt").unwrap();
    ops::poll_all(&cx);
    i.eval("targets tc37x.mcs0.ch0").unwrap();
    i.eval("reg pc 0x2c").unwrap();
    i.eval("step").unwrap();
    assert_eq!(i.eval("reg pc").unwrap(), "pc (/32): 0x00000060", "step into the call");
    i.eval("step").unwrap();
    i.eval("step").unwrap();
    assert_eq!(i.eval("reg pc").unwrap(), "pc (/32): 0x00000030", "return");
    assert_eq!(ops::state(&cx, MCS0_CH0).1.name(), "single-step");
    i.eval("reg pc 0x2c").unwrap();
    ops::step_as(&cx, MCS0_CH0, None, aurix_core::target::RunMode::StepOver).unwrap();
    assert_eq!(i.eval("reg pc").unwrap(), "pc (/32): 0x00000030", "step over the call");
    assert!(!with_sim(&s, |sim| sim.gtm.modules[0].ch[0].en));
    for a in [0x28, 0x2C, 0x30, 0x60, 0x64] {
        assert_eq!(ram_word(&s, a), mcs_program()[(a / 4) as usize], "trap at 0x{a:x} left behind");
    }
}

#[test]
fn mcs_halt_of_a_waiting_channel_takes_effect_after_the_wait() {
    let s = mcs_server();
    let i = &s.interp;
    poll_until(&s, "channel 1 waiting", |s| with_sim(s, |sim| sim.gtm.modules[0].ch[1].waiting));
    i.eval("tc37x.mcs0.ch1 arp_halt").unwrap();
    ops::poll_all(&s.cx());
    assert_eq!(curstate(&s, "tc37x.mcs0.ch1"), "running");
    with_sim(&s, |sim| sim.gtm.modules[0].strg |= 1);
    poll_until(&s, "channel 1 stopped", |s| curstate(s, "tc37x.mcs0.ch1") == "halted");
    i.eval("targets tc37x.mcs0.ch1").unwrap();
    assert_eq!(i.eval("reg pc").unwrap(), "pc (/32): 0x00000048");
}

#[test]
fn mcs_breakpoint_is_planted_again_after_the_program_is_reloaded() {
    let s = mcs_server();
    let i = &s.interp;
    i.eval("targets tc37x.mcs0.ch0").unwrap();
    i.eval("bp 0x30 4").unwrap();
    // The application loads the program again (e.g. after its own re-initialisation).
    with_sim(&s, |sim| sim.gtm.load(0, &mcs_program()));
    ops::poll_all(&s.cx());
    assert_eq!(ram_word(&s, 0x30), 0xE000_0030);
    i.eval("rbp 0x30").unwrap();
    assert_eq!(ram_word(&s, 0x30), asm("JMP", &[0x28]));
}

/// The application protects the MCS code against CPU writes (CCM ARP0 with WPROT_AEI, locked by
/// CLS_PROT; values read on the TC377): breakpoints open it for the write and restore it.
#[test]
fn mcs_breakpoint_opens_the_ram_write_protection_and_restores_it() {
    let s = mcs_server();
    let i = &s.interp;
    let arps = [0x800B_0000, 0x8103_0020, 0x0107_0100, 0x8107_0040];
    with_sim(&s, |sim| {
        let md = &mut sim.gtm.modules[0];
        md.arp[..4].copy_from_slice(&arps);
        md.cls_prot = true;
    });
    i.eval("mww 0xF0138060 0").unwrap_err();
    i.eval("targets tc37x.mcs0.ch0").unwrap();
    i.eval("bp 0x60 4").unwrap();
    assert_eq!(ram_word(&s, 0x60), 0xE000_0060);
    let (arp, cls) = with_sim(&s, |sim| (sim.gtm.modules[0].arp, sim.gtm.modules[0].cls_prot));
    assert_eq!(&arp[..4], &arps);
    assert!(cls);
    // Stepping over the breakpoint writes traps and the instruction through the same path.
    poll_until(&s, "breakpoint hit", |s| curstate(s, "tc37x.mcs0.ch0") == "halted");
    i.eval("resume").unwrap();
    assert_eq!(ram_word(&s, 0x64), asm("RET", &[]));
    i.eval("rbp 0x60").unwrap();
    assert_eq!(ram_word(&s, 0x60), asm("ADDL", &[2, 1]));
    assert!(with_sim(&s, |sim| sim.gtm.modules[0].cls_prot && sim.gtm.modules[0].arp[0] == 0x800B_0000));
}

#[test]
fn mcs_halt_all_handler_pulses_tl1_only_for_breakpoints() {
    let s = mcs_server();
    let i = &s.interp;
    i.eval("tc37x.mcs0.ch0 configure -event halted {tc3xx_mcs_halt_all tc37x.mcs0.ch0}").unwrap();
    take_writes(&s);
    i.eval("tc37x.mcs0.ch0 arp_halt").unwrap();
    ops::poll_all(&s.cx());
    assert!(!take_writes(&s).contains(&(CBS_TLC, 0x30)), "a pause of the channel does not halt the cores");
    i.eval("tc37x.mcs0.ch0 arp_resume").unwrap();
    i.eval("targets tc37x.mcs0.ch0").unwrap();
    i.eval("bp 0x28 4").unwrap();
    take_writes(&s);
    poll_until(&s, "breakpoint", |s| curstate(s, "tc37x.mcs0.ch0") == "halted");
    assert!(take_writes(&s).contains(&(CBS_TLC, 0x30)));
}

/// tc3xx_ocds.cfg takes the cores and their registers from the target cfg: TC39x with six cores,
/// CPU5 at dbgbase 0xF88C0000, and an MCS RAM size for the module.
#[test]
fn generic_setup_follows_the_target_cfg() {
    aurix_core::log::set_debug_level(1);
    let s = Server::new();
    s.cx().with(|c| c.search_dirs.push(repo().join("third_party/openocd/tcl")));
    let i = &s.interp;
    i.eval_global("adapter driver sim; script target/infineon/tc39x.cfg").unwrap();
    i.eval_global("set tc3xx_mcs_modules {0 9}; set tc3xx_mcs_ramsize 0x1800").unwrap();
    i.eval_global(&format!("script {}", jim::quote(&tcl_path(repo().join("tc3xx_ocds.cfg"))))).unwrap();
    i.eval_global("tc3xx_ocds_setup tc39x").unwrap();
    assert_eq!(i.eval("set tc3xx_cores").unwrap(), "tc39x.cpu0 tc39x.cpu1 tc39x.cpu2 tc39x.cpu3 tc39x.cpu4 tc39x.cpu5");
    assert_eq!(i.eval("set tc3xx_dbgsr(tc39x.cpu5)").unwrap(), "0xF88DFD00");
    assert_eq!(i.eval("set tc3xx_coreid(tc39x.cpu5)").unwrap(), "5");
    assert_eq!(i.eval("format 0x%08X [tc3xx_cbs_trc 5]").unwrap(), "0xF0000434");
    assert_eq!(i.eval("tc39x.cpu5 cget -event step-start").unwrap(), "tc3xx_on_step_start tc39x.cpu5");
    assert_eq!(i.eval("tc39x.hsm cget -type").unwrap(), "cortex_m");
    assert_eq!(i.eval("tc39x.mcs9.ch7 cget -ramsize").unwrap(), "0x1800");
    assert_eq!(i.eval("target current").unwrap(), "tc39x.cpu0");
    assert!(i.eval("catch {target create x gtm_mcs -module 10}").unwrap() == "1");
    assert!(i.eval("catch {tc3xx_ocds_setup tc27x}").unwrap() == "1");
}
