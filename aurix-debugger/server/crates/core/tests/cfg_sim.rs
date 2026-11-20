//! Loads the real OpenOCD board cfg and tc37x_ocds_init.cfg (with tc3xx_ocds.cfg and fs26_sbc.cfg)
//! against the simulated TC37x and checks the event-driven register sequences and the FS26 attach and
//! reset flows.

use std::path::PathBuf;

use aurix_core::sim::{SimAdapter, SimFs26, APP_DRIVER_STATE, APP_MARKER, APP_MARKER_DEBUGGER};
use aurix_core::{ops, Server};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize().unwrap()
}

fn tcl_path(p: PathBuf) -> String {
    p.to_string_lossy().trim_start_matches(r"\\?\").replace('\\', "/")
}

fn server() -> Server {
    server_with_mcs(true)
}

/// The board cfgs; with `mcs`, the MCS0 and MCS1 channels as targets (off by default).
fn server_with_mcs(mcs: bool) -> Server {
    aurix_core::log::set_debug_level(1);
    let s = Server::new();
    s.cx().with(|c| {
        c.adapter_override = Some("sim".into());
        c.search_dirs.push(PathBuf::from(openocd_tcl::TCL_DIR));
    });
    if mcs {
        s.interp.eval_global("set tc3xx_mcs_modules {0 1}").unwrap();
    }
    s.interp.eval_global("script board/infineon/kit_a2g_tc375_lite.cfg").unwrap();
    s.interp.eval_global(aurix_core::sim::SIM_BOARD_SETTINGS).unwrap();
    s.interp
        .eval_global(&format!("script {}", jim::quote(&tcl_path(repo().join("tc37x_ocds_init.cfg")))))
        .unwrap();
    // The simulated HSM never reports ready: don't wait for it after halting resets.
    s.interp.eval_global("hsm ready_ms 0").unwrap();
    // No FS26 unless a test adds one: the debugger's reset is not wired to it and there is no
    // watchdog to disarm (the FS26 tests set these themselves, see `add_fs26`).
    s.interp.eval_global("set sbc_rstb_wired 0; set sbc_disarmed 1").unwrap();
    s
}

/// Puts an FS26 on the simulated board; its watchdog state is not known to the scripts, as after a
/// connection.
fn add_fs26(s: &Server, f: SimFs26) {
    with_sim(s, |sim| sim.fs26 = Some(f));
    s.interp.eval("set sbc_disarmed 0").unwrap();
}

fn no_violations(s: &Server) {
    let v = with_sim(s, |sim| sim.violations.clone());
    assert!(v.is_empty(), "OCS writes the hardware refuses (they reset the board): {v:08x?}");
}

fn mem(s: &Server, addr: u32) -> u32 {
    let v = s.interp.eval(&format!("tc3xx_rd {addr:#x}")).unwrap();
    u32::from_str_radix(v.trim_start_matches("0x"), 16).unwrap()
}

fn ocs_sus(s: &Server, ocs: u32) -> u32 {
    (mem(s, ocs) >> 24) & 0xF
}

const STM0_OCS: u32 = 0xF000_10E8;
const STM1_OCS: u32 = 0xF000_11E8;
const EVADC_OCS: u32 = 0xF002_0028;

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
    assert_eq!(s.interp.eval("ocds step_timers frozen").unwrap(), "frozen");
    s.interp.eval("halt 1000").unwrap();
    let w = take_writes(&s);
    for d in DBGSR {
        assert!(w.contains(&(d, 0x10)), "SUSP for {d:08x} missing after halt: {w:08x?}");
    }
    s.interp.eval("step").unwrap();
    let w = take_writes(&s);
    // The other cores released under the TL1 hold with SUSP kept, the step trigger keeping SUSP=1,
    // then this core released with SUSP.
    let others = pos(&w, (DBGSR[1], 0x14));
    let evt = pos(&w, (0xF881_F038, 0x1022));
    let release = pos(&w, (DBGSR[0], 0x14));
    assert!(others < evt && evt < release, "{w:08x?}");

    s.interp.eval("resume").unwrap();
    let w = take_writes(&s);
    let p_clear = pos(&w, (DBGSR[0], 0));
    let pend = pos(&w, (CBS_TLCHS, 2));
    assert!(p_clear < pend, "suspend-out cleared after the TL1 hold was dropped: {w:08x?}");
    assert!(s.interp.eval("catch {ocds step_timers slow}").unwrap() == "1");
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
    // tl1_step: TL1 still held from the halt (no new pulse); the step releases the other cores
    // under the hold (SUSP clear for "run" mode), arms the trigger, releases cpu0 and then clears
    // the hold.
    assert!(!w.contains(&(CBS_TLC, 0x30)), "TL1 pulsed although it is held: {w:08x?}");
    let others = pos(&w, (DBGSR[1], 0x4));
    // TR7EVT: PC trigger, no BBM, EVTA=halt, SUSP=0 (TL1 stops the STMs with the cores)
    let evt = pos(&w, (0xF881_F038, 0x1002));
    let release = pos(&w, (DBGSR[0], 0x4));
    let start = pos(&w, (CBS_TLCHS, 2));
    assert!(others < evt && evt < release && release < start, "wrong order: {w:08x?}");
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
    s.interp.eval("ocds step_timers frozen").unwrap();
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

/// Debugger writes to globals linked in the cached LMU/DLMU segment `0x9` go to the non-cached
/// alias `0xB` on every path (Tcl, `ops`, the per-core path of TCF/REST, raw `Io`): the board drops
/// a write to `0x9` and resets (the simulator records it as a violation). Reads go there too.
#[test]
fn cached_lmu_writes_go_to_the_non_cached_alias() {
    const DLMU: u32 = 0x9000_2000;
    const ALIAS: u32 = 0xB000_2000;
    let s = server();
    s.interp.eval("init").unwrap();
    take_writes(&s);
    s.interp.eval(&format!("mww {DLMU:#x} 0x11223344")).unwrap();
    s.interp.eval(&format!("mwb {:#x} 0x55", DLMU + 4)).unwrap();
    s.interp.eval(&format!("write_memory {:#x} 32 {{0x66778899}}", DLMU + 8)).unwrap();
    ops::write_memory(&s.cx(), DLMU + 12, &0xAABB_CCDDu32.to_le_bytes()).unwrap();
    ops::write_memory_as(&s.cx(), 1, DLMU + 16, &0x0102_0304u32.to_le_bytes()).unwrap();
    let r = s.cx().with(|c| c.io().unwrap().write_op(DLMU + 20, &0x0506_0708u32.to_le_bytes())).unwrap();
    assert!(r.ok());
    no_violations(&s);
    let w = take_writes(&s);
    assert!(w.iter().all(|&(a, _)| !aurix_core::tricore::CACHED_RAM.contains(&a)), "a write reached the cached alias: {w:08x?}");
    assert_eq!(with_sim(&s, |sim| sim.mem_word(ALIAS)), 0x1122_3344);
    assert_eq!(
        s.interp.eval(&format!("mdw {DLMU:#x} 6")).unwrap(),
        format!("{DLMU:#x}: 11223344 00000055 66778899 aabbccdd 01020304 05060708 ")
    );
    assert_eq!(ops::read_memory_as(&s.cx(), 0, DLMU + 12, 4).unwrap(), 0xAABB_CCDDu32.to_le_bytes());

    // What the board does with a write that still goes to the cached alias.
    let mut b = tas::Batch::new();
    b.write32(DLMU as u64, 0);
    let r = s.cx().with(|c| c.adapter.as_mut().unwrap().execute(&b)).unwrap();
    assert!(!r[0].ok());
    assert_eq!(with_sim(&s, |sim| std::mem::take(&mut sim.violations)), vec![DLMU]);
}

#[test]
fn write_images_places_each_file_by_its_own_options() {
    let s = server();
    s.interp.eval("init").unwrap();
    let bin = std::env::temp_dir().join(format!("aurix-write-images-{}.bin", std::process::id()));
    std::fs::write(&bin, [1, 2, 3, 4]).unwrap();
    let bin = jim::quote(&tcl_path(bin));
    let err = |cmd: &str| s.interp.eval(&format!("catch {{{cmd}}} err; set err")).unwrap();
    // The image data lies outside every flash bank, which names where the file was placed.
    assert!(err(&format!("flash write_images {bin}")).contains("image data at 0x00000000"));
    assert!(err(&format!("flash write_images erase -offset 0x90000000 {bin} {bin}")).contains("image data at 0x00000000"));
    assert!(err(&format!("flash write_images {bin} -offset 0x90000000 -type bin {bin}")).contains("image data at 0x00000000"));
    let e = err(&format!("flash write_images -offset 0x90000000 {bin}"));
    assert!(e.contains("image data at 0x90000000"), "{e}");
    assert!(err(&format!("flash write_images -type s19 {bin}")).contains("S-record line 1"));
    assert!(err(&format!("flash write_images {bin} -offset 0x90000000")).starts_with("usage"));
    assert!(err("flash write_images -type").starts_with("usage"));
    // Next to flash data, data outside the flash banks (here the non-cached LMU) is skipped with a
    // warning instead of failing the write.
    let e = err(&format!("flash write_images erase -offset 0x80000000 {bin} -offset 0xb0040000 {bin}"));
    assert!(e.contains("wrote 4 bytes from 2 files") && e.contains("4 bytes outside the flash banks skipped"), "{e}");
    std::fs::remove_file(std::env::temp_dir().join(format!("aurix-write-images-{}.bin", std::process::id()))).unwrap();
}

#[test]
fn non_cached_pflash_data_is_written_to_the_cached_bank() {
    use aurix_core::image::{Image, Section};
    let s = server();
    s.interp.eval("init").unwrap();
    let tmp = |name: &str, img: &Image| {
        let p = std::env::temp_dir().join(format!("aurix-nc-{name}-{}.hex", std::process::id()));
        std::fs::write(&p, img.to_ihex()).unwrap();
        p
    };
    let words = |n: u32, seed: u32| (0..n).flat_map(|i| (seed + i).to_le_bytes()).collect::<Vec<u8>>();
    // An iLLD project: startup code at the non-cached starts of CPU0 (0xA0000000) and CPU1/2
    // (0xA0300200), the rest at cached addresses, initialized data in the non-cached LMU.
    let app = tmp(
        "app",
        &Image::from_sections(vec![
            Section { addr: 0xA000_0000, data: words(8, 0x1000) },
            Section { addr: 0x8000_0100, data: words(8, 0x2000) },
            Section { addr: 0xA030_0200, data: words(14, 0x3000) },
            Section { addr: 0xB004_0000, data: words(1, 0x4000) },
        ]),
    );
    let out = s.interp.eval(&format!("flash write_images erase verify {}", jim::quote(&tcl_path(app.clone())))).unwrap();
    // Only the LMU data is skipped; the verify reads the startup code back.
    assert!(out.contains("wrote 120 bytes from 1 files") && out.contains("4 bytes outside the flash banks skipped"), "{out}");
    assert!(out.contains("verified 120 bytes"), "{out}");
    assert_eq!((mem(&s, 0x8000_0000), mem(&s, 0x8000_001C)), (0x1000, 0x1007));
    assert_eq!((mem(&s, 0x8030_0200), mem(&s, 0x8030_0234)), (0x3000, 0x300D));
    assert_eq!(mem(&s, 0x8000_0100), 0x2000);
    // So does program.
    let p = s.interp.eval(&format!("program {} verify", jim::quote(&tcl_path(app.clone())))).unwrap();
    assert!(p.contains("** Verified OK ** (120 bytes)"), "{p}");

    // The same bytes at both addresses are fine and written once.
    let same = tmp(
        "same",
        &Image::from_sections(vec![Section { addr: 0x8000_0000, data: words(8, 0x5000) }, Section { addr: 0xA000_0000, data: words(8, 0x5000) }]),
    );
    let out = s.interp.eval(&format!("flash write_image erase {}", jim::quote(&tcl_path(same.clone())))).unwrap();
    assert!(out.contains("wrote 32 bytes") && !out.contains("skipped"), "{out}");
    assert_eq!(mem(&s, 0x8000_0000), 0x5000);
    // verify_image compares the file as it is: both addresses read the same flash.
    let v = s.interp.eval(&format!("verify_image {}", jim::quote(&tcl_path(same.clone())))).unwrap();
    assert_eq!(v, "verified 64 bytes in 2 sections");
    // Different bytes for the same flash in one file are an error...
    let clash = tmp(
        "clash",
        &Image::from_sections(vec![Section { addr: 0x8000_0004, data: words(1, 0x6000) }, Section { addr: 0xA000_0000, data: words(8, 0x7000) }]),
    );
    let err = s.interp.eval(&format!("catch {{flash write_images erase {}}} err; set err", jim::quote(&tcl_path(clash.clone())))).unwrap();
    assert!(err.contains("different data at 0xa0000004") && err.contains("0x80000004"), "{err}");
    // ...while across files the later file wins, at either address (`app` covers all of `same`).
    let out = s
        .interp
        .eval(&format!(
            "flash write_images erase verify {} {}",
            jim::quote(&tcl_path(same.clone())),
            jim::quote(&tcl_path(app.clone()))
        ))
        .unwrap();
    assert!(out.contains("wrote 120 bytes from 2 files"), "{out}");
    assert!(out.contains("verified 120 bytes"), "{out}");
    assert_eq!(mem(&s, 0x8000_0000), 0x1000);
    for p in [app, same, clash] {
        std::fs::remove_file(p).unwrap();
    }
}

#[test]
fn reset_halt_runs_reset_sequence() {
    let s = server();
    s.interp.eval("init").unwrap();
    s.interp.eval("reset halt").unwrap();
    assert_eq!(s.interp.eval("tc37x.cpu0 curstate").unwrap(), "halted");
    assert_eq!(s.interp.eval("reg pc").unwrap(), "pc (/32): 0x800a8000");
}

/// Wired board (debugger reset on the FS26 RSTB), wiring not configured: the first halting reset is
/// lost to the FS26's INIT_FS reset, which tells the wiring; the default then drives the FS26 through
/// deep fail-safe with halting resets, after which it is in debug mode and the halt at the reset
/// vector holds. Resumed, the application sees the debugger and disarms the watchdog.
#[test]
fn wired_reset_halt_goes_through_deep_fail_safe_and_holds() {
    let s = server();
    s.interp.eval("set sbc_rstb_wired auto").unwrap();
    s.interp.eval("set sbc_hold_ms 350").unwrap();
    let mut f = SimFs26::new(true, false);
    f.dfs_limit = 3;
    add_fs26(&s, f);
    s.interp.eval("init").unwrap();
    s.interp.eval("reset halt").unwrap();
    assert_eq!(s.interp.eval("set sbc_wired_found").unwrap(), "1");
    assert_eq!(with_sim(&s, |sim| sim.fs26.as_ref().unwrap().dfs_entries), 1, "no deep fail-safe");
    assert_eq!(s.interp.eval("set sbc_assume").unwrap(), "debug");
    assert_eq!(curstate(&s, "tc37x.cpu0"), "halted");
    assert_eq!(s.interp.eval("reg pc").unwrap(), "pc (/32): 0x800a8000");
    // Holds: the FS26 is in debug mode.
    std::thread::sleep(std::time::Duration::from_millis(400));
    ops::poll_all(&s.cx());
    assert_eq!(curstate(&s, "tc37x.cpu0"), "halted");

    s.interp.eval("resume").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(80));
    ops::poll_all(&s.cx());
    assert_eq!(with_sim(&s, |sim| sim.mem_word(APP_MARKER)), APP_MARKER_DEBUGGER);
    assert_eq!(with_sim(&s, |sim| sim.mem_word(APP_DRIVER_STATE)), 3);
    assert!(with_sim(&s, |sim| { let f = sim.fs26.as_ref().unwrap(); !f.armed && !f.debug_mode }));
    assert_eq!(s.interp.eval("set sbc_assume").unwrap(), "");
    assert_eq!(s.interp.eval("set sbc_disarmed").unwrap(), "1", "resumed from the halting reset: disarmed");
    no_violations(&s);
}

/// Explicit sbc_rstb_wired 0: the halt after the reset is not watched (no ~400 ms hold check). On a
/// board whose debugger reset does reach the FS26 after all, the FS26's reset comes later and is one
/// the poll detects; it is never repeated - no deep fail-safe on a board said to be unwired.
#[test]
fn halt_with_rstb_unwired_setting_is_not_watched_or_repeated() {
    let s = server();
    add_fs26(&s, SimFs26::new(true, false));
    s.interp.eval("init").unwrap();
    s.interp.eval("set sbc_disarmed 1").unwrap();
    let t0 = std::time::Instant::now();
    s.interp.eval("reset halt").unwrap();
    assert!(t0.elapsed() < std::time::Duration::from_millis(250), "halt watched: {:?}", t0.elapsed());
    assert_eq!(curstate(&s, "tc37x.cpu0"), "halted");
    std::thread::sleep(std::time::Duration::from_millis(400));
    ops::poll_all(&s.cx());
    ops::poll_all(&s.cx());
    assert_eq!(with_sim(&s, |sim| sim.fs26.as_ref().unwrap().resets), 1);
    assert_eq!(with_sim(&s, |sim| sim.fs26.as_ref().unwrap().dfs_entries), 0);
    no_violations(&s);
}

/// sbc_wired_reset_halt app: a halting reset the FS26 ends becomes a running reset and a halt in the
/// application, without deep fail-safe.
#[test]
fn wired_reset_halt_app_halts_in_the_application() {
    let s = server();
    s.interp.eval("set sbc_rstb_wired auto; set sbc_wired_reset_halt app; set sbc_hold_ms 350").unwrap();
    add_fs26(&s, SimFs26::new(true, false));
    s.interp.eval("init").unwrap();
    s.interp.eval("reset halt").unwrap();
    assert_eq!(curstate(&s, "tc37x.cpu0"), "halted");
    assert_eq!(mem(&s, 0xF000_10FC), 0x4, "the application did not run before the halt");
    assert_eq!(ocs_sus(&s, STM0_OCS), 2);
    assert_eq!(with_sim(&s, |sim| sim.fs26.as_ref().unwrap().dfs_entries), 0);
    std::thread::sleep(std::time::Duration::from_millis(300));
    ops::poll_all(&s.cx());
    assert_eq!(curstate(&s, "tc37x.cpu0"), "halted");
    no_violations(&s);
}

/// Unwired board, wiring not configured: the first reset after the connection is preceded by a
/// running reset that lets the application disarm the watchdog (its state is not read from the
/// application); later halting resets are done once. The halting reset holds (the FS26 does not see
/// it), with the STM suspend written before the application runs.
#[test]
fn unwired_reset_halt_disarms_once_then_is_done_once_and_holds() {
    let s = server();
    s.interp.eval("set sbc_rstb_wired auto").unwrap();
    add_fs26(&s, SimFs26::new(false, false));
    s.interp.eval("init").unwrap();
    take_writes(&s);
    s.interp.eval("reset halt").unwrap();
    let w = take_writes(&s);
    let setups = w.iter().filter(|x| **x == (0xF000_0494, 0xB000_003F)).count();
    assert_eq!(setups, 2, "first reset: a running reset, then the halting one: {w:08x?}");
    assert_eq!(s.interp.eval("set sbc_disarmed").unwrap(), "1");

    s.interp.eval("resume").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(80));
    ops::poll_all(&s.cx());
    take_writes(&s);
    s.interp.eval("reset halt").unwrap();
    assert_eq!(s.interp.eval("set sbc_wired_found").unwrap(), "0");
    assert_eq!(s.interp.eval("set sbc_assume").unwrap(), "held");
    let w = take_writes(&s);
    let setups = w.iter().filter(|x| **x == (0xF000_0494, 0xB000_003F)).count();
    assert_eq!(setups, 1, "reset repeated: {w:08x?}");
    assert!(w.contains(&(STM0_OCS, 0x1200_0000)), "STM0 suspend not written: {w:08x?}");
    assert_eq!(curstate(&s, "tc37x.cpu0"), "halted");
    assert_eq!(with_sim(&s, |sim| sim.fs26.as_ref().unwrap().resets), 0);
    no_violations(&s);
}

/// Unwired board, application started without the debugger (watchdog armed): the FS26 does not see
/// the debugger's reset and would end any halt after its window, the ~150 ms init halt included. So a
/// reset that runs at once comes first (the application disarms the watchdog), then the halting
/// reset: no FS26 reset at all, and the halt at the vector holds.
#[test]
fn unwired_reset_halt_disarms_an_armed_watchdog_first() {
    let s = server();
    add_fs26(&s, SimFs26::new(false, true));
    with_sim(&s, |sim| sim.app_started_without_debugger());
    s.interp.eval("init").unwrap();
    s.interp.eval("reset halt").unwrap();
    assert_eq!(curstate(&s, "tc37x.cpu0"), "halted");
    assert_eq!(s.interp.eval("reg pc").unwrap(), "pc (/32): 0x800a8000");
    assert!(with_sim(&s, |sim| !sim.fs26.as_ref().unwrap().armed), "watchdog still armed");
    std::thread::sleep(std::time::Duration::from_millis(200));
    ops::poll_all(&s.cx());
    assert_eq!(curstate(&s, "tc37x.cpu0"), "halted");
    assert_eq!(with_sim(&s, |sim| sim.fs26.as_ref().unwrap().resets), 0, "FS26 reset");
    assert_eq!(ocs_sus(&s, STM0_OCS), 2);
    no_violations(&s);
}

/// A reset without halt still gets STM0/1 their suspend: halted for the init, before the application
/// locks them (ACCEN0), then running; it returns once the application has disarmed the watchdog, and
/// the first resume suspends the modules the application enables.
#[test]
fn reset_run_suspends_stm_before_the_application_locks_it() {
    let s = server();
    s.interp.eval("init").unwrap();
    take_writes(&s);
    s.interp.eval("reset run").unwrap();
    let w = take_writes(&s);
    assert!(w.contains(&(STM0_OCS, 0x1200_0000)), "STM0 suspend not written: {w:08x?}");
    wait_polling(&s, "the application locks STM0", |s| mem(s, 0xF000_10FC) == 0x4);
    assert_eq!(ocs_sus(&s, STM0_OCS), 2);
    assert_eq!(ocs_sus(&s, STM1_OCS), 2);
    // The modules the application enables later get their suspend from the background poll.
    wait_polling(&s, "EVADC suspended by the poll", |s| ocs_sus(s, EVADC_OCS) == 2);
    assert_eq!(with_sim(&s, |sim| sim.mem_word(APP_MARKER)), APP_MARKER_DEBUGGER);
    assert_eq!(curstate(&s, "tc37x.cpu0"), "running");
    no_violations(&s);
}

/// Hot attach to an application that started without the debugger: no reset, no halt, and no write
/// the hardware refuses (STM0/1 are locked: left alone). A halt then makes the FS26 reset the board;
/// the scripts restore OCDS and the STM suspend at once, the application disarms the watchdog, and
/// the cores are halted again in the application.
#[test]
fn hot_attach_then_halt_while_armed_recovers() {
    let s = server();
    add_fs26(&s, SimFs26::new(false, true));
    with_sim(&s, |sim| sim.app_started_without_debugger());
    s.interp.eval("init").unwrap();
    assert_eq!(curstate(&s, "tc37x.cpu0"), "running");
    assert_eq!(ocs_sus(&s, STM0_OCS), 0, "STM0 is locked by the application");
    assert_eq!(ocs_sus(&s, EVADC_OCS), 2, "enabled module not suspended at the hot attach");
    no_violations(&s);

    s.interp.eval("halt 200").unwrap();
    ops::poll_all(&s.cx());
    assert_eq!(with_sim(&s, |sim| sim.fs26.as_ref().unwrap().resets), 1);
    assert_eq!(with_sim(&s, |sim| sim.mem_word(APP_MARKER)), APP_MARKER_DEBUGGER, "application did not see the debugger");
    assert!(with_sim(&s, |sim| !sim.fs26.as_ref().unwrap().armed));
    assert_eq!(curstate(&s, "tc37x.cpu0"), "halted");
    assert_eq!(ocs_sus(&s, STM0_OCS), 2, "STM0 suspend not written after the FS26 reset");
    no_violations(&s);
}

/// Hot attach to an application that saw the debugger: the first halt is watched, holds, and so
/// proves the watchdog disarmed - no reset.
#[test]
fn hot_attach_then_halt_that_holds_proves_disarmed() {
    let s = server();
    add_fs26(&s, SimFs26::new(false, false));
    s.interp.eval("init").unwrap();
    assert_eq!(s.interp.eval("set sbc_disarmed").unwrap(), "0");
    s.interp.eval("halt 200").unwrap();
    ops::poll_all(&s.cx());
    assert_eq!(curstate(&s, "tc37x.cpu0"), "halted");
    assert_eq!(s.interp.eval("set sbc_disarmed").unwrap(), "1");
    assert_eq!(with_sim(&s, |sim| sim.fs26.as_ref().unwrap().resets), 0);
    no_violations(&s);
}

/// Once disarmed, a reset outside the scripts that clears OCDS (FS26 reset, power cycle, a reset
/// while the connection was lost) makes the state unknown again: the next reset disarms again. An
/// application reset keeps OCDS: still disarmed, the next reset is done once.
#[test]
fn only_a_reset_that_cleared_ocds_needs_another_disarm() {
    let s = server();
    s.interp.eval("set sbc_rstb_wired auto").unwrap();
    add_fs26(&s, SimFs26::new(false, false));
    s.interp.eval("init").unwrap();
    s.interp.eval("reset run").unwrap();
    assert_eq!(s.interp.eval("set sbc_disarmed").unwrap(), "1");

    with_sim(&s, |sim| sim.application_reset());
    ops::poll_all(&s.cx());
    ops::poll_all(&s.cx());
    assert_eq!(s.interp.eval("set sbc_disarmed").unwrap(), "1", "an application reset keeps OCDS");

    // Nothing accessed the board for longer than the device watch's threshold: the reset is answered
    // late (the application may have checked for the debugger meanwhile).
    with_sim(&s, |sim| sim.external_reset());
    std::thread::sleep(std::time::Duration::from_millis(40));
    ops::poll_all(&s.cx());
    ops::poll_all(&s.cx());
    assert_eq!(s.interp.eval("set sbc_disarmed").unwrap(), "0", "OCDS was off after the reset");
    take_writes(&s);
    s.interp.eval("reset halt").unwrap();
    let w = take_writes(&s);
    let setups = w.iter().filter(|x| **x == (0xF000_0494, 0xB000_003F)).count();
    assert_eq!(setups, 2, "a running reset first, then the halting one: {w:08x?}");
    assert_eq!(s.interp.eval("set sbc_disarmed").unwrap(), "1");
    no_violations(&s);
}

fn reset_info(s: &Server, key: &str) -> String {
    s.interp.eval(&format!("dict get [reset_info] {key}")).unwrap()
}

/// A board reset while a Tcl command only waits (no background poll in between): the device watch
/// accesses the board every slice of the wait and switches OCDS on again before the application
/// checks for the debugger (25 ms after it starts). The FS26 policy then has no reason to disarm
/// again.
#[test]
fn reset_during_a_script_wait_is_answered_at_once() {
    let s = server();
    s.interp.eval("init").unwrap();
    s.interp.eval("reset run").unwrap();
    assert_eq!(reset_info(&s, "count"), "0", "the server's own reset is not a detected one");
    assert_eq!(reset_info(&s, "watching"), "1");

    with_sim(&s, |sim| sim.schedule_board_reset(std::time::Duration::from_millis(20)));
    s.interp.eval("sleep 150").unwrap();
    assert_eq!(with_sim(&s, |sim| sim.ostate & 1), 1, "OCDS not switched on during the wait");
    assert!(with_sim(&s, |sim| sim.app.as_ref().unwrap().saw_debugger), "the application missed the debugger");
    assert_eq!(reset_info(&s, "count"), "1");
    assert_eq!(reset_info(&s, "restored"), "1");
    assert_eq!(reset_info(&s, "late"), "0", "latency {} ms", reset_info(&s, "latency_ms"));

    // The poll still re-examines after the reset, but finds nothing that would re-arm the FS26.
    ops::poll_all(&s.cx());
    ops::poll_all(&s.cx());
    assert_eq!(s.interp.eval("tc37x.cpu0 was_examined").unwrap(), "1");
    assert_eq!(s.interp.eval("set sbc_disarmed").unwrap(), "1", "{}", s.interp.eval("set sbc_disarmed_why").unwrap());
    no_violations(&s);
}

/// The same reset with the device watch off: nothing answers it until the command has ended, the
/// application starts without the debugger, and the FS26 policy takes the watchdog as armed.
#[test]
fn reset_during_a_script_wait_without_the_watch_is_answered_by_the_poll() {
    let s = server();
    s.interp.eval("init").unwrap();
    s.interp.eval("reset_watch off").unwrap();
    s.interp.eval("reset run").unwrap();

    with_sim(&s, |sim| sim.schedule_board_reset(std::time::Duration::from_millis(20)));
    s.interp.eval("sleep 150").unwrap();
    assert_eq!(reset_info(&s, "count"), "0");
    ops::poll_all(&s.cx());
    ops::poll_all(&s.cx());
    assert!(!with_sim(&s, |sim| sim.app.as_ref().unwrap().saw_debugger));
    assert_eq!(with_sim(&s, |sim| sim.ostate & 1), 1, "the examination switches OCDS on");
    assert_eq!(s.interp.eval("set sbc_disarmed").unwrap(), "0");
}

/// The running reset before the first halting one fails (TasServer refused its reset connect): the
/// halting reset still resets the chip and halts at the reset vector, rather than halting the
/// application where it runs.
#[test]
fn halting_reset_after_a_failed_reset_still_resets() {
    let s = server();
    add_fs26(&s, SimFs26::new(false, false));
    s.interp.eval("init").unwrap();
    with_sim(&s, |sim| {
        sim.pc[0] = 0x8000_1234;
        sim.deassert_fails = 1;
    });
    s.interp.eval("reset halt").unwrap();
    assert_eq!(with_sim(&s, |sim| sim.deassert_fails), 0, "the running reset was not tried");
    assert_eq!(curstate(&s, "tc37x.cpu0"), "halted");
    assert_eq!(s.interp.eval("reg pc").unwrap(), "pc (/32): 0x800a8000");
    no_violations(&s);
}

/// sbc_attach halt on a disarmed application whose STM0/1 have no suspend (locked): a halt would make
/// the OS panic on resume, so it resets first, then halts in the application.
#[test]
fn attach_halt_resets_first_when_stm_is_not_suspended() {
    let s = server();
    s.interp.eval("attach_mode halt").unwrap();
    s.interp.eval("init").unwrap();
    assert_eq!(curstate(&s, "tc37x.cpu0"), "halted");
    assert_eq!(ocs_sus(&s, STM0_OCS), 2);
    assert_eq!(ocs_sus(&s, STM1_OCS), 2);
    // EVADC: suspended if the application had enabled it before the halt, else once it does.
    let evadc_clc = 0xF002_0000;
    assert!(ocs_sus(&s, EVADC_OCS) == 2 || mem(&s, evadc_clc) & 2 != 0, "EVADC enabled but not suspended at the halt");
    // Halted in the application (it ran: it locked STM0 against the debugger), not at the vector.
    assert_eq!(mem(&s, 0xF000_10FC), 0x4, "the application did not run before the halt");
    assert_eq!(mem(&s, APP_MARKER), APP_MARKER_DEBUGGER);
    s.interp.eval("resume").unwrap();
    wait_polling(&s, "EVADC suspended by the poll", |s| ocs_sus(s, EVADC_OCS) == 2);
    no_violations(&s);
}

/// Passive attach: nothing written to the target, not even by aurix-dbg's examination (OCDS stays
/// as it was); memory reads work; resets are refused.
#[test]
fn passive_attach_writes_nothing() {
    let s = server();
    s.interp.eval("attach_mode passive").unwrap();
    s.interp.eval("init").unwrap();
    ops::poll_all(&s.cx());
    assert_eq!(take_writes(&s), vec![]);
    assert_eq!(with_sim(&s, |sim| sim.ostate & 1), 0, "OCDS enabled");
    assert_eq!(mem(&s, APP_MARKER), APP_MARKER_DEBUGGER);
    assert!(s.interp.eval("reset run").is_err());
    assert_eq!(take_writes(&s), vec![]);
}

#[test]
fn debugger_reset_reapplies_setup_and_latches_tl1_when_halted() {
    let s = server();
    s.interp.eval("init").unwrap();
    take_writes(&s);
    s.interp.eval("reset halt").unwrap();
    let w = take_writes(&s);
    assert!(w.contains(&(0xF000_0494, 0xB000_003F)), "setup not redone after reset halt: {w:08x?}");
    // The server suspends the STMs right after cpu0's deassert, before any handler; the quick
    // reset-end then releases the HSM (boot ROM breakpoint deleted, then run: it needs ~32 ms to
    // report ready); once the halt holds, TL1 is routed and latched (its edge halts the HSM with the
    // cores; continue releases it as after any halt).
    let ocs = pos(&w, (0xF000_10E8, 0x1200_0000));
    let bp = pos(&w, (0xF005_2008, 0));
    let run = bp + pos(&w[bp..], (0xF005_EDF0, 0xA05F_0001));
    let latch = w.iter().rposition(|x| *x == (CBS_TLC, 0x30)).expect("TL1 not latched after reset halt");
    assert!(ocs < bp && bp < run, "STM suspend, then HSM release: {w:08x?}");
    assert!(run < latch, "TL1 latched once the halt holds: {w:08x?}");
    assert_eq!(s.interp.eval("tc37x.cpu0 curstate").unwrap(), "halted");

    // A reset without halt: the set-up is redone right after it, nothing holds the cores.
    s.interp.eval("reset run").unwrap();
    let w = take_writes(&s);
    assert!(w.contains(&(0xF000_0494, 0xB000_003F)), "setup not redone after reset run: {w:08x?}");
    assert!(w.contains(&(0xF000_10E8, 0x1200_0000)), "STM0 not suspended after reset run: {w:08x?}");
    assert!(!with_sim(&s, |sim| sim.tl1_held), "TL1 still held after reset run: {w:08x?}");
    ops::poll_all(&s.cx());
    assert_eq!(s.interp.eval("tc37x.cpu0 curstate").unwrap(), "running");
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

/// The HSM bridge only takes aligned word accesses: host reads and writes of it are split into
/// single words instead of the 64-bit blocks and byte edges of other memory.
#[test]
fn hsm_bridge_is_accessed_word_by_word() {
    let s = server();
    s.interp.eval("init").unwrap();
    with_sim(&s, |sim| {
        for (i, a) in (0xF004_0020..0xF004_0040).step_by(4).enumerate() {
            sim.set_mem(a, 0x1111_1111 * i as u32);
        }
        sim.reads.clear();
    });
    // Unaligned and longer than a word (HT2HSMF at 0x20 ... HSM2HTS at 0x34).
    let data = ops::read_memory(&s.cx(), 0xF004_0022, 0x14).unwrap();
    let words: Vec<u8> = (0..8u32).flat_map(|i| (0x1111_1111 * i).to_le_bytes()).collect();
    assert_eq!(data, words[2..0x16]);
    let reads = with_sim(&s, |sim| std::mem::take(&mut sim.reads));
    assert_eq!(reads, (0xF004_0020..0xF004_0038).step_by(4).map(|a| (a, 4)).collect::<Vec<_>>());

    take_writes(&s);
    ops::write_memory(&s.cx(), 0xF004_0030, &[1, 0, 0, 0, 2, 0, 0, 0]).unwrap();
    assert_eq!(take_writes(&s), vec![(0xF004_0030, 1), (0xF004_0034, 2)]);
    // A partial word is refused rather than merged by read-modify-write.
    let err = ops::write_memory(&s.cx(), 0xF004_0022, &[1, 2]).unwrap_err().to_string();
    assert!(err.contains("32-bit aligned"), "{err}");
    // Other host memory is unaffected.
    ops::read_memory(&s.cx(), 0x7000_0001, 3).unwrap();
}

/// HSM peripherals have no host address: accesses from a TriCore context go to the HSM, while
/// HSM RAM stays HSM-context only (it needs a halted HSM and a cache clean).
#[test]
fn hsm_peripherals_are_read_through_the_hsm_from_every_core() {
    let s = server();
    s.interp.eval("init").unwrap();
    let cx = s.cx();
    assert_eq!(ops::own_access_target(&cx, Some(0), 0xE800_0014, 4), Some(HSM));
    assert_eq!(ops::own_access_target(&cx, None, 0xEC00_0200, 0x14), Some(HSM));
    assert_eq!(ops::own_access_target(&cx, Some(0), 0x2000_0000, 4), None);
    assert_eq!(ops::own_access_target(&cx, Some(HSM), 0x2000_0000, 4), Some(HSM));
    assert_eq!(ops::own_access_target(&cx, Some(0), 0xEFFF_FFFC, 8), None, "range leaves the peripheral region");
    assert_eq!(ops::own_access_target(&cx, Some(0), 0x7000_0000, 4), None);

    with_sim(&s, |sim| sim.hsm.mem.insert(0xE800_0014, 0x0000_0006));
    take_writes(&s);
    assert_eq!(ops::read_memory_as(&cx, HSM, 0xE800_0014, 4).unwrap(), vec![6, 0, 0, 0]);
    assert!(take_writes(&s).contains(&(HSM_DBGBASE, 0xE800_0000)));
}

/// Unused addresses inside an HSM peripheral give bus errors (TIM, WDT), and a failing access also
/// fails the rest of its transaction: a read across them only fails those words, so the peripheral
/// view still shows the registers around them. A word that fails once is read again.
#[test]
fn hsm_peripheral_read_fails_only_the_unused_words() {
    let s = server();
    s.interp.eval("init").unwrap();
    with_sim(&s, |sim| {
        sim.hsm.bus_errors.extend([0xEC00_0008, 0xEC00_000C, 0xEC00_0018]);
        sim.hsm.flaky.insert(0xEC00_0010, 2);
        sim.hsm.mem.insert(0xEC00_0010, 0x1234);
        sim.hsm.mem.insert(0xEC00_0020, 0x5678);
    });
    let r = ops::read_memory_partial_as(&s.cx(), HSM, 0xEC00_0002, 0x20).unwrap();
    let e: Vec<(u32, usize)> = r.unreadable.iter().map(|(a, n, _)| (*a, *n)).collect();
    assert_eq!(e, vec![(0xEC00_0008, 8), (0xEC00_0018, 4)]);
    assert!(r.unreadable[0].2.contains("HSM address"), "{:?}", r.unreadable);
    assert_eq!(r.data.len(), 0x20);
    assert_eq!(r.data[0x0E..0x10], [0x34, 0x12]);
    assert_eq!(r.data[0x1E..0x20], [0x78, 0x56]);
    assert_eq!(with_sim(&s, |sim| sim.hsm.flaky[&0xEC00_0010]), 0);
    // The all-or-nothing read still fails.
    assert!(ops::read_memory_as(&s.cx(), HSM, 0xEC00_0000, 0x28).is_err());
}

/// Host reads across bridge registers only the HSM may access (HT2HSMIE) fail only those words.
#[test]
fn hsm_bridge_read_fails_only_the_hsm_side_registers() {
    let s = server();
    s.interp.eval("init").unwrap();
    with_sim(&s, |sim| {
        sim.hsm.bus_errors.insert(0xF004_0024);
        sim.set_mem(0xF004_0034, 3);
    });
    let r = ops::read_memory_partial(&s.cx(), 0xF004_0020, 0x1C).unwrap();
    let e: Vec<(u32, usize)> = r.unreadable.iter().map(|(a, n, _)| (*a, *n)).collect();
    assert_eq!(e, vec![(0xF004_0024, 4)]);
    assert_eq!(r.data[0x14..0x18], [3, 0, 0, 0]);
    assert!(ops::read_memory(&s.cx(), 0xF004_0020, 0x1C).is_err());
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

/// A target whose poll fails right after every successful re-examination (the HSM while the device
/// is in reset) counts as one failure: it is not reported as recovered and is retried less often.
#[test]
fn target_failing_after_each_reexamination_is_retried_slowly() {
    let s = server();
    s.interp.eval("init").unwrap();
    let hsm = s.cx().with(|c| c.targets.iter().position(|e| e.name == "tc37x.hsm").unwrap());
    let entry = |s: &Server| s.cx().with(|c| (c.targets[hsm].poll_failures, c.targets[hsm].recovering, c.targets[hsm].flapping));
    let access = |s: &Server, on: bool| with_sim(s, |sim| sim.hsm.debug_enabled = on);
    let poll_until_failed = |s: &Server| {
        let t0 = std::time::Instant::now();
        while entry(s).0 == 0 {
            assert!(t0.elapsed() < std::time::Duration::from_secs(2), "poll failure not detected");
            ops::poll_all(&s.cx());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    };

    // A single failed poll is ignored.
    access(&s, false);
    ops::poll_all(&s.cx());
    access(&s, true);
    ops::poll_all(&s.cx());
    assert_eq!(entry(&s), (0, None, false));
    access(&s, false);
    poll_until_failed(&s);
    // Examined again, but the polls fail again: still the same failure.
    access(&s, true);
    ops::poll_all(&s.cx());
    assert_eq!(s.interp.eval("tc37x.hsm was_examined").unwrap(), "1");
    assert!(entry(&s).1.is_some());
    access(&s, false);
    poll_until_failed(&s);
    let (failures, recovering, flapping) = entry(&s);
    assert!(failures > 1 && recovering.is_none() && flapping, "{:?}", entry(&s));
    // Not re-examined before FLAP_RETRY.
    access(&s, true);
    ops::poll_all(&s.cx());
    assert_eq!(s.interp.eval("tc37x.hsm was_examined").unwrap(), "0");
    // Recovered once a poll after the re-examination succeeds.
    s.cx().with(|c| c.targets[hsm].last_examine = None);
    ops::poll_all(&s.cx());
    ops::poll_all(&s.cx());
    assert_eq!(entry(&s), (0, None, false));
    assert_eq!(s.interp.eval("tc37x.hsm was_examined").unwrap(), "1");
}

/// A target inaccessible for UNREACHABLE_TIME is flagged for the frontends until it is examined
/// again.
#[test]
fn inaccessible_target_is_flagged_unreachable() {
    let s = server();
    s.interp.eval("init").unwrap();
    let hsm = s.cx().with(|c| c.targets.iter().position(|e| e.name == "tc37x.hsm").unwrap());
    let unreachable = |s: &Server| s.cx().with(|c| c.targets[hsm].unreachable);
    with_sim(&s, |sim| sim.hsm.debug_enabled = false);
    let t0 = std::time::Instant::now();
    while !unreachable(&s) {
        assert!(t0.elapsed() < std::time::Duration::from_secs(3), "not flagged unreachable");
        ops::poll_all(&s.cx());
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(t0.elapsed() >= aurix_core::target::UNREACHABLE_TIME);
    with_sim(&s, |sim| sim.hsm.debug_enabled = true);
    s.cx().with(|c| c.targets[hsm].last_examine = None);
    ops::poll_all(&s.cx());
    assert!(!unreachable(&s));
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

/// Polls (as the server loop does, every 5 ms) until `f` holds, for up to 2 s.
fn wait_polling(s: &Server, what: &str, f: impl Fn(&Server) -> bool) {
    let t0 = std::time::Instant::now();
    while t0.elapsed() < std::time::Duration::from_secs(2) {
        ops::poll_all(&s.cx());
        if f(s) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("timed out waiting for {what}");
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

/// MCS channels are targets only when asked for, also from a launch configuration's commands
/// after the cfg files.
#[test]
fn mcs_targets_are_off_by_default_and_can_be_set_after_the_cfg_files() {
    let s = server_with_mcs(false);
    let i = &s.interp;
    assert!(!i.eval("target names").unwrap().contains(".mcs"));
    i.eval_global("set tc3xx_mcs_modules {1}").unwrap();
    i.eval("init").unwrap();
    let names = i.eval("target names").unwrap();
    assert!(names.contains("tc37x.mcs1.ch0") && names.ends_with("tc37x.mcs1.ch7") && !names.contains(".mcs0"), "{names}");
    assert_eq!(i.eval("target current").unwrap(), "tc37x.cpu0");
    assert_eq!(i.eval("llength $tc3xx_mcs_targets").unwrap(), "8");
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
    s.cx().with(|c| c.search_dirs.push(PathBuf::from(openocd_tcl::TCL_DIR)));
    let i = &s.interp;
    i.eval_global("adapter driver sim; script target/infineon/tc39x.cfg").unwrap();
    i.eval_global("set tc3xx_mcs_modules {0 9}; set tc3xx_mcs_ramsize 0x1800").unwrap();
    i.eval_global(&format!("script {}", jim::quote(&tcl_path(repo().join("tc3xx_ocds.cfg"))))).unwrap();
    i.eval_global("tc3xx_setup tc39x").unwrap();
    assert_eq!(i.eval("set tc3xx_cores").unwrap(), "tc39x.cpu0 tc39x.cpu1 tc39x.cpu2 tc39x.cpu3 tc39x.cpu4 tc39x.cpu5");
    assert_eq!(i.eval("set tc3xx_dbgsr(tc39x.cpu5)").unwrap(), "0xF88DFD00");
    assert_eq!(i.eval("set tc3xx_coreid(tc39x.cpu5)").unwrap(), "5");
    assert_eq!(i.eval("format 0x%08X [tc3xx_cbs_trc 5]").unwrap(), "0xF0000434");
    assert_eq!(i.eval("tc39x.cpu5 cget -event halted").unwrap(), "tc3xx_on_halted tc39x.cpu5");
    assert_eq!(i.eval("ocds sync").unwrap(), "tl1");
    assert_eq!(i.eval("tc39x.hsm cget -type").unwrap(), "cortex_m");
    assert_eq!(i.eval("tc39x.mcs9.ch7 cget -ramsize").unwrap(), "0x1800");
    assert_eq!(i.eval("target current").unwrap(), "tc39x.cpu0");
    assert!(i.eval("catch {target create x gtm_mcs -module 10}").unwrap() == "1");
    assert!(i.eval("catch {tc3xx_setup tc27x}").unwrap() == "1");
}

/// The cfg drives OCDS itself on the TAS and simulator backends, and steps
/// aside on one whose probe owns OCDS -- without losing the target topology.
mod backend_guard {
    use super::*;

    fn server_with(driver: &str) -> Server {
        aurix_core::log::set_debug_level(1);
        let s = Server::new();
        s.cx().with(|c| {
            c.adapter_override = Some(driver.into());
            c.search_dirs.push(PathBuf::from(openocd_tcl::TCL_DIR));
        });
        s.interp.eval_global("script board/infineon/kit_a2g_tc375_lite.cfg").unwrap();
        s.interp.eval_global(aurix_core::sim::SIM_BOARD_SETTINGS).unwrap();
        s.interp
            .eval_global(&format!("script {}", jim::quote(&tcl_path(repo().join("tc37x_ocds_init.cfg")))))
            .unwrap();
        s
    }

    #[test]
    fn ocds_backends_keep_the_full_event_wiring() {
        let s = server_with("sim");
        let i = &s.interp;
        assert_eq!(i.eval("adapter name").unwrap(), "sim");
        assert_eq!(i.eval("tc3xx_ocds_available").unwrap(), "1");
        assert_eq!(i.eval("tc37x.cpu0 cget -event examine-end").unwrap(), "tc3xx_on_examine");
        assert_eq!(i.eval("tc37x.cpu0 cget -event reset-end").unwrap(), "tc3xx_on_reset_end");
        assert_eq!(i.eval("tc37x.cpu1 cget -event halted").unwrap(), "tc3xx_on_halted tc37x.cpu1");
        assert_eq!(i.eval("ocds sync").unwrap(), "tl1");
    }

    /// The winIDEA backend skips the OCDS handlers and leaves the HSM to
    /// winIDEA, but must still create the TriCore and MCS targets.
    ///
    /// Seen on a BlueBox: with our HSM target examined (halting debug enabled
    /// on the HSM), halting cpu1 through winIDEA also halted the HSM through
    /// TL1, and nothing resumed it.
    #[test]
    fn winidea_skips_ocds_handlers_and_leaves_the_hsm_to_winidea() {
        let s = server_with("winidea");
        let i = &s.interp;
        assert_eq!(i.eval("adapter name").unwrap(), "winidea");
        assert_eq!(i.eval("tc3xx_ocds_available").unwrap(), "0");

        // No OCDS event handler is wired, reset-end included.
        for (target, event) in [
            ("tc37x.cpu0", "examine-end"),
            ("tc37x.cpu0", "reset-end"),
            ("tc37x.cpu0", "halted"),
            ("tc37x.cpu0", "resume-start"),
            ("tc37x.cpu0", "resume-end"),
            ("tc37x.cpu0", "step-start"),
            ("tc37x.cpu1", "halted"),
        ] {
            let got = i.eval(&format!("{target} cget -event {event}")).unwrap();
            assert_eq!(got, "", "{target} still has a {event} handler");
        }

        let names = i.eval("target names").unwrap();
        assert!(!names.contains("tc37x.hsm"), "the HSM is winIDEA's core here: {names}");
        // MCS channels are polled through memory reads, ~5 ms each over winIDEA.
        assert!(!names.contains(".mcs"), "no MCS targets on winIDEA: {names}");
        for t in ["tc37x.cpu0", "tc37x.cpu1", "tc37x.cpu2"] {
            assert!(names.contains(t), "{t} missing from: {names}");
        }
        assert_eq!(i.eval("llength $tc3xx_mcs_targets").unwrap(), "0");
        // And the globals every other proc reads are still set.
        assert_eq!(i.eval("llength $tc3xx_cores").unwrap(), "3");
    }
}

// --- SCR (xc800 target) ---------------------------------------------------------------------------

/// The board cfg with `tc3xx_scr_target 1`.
fn scr_server() -> Server {
    aurix_core::log::set_debug_level(1);
    let s = Server::new();
    s.cx().with(|c| {
        c.adapter_override = Some("sim".into());
        c.search_dirs.push(PathBuf::from(openocd_tcl::TCL_DIR));
    });
    s.interp.eval_global("set tc3xx_scr_target 1").unwrap();
    s.interp.eval_global("script board/infineon/kit_a2g_tc375_lite.cfg").unwrap();
    s.interp.eval_global(aurix_core::sim::SIM_BOARD_SETTINGS).unwrap();
    s.interp
        .eval_global(&format!("script {}", jim::quote(&tcl_path(repo().join("tc37x_ocds_init.cfg")))))
        .unwrap();
    s.interp.eval_global("hsm ready_ms 0").unwrap();
    s.interp.eval_global("set sbc_rstb_wired 0; set sbc_disarmed 1").unwrap();
    s
}

fn scr_index(s: &Server) -> usize {
    s.cx().with(|c| c.targets.iter().position(|e| e.name == "tc37x.scr").expect("tc37x.scr exists"))
}

#[test]
fn scr_target_waits_for_the_application() {
    let s = server();
    assert!(!s.interp.eval("target names").unwrap().contains(".scr"), "off by default");

    let s = scr_server();
    let i = &s.interp;
    assert_eq!(i.eval("tc37x.scr cget -type").unwrap(), "xc800");
    assert_eq!(i.eval("tc37x.scr cget -coreid").unwrap(), "7");
    i.eval("init").unwrap();
    // The application has not started the SCR in its OCDS boot mode: not examined, the rest works.
    assert_eq!(i.eval("tc37x.scr was_examined").unwrap(), "0");
    assert_eq!(i.eval("tc37x.cpu0 was_examined").unwrap(), "1");
    with_sim(&s, |sim| sim.scr.enabled = true);
    i.eval("tc37x.scr arp_examine").unwrap();
    // Attaching stops it at the boot ROM's start breakpoint; no TL1 or OCDS writes for it.
    assert_eq!(i.eval("tc37x.scr curstate").unwrap(), "halted");
    let t = scr_index(&s);
    assert_eq!(ops::state(&s.cx(), t).1.name(), "debug-request");
    // Refused at init, then attached.
    assert_eq!(with_sim(&s, |sim| sim.scr.calls.clone()), vec!["attach", "attach"]);
}

#[test]
fn scr_registers_memory_step_and_breakpoints() {
    let s = scr_server();
    let i = &s.interp;
    with_sim(&s, |sim| sim.scr.enabled = true);
    i.eval("init").unwrap();
    i.eval("targets tc37x.scr").unwrap();
    let t = scr_index(&s);
    assert_eq!(i.eval("reg pc").unwrap(), "pc (/32): 0x00000000");
    assert!(i.eval("reg").unwrap().starts_with("===== xc800 registers"));
    // Code and IRAM through the SCR's virtual address map.
    assert_eq!(ops::read_memory_as(&s.cx(), t, 0, 3).unwrap(), vec![0x02, 0x00, 0x79]);

    i.eval("step").unwrap();
    i.eval("step").unwrap();
    assert_eq!(i.eval("reg pc").unwrap(), "pc (/32): 0x0000007c");
    assert_eq!(i.eval("reg sp").unwrap(), "sp (/32): 0x00000020");
    assert_eq!(ops::state(&s.cx(), t).1.name(), "single-step");

    // R7 of bank 0 is IRAM 0x07.
    i.eval("reg r7 0x41").unwrap();
    assert_eq!(with_sim(&s, |sim| sim.scr.iram[7]), 0x41);

    // A hardware breakpoint in the subroutine.
    i.eval("bp 0x101 4 hw").unwrap();
    assert_eq!(i.eval("xc800 triggers").unwrap().lines().next().unwrap(), "HWBP0: on  addr=0x0101");
    i.eval("resume").unwrap();
    i.eval("wait_halt 1000").unwrap();
    assert_eq!(ops::state(&s.cx(), t).1.name(), "breakpoint");
    assert_eq!(i.eval("reg pc").unwrap(), "pc (/32): 0x00000101");
    assert_eq!(i.eval("reg r7").unwrap(), "r7 (/32): 0x00000042", "INC R7 ran");
    i.eval("rbp all").unwrap();
    assert_eq!(with_sim(&s, |sim| sim.scr.bps), [None; 4]);

    // Step over the call at 0x7C: runs to a temporary breakpoint behind it, which goes again.
    i.eval("reg pc 0x7c").unwrap();
    assert!(ops::step_as(&s.cx(), t, None, aurix_core::target::RunMode::StepOver).is_err(), "still running");
    poll_until(&s, "the step over", |s| ops::state(&s.cx(), t).0 == aurix_core::target::TargetState::Halted);
    assert_eq!(i.eval("reg pc").unwrap(), "pc (/32): 0x0000007f");
    assert_eq!(ops::state(&s.cx(), t).1.name(), "single-step");
    assert_eq!(with_sim(&s, |sim| sim.scr.bps), [None; 4]);

    // Halt while running.
    i.eval("resume").unwrap();
    assert_eq!(i.eval("tc37x.scr curstate").unwrap(), "running");
    i.eval("halt 1000").unwrap();
    assert_eq!(i.eval("tc37x.scr curstate").unwrap(), "halted");
    assert_eq!(ops::state(&s.cx(), t).1.name(), "debug-request");
    // The TriCore cores were not stopped with it (the SCR is not on TL1).
    assert_eq!(i.eval("tc37x.cpu0 curstate").unwrap(), "running");
}

/// The SCR's SFRs (virtual addresses 0x40000-0x7FFFF, paged ones included) have no host address
/// (segment 0 is reserved on the TriCore side): the peripheral view reads them through the TriCore
/// context, and they go to the SCR.
#[test]
fn scr_sfrs_are_read_through_the_scr_from_every_core() {
    let s = server();
    s.interp.eval("init").unwrap();
    assert_eq!(ops::own_access_target(&s.cx(), Some(0), 0x4_7176, 1), None, "no SCR target");

    let s = scr_server();
    with_sim(&s, |sim| sim.scr.enabled = true);
    s.interp.eval("init").unwrap();
    let cx = s.cx();
    let scr = scr_index(&s);
    // TCINTEXCHG: SCU_PAGE (0xF1) page 0, SFR 0xF6.
    let tcint = aurix_core::adapter::scr_space::paged_sfr(0xF1, 0, 0xF6);
    assert_eq!(ops::own_access_target(&cx, Some(0), tcint, 1), Some(scr));
    assert_eq!(ops::own_access_target(&cx, None, 0x4_0000, 0x80), Some(scr));
    assert_eq!(ops::own_access_target(&cx, Some(0), 0x7_FFFC, 8), None, "range leaves the window");
    assert_eq!(ops::own_access_target(&cx, Some(0), 0x3_FFFF, 1), None, "XDATA/IRAM only through the SCR context");
    assert_eq!(ops::own_access_target(&cx, Some(scr), 0x2_0000, 1), Some(scr));
    with_sim(&s, |sim| sim.scr.sfr.insert(tcint, 0xB4));
    assert_eq!(ops::read_memory_partial_as(&cx, scr, tcint - 1, 2).unwrap().data, vec![0, 0xB4]);
    // Only while halted: the monitor serves the accesses.
    s.interp.eval("targets tc37x.scr; resume").unwrap();
    let err = ops::read_memory_as(&cx, scr, tcint, 1).unwrap_err().to_string();
    assert!(err.contains("running"), "{err}");
}

/// A launch configuration sets `tc3xx_scr_target` in its commands, after the cfg files: the SCR
/// target is created by init_targets, before the probe connects.
#[test]
fn scr_target_set_after_the_cfg_files() {
    let s = server();
    let i = &s.interp;
    assert!(!i.eval("target names").unwrap().contains(".scr"));
    i.eval_global("set tc3xx_scr_target 1").unwrap();
    with_sim(&s, |sim| sim.scr.enabled = true);
    i.eval("init").unwrap();
    assert!(i.eval("target names").unwrap().contains("tc37x.scr"));
    assert_eq!(i.eval("target current").unwrap(), "tc37x.cpu0");
    assert_eq!(i.eval("tc37x.scr was_examined").unwrap(), "1");
    assert_eq!(i.eval("tc37x.scr curstate").unwrap(), "halted");
}

/// An Intel HEX file in the temp folder, as a quoted Tcl path.
fn hex_file(name: &str, img: &aurix_core::image::Image) -> String {
    let p = std::env::temp_dir().join(format!("aurix-{name}-{}.hex", std::process::id()));
    std::fs::write(&p, img.to_ihex()).unwrap();
    jim::quote(&tcl_path(p))
}

fn pattern(words: u32, seed: u32) -> Vec<u8> {
    (0..words).flat_map(|i| (seed.wrapping_mul(0x0100_0193) ^ i.wrapping_mul(0x9E37_79B9)).to_le_bytes()).collect()
}

const CBS_TL1ST: u32 = 0xF000_0494;
const CPU0_D0: u32 = 0xF881_FF00;
const CPU0_A4: u32 = 0xF881_FF90;

/// cpu0's d0, a4, PC (the simulator's) and CBS_TL1ST, which the loader borrows and gives back.
fn loader_state(s: &Server) -> (u32, u32, u32, u32) {
    let tl1st = mem(s, CBS_TL1ST);
    with_sim(s, |sim| (sim.mem_word(CPU0_D0), sim.mem_word(CPU0_A4), sim.pc[0], tl1st))
}

#[test]
fn flash_loader_programs_through_cpu0() {
    use aurix_core::image::{Image, Section};
    let s = server();
    s.interp.eval("init").unwrap();
    assert_eq!(s.interp.eval("flash loader").unwrap(), "on");
    s.interp.eval("halt").unwrap();
    with_sim(&s, |sim| {
        sim.set_mem(CPU0_D0, 0x1111_1111);
        sim.set_mem(CPU0_A4, 0x2222_2222);
    });
    let before = loader_state(&s);
    assert_eq!(before.3 & 1, 1, "the cfg puts cpu0 on trigger line 1");
    // Off a burst boundary, across 8 KB slots and three sectors, and a second bank.
    let image = |seed: u32| {
        Image::from_sections(vec![
            Section { addr: 0x8000_4040, data: pattern(10_000, seed) },
            Section { addr: 0x8030_0000, data: pattern(64, seed + 1) },
        ])
    };
    let file = hex_file("loader-a", &image(1));
    let out = s.interp.eval(&format!("flash write_images erase verify {file}")).unwrap();
    assert!(out.contains("4 sectors: 0 unchanged, 4 programmed (0 erased first)") && out.contains("(RAM loader)"), "{out}");
    assert!(out.contains("verified 40256 bytes"), "{out}");
    assert_eq!(with_sim(&s, |sim| sim.cpu0_fault.clone()), None);
    assert_eq!(loader_state(&s), before, "cpu0's registers and its TL1 routing are back");
    assert_eq!(mem(&s, 0xF003_62A8) & 3, 3, "Safety ENDINIT is set again");
    assert!(with_sim(&s, |sim| sim.halted[0] && sim.cpu0.is_none()));

    // The same image again: unchanged; another one: the sectors are erased and programmed.
    let out = s.interp.eval(&format!("flash write_images erase incremental {file}")).unwrap();
    assert!(out.contains("4 sectors: 4 unchanged"), "{out}");
    // An all-zero page where the flash is still erased reads as zeros: the loader's checksum must
    // not take it for unchanged data (Verify Erased Page).
    let mut grown = image(1);
    grown.sections.push(Section { addr: 0x8000_4000, data: vec![0; 32] });
    let out = s.interp.eval(&format!("flash write_images erase incremental {}", hex_file("loader-zero", &grown))).unwrap();
    assert!(out.contains("4 sectors: 3 unchanged, 1 programmed (1 erased first)"), "{out}");
    assert!(with_sim(&s, |sim| sim.dmu.pflash_pages.contains_key(&0xA000_4000)), "the zero page is programmed");
    // Changed data: every sector is programmed again.
    let out = s.interp.eval(&format!("flash write_images erase incremental {}", hex_file("loader-changed", &image(9)))).unwrap();
    assert!(out.contains("4 sectors: 0 unchanged, 4 programmed (4 erased first)"), "{out}");
    let execs = |s: &Server| with_sim(s, |sim| sim.execs);
    let e0 = execs(&s);
    let out = s.interp.eval(&format!("flash write_images erase verify {}", hex_file("loader-b", &image(2)))).unwrap();
    assert!(out.contains("4 programmed (4 erased first)") && out.contains("verified 40256 bytes"), "{out}");
    let with_loader = execs(&s) - e0;

    // The DMU over the debug interface takes many more round trips for the same work.
    assert_eq!(s.interp.eval("flash loader off").unwrap(), "off");
    let e0 = execs(&s);
    let out = s.interp.eval(&format!("flash write_images erase verify {}", hex_file("loader-c", &image(3)))).unwrap();
    assert!(out.contains("4 programmed (4 erased first)") && !out.contains("RAM loader"), "{out}");
    let without = execs(&s) - e0;
    assert!(with_loader * 3 < without, "round trips with the loader {with_loader}, without {without}");
    assert!(s.interp.eval("catch {flash loader maybe} e; set e").unwrap().starts_with("usage"));
    no_violations(&s);
}

#[test]
fn flash_loader_falls_back_when_the_watchdogs_stay_on() {
    use aurix_core::image::{Image, Section};
    let s = server();
    s.interp.eval("init").unwrap();
    s.interp.eval("halt").unwrap();
    with_sim(&s, |sim| sim.wdt_stuck = true);
    let before = loader_state(&s);
    let file = hex_file("loader-wdt", &Image::from_sections(vec![Section { addr: 0x8000_0000, data: pattern(64, 5) }]));
    let out = s.interp.eval(&format!("flash write_images erase verify {file}")).unwrap();
    assert!(out.contains("1 programmed") && !out.contains("RAM loader") && out.contains("verified 256 bytes"), "{out}");
    assert_eq!(loader_state(&s), before);
    assert!(with_sim(&s, |sim| sim.halted[0] && sim.cpu0.is_none()));
}

#[test]
fn flash_loader_reports_a_failed_program_command() {
    use aurix_core::image::{Image, Section};
    let s = server();
    s.interp.eval("init").unwrap();
    s.interp.eval("halt").unwrap();
    let before = loader_state(&s);
    // A page with data in a sector the erase check finds erased: programming it fails.
    with_sim(&s, |sim| {
        sim.dmu.pflash_pages.insert(0xA000_8000, vec![0; 32]);
    });
    let file = hex_file("loader-fail", &Image::from_sections(vec![Section { addr: 0x8000_8000, data: pattern(64, 6) }]));
    let e = s.interp.eval(&format!("catch {{flash write_image erase {file}}} e; set e")).unwrap();
    assert!(e.contains("flash program at 0x80008000 failed: program verify error"), "{e}");
    assert_eq!(loader_state(&s), before);
    assert!(with_sim(&s, |sim| sim.halted[0] && sim.cpu0.is_none() && sim.cpu0_fault.is_none()));
}

/// An application that protects cpu0's DSPR against the debugger (its writes are dropped without an
/// error, seen on a TC375 Lite Kit): the loader's slots go to cpu0's PSPR behind the code.
#[test]
fn flash_loader_uses_the_pspr_when_the_dspr_is_protected() {
    use aurix_core::image::{Image, Section};
    let s = server();
    s.interp.eval("init").unwrap();
    s.interp.eval("halt").unwrap();
    with_sim(&s, |sim| sim.debugger_dropped.push(0x7000_0000..0x7010_0000));
    let before = loader_state(&s);
    let img = Image::from_sections(vec![Section { addr: 0x8000_4040, data: pattern(10_000, 11) }]);
    let file = hex_file("loader-pspr", &img);
    let out = s.interp.eval(&format!("flash write_images erase verify {file}")).unwrap();
    assert!(out.contains("3 programmed") && out.contains("(RAM loader)") && out.contains("verified 40000 bytes"), "{out}");
    let out = s.interp.eval(&format!("flash write_images erase incremental {file}")).unwrap();
    assert!(out.contains("3 sectors: 3 unchanged"), "{out}");
    assert_eq!(loader_state(&s), before);
    assert_eq!(with_sim(&s, |sim| sim.cpu0_fault.clone()), None);
}
