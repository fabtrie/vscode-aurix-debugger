//! Loads the real OpenOCD board cfg and tc37x_ocds_init.cfg against the
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
    assert_eq!(i.eval("target names").unwrap(), "tc37x.cpu0 tc37x.cpu1 tc37x.cpu2");
    assert_eq!(i.eval("flash banks").unwrap().lines().count(), 2);
    assert_eq!(i.eval("tc37x.cpu1 cget -dbgbase").unwrap(), "0xf8820000");
    assert_eq!(i.eval("tc37x.cpu0 cget -event examine-end").unwrap().trim(), "tc37x_on_examine");
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
    assert_eq!(s.interp.eval("tc37x_step_timers frozen").unwrap(), "frozen");
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
    assert!(s.interp.eval("catch {tc37x_step_timers slow}").unwrap() == "1");
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
    s.interp.eval("tc37x_step_timers frozen").unwrap();
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
