//! Reverse debugging against the simulator: checkpoints before writes and steps, Step Back by
//! whole actions (restore by difference), the history cleared by a reset, and the trace owned by
//! the mode.

use std::path::PathBuf;

use aurix_core::Server;
use serde_json::Value;

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize().unwrap()
}

fn tcl_path(p: PathBuf) -> String {
    p.to_string_lossy().trim_start_matches(r"\\?\").replace('\\', "/")
}

fn server() -> Server {
    let s = server_running();
    s.interp.eval_global("halt").unwrap();
    s
}

/// The board cfgs, initialised, CPU0 selected and running.
fn server_running() -> Server {
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
    s
}

fn tcl(s: &Server, cmd: &str) -> String {
    s.interp.eval_global(cmd).unwrap_or_else(|e| panic!("{cmd}: {e}"))
}

fn json(s: &Server, cmd: &str) -> Value {
    serde_json::from_str(&tcl(s, cmd)).unwrap()
}

fn word(s: &Server, addr: u32) -> u32 {
    let v = tcl(s, &format!("read_memory {addr:#x} 32 1"));
    u32::from_str_radix(v.trim_start_matches("0x"), 16).unwrap()
}

fn reg(s: &Server, name: &str) -> u32 {
    let v = tcl(s, &format!("reg {name}"));
    let hex = v.rsplit("0x").next().unwrap().trim();
    u32::from_str_radix(hex, 16).unwrap()
}

#[test]
fn writes_and_steps_are_undone_action_by_action() {
    let s = server();
    tcl(&s, "mww 0x70000010 0x5");
    tcl(&s, "reg d4 0x10");
    let on = json(&s, "reverse on -notrace -region 0x70000000 0x100 -region 0x70001000 0x40 -private -json");
    assert_eq!(on["snapshot_bytes"], 0x140);

    tcl(&s, "mww 0x70000010 0x6");
    tcl(&s, "reg d4 0x20");
    tcl(&s, "mww 0x70001004 0xAB");
    let h = json(&s, "reverse history -json");
    assert_eq!(h.as_array().unwrap().len(), 3);
    assert_eq!(h[0]["kind"], "write");
    assert_eq!(h[1]["kind"], "write register");

    // Back before the private write, then before the register write, then before the first write.
    let b = json(&s, "reverse back -json");
    assert_eq!(b["action"], "write");
    assert_eq!(word(&s, 0x7000_1004), 0);
    assert_eq!(reg(&s, "d4"), 0x20);
    let b = json(&s, "reverse back -json");
    assert_eq!(b["action"], "write register");
    assert_eq!(b["registers"], serde_json::json!(["d4"]));
    assert_eq!(reg(&s, "d4"), 0x10);
    assert_eq!(word(&s, 0x7000_0010), 6);
    json(&s, "reverse back -json");
    assert_eq!(word(&s, 0x7000_0010), 5);
    assert!(s.interp.eval_global("reverse back").is_err(), "no history before the first checkpoint");

    // A new action from the restored state reuses its checkpoint; a write outside the regions is
    // saved byte by byte.
    tcl(&s, "mww 0x70002000 0x77");
    assert_eq!(json(&s, "reverse history -json").as_array().unwrap().len(), 1);
    json(&s, "reverse back -json");
    assert_eq!(word(&s, 0x7000_2000), 0);
}

#[test]
fn step_is_an_action_and_reset_clears() {
    let s = server();
    json(&s, "reverse on -notrace -region 0x70000000 0x40 -json");
    let pc0 = reg(&s, "pc");
    tcl(&s, "step");
    assert_eq!(json(&s, "reverse status -json")["checkpoints"], 1);
    json(&s, "reverse back -json");
    assert_eq!(reg(&s, "pc"), pc0);
    tcl(&s, "reset halt");
    let st = json(&s, "reverse status -json");
    assert_eq!(st["checkpoints"], 0);
    assert_eq!(st["cleared"], "reset");
}

#[test]
fn the_trace_belongs_to_the_mode() {
    let s = server();
    tcl(&s, "mcds arm -data 0x70000100 16");
    let e = s.interp.eval_global("reverse on -region 0x70000000 0x40").unwrap_err();
    assert!(e.contains("manual trace"), "{e}");
    tcl(&s, "mcds stop");
    tcl(&s, "reverse on -region 0x70000000 0x40");
    let e = s.interp.eval_global("mcds arm -data 0x70000100 16").unwrap_err();
    assert!(e.contains("reverse debugging"), "{e}");
    let e = s.interp.eval_global("mcds auto -program -stop halt").unwrap_err();
    assert!(e.contains("reverse debugging"), "{e}");
    tcl(&s, "reverse off");
    tcl(&s, "mcds arm -data 0x70000100 16");
}

#[test]
fn protected_ram_is_never_written() {
    let s = server();
    // CPU0's DSPR open to other masters only from 0x7000B400, as the TC377 application sets it up.
    tcl(&s, "mww 0xF880E000 0x7000B400");
    tcl(&s, "mww 0xF880E004 0x70025400");
    tcl(&s, "mww 0xF880E008 0x50000046");
    json(&s, "reverse on -notrace -region 0x70000400 0x100 -private -region 0x7000B400 0x100 -json");
    tcl(&s, "mww 0x70000410 0x11");
    let e = s.interp.eval_global("reverse back").unwrap_err();
    assert!(e.contains("RAM protection") && e.contains("nothing was changed"), "{e}");
    assert!(e.contains("reset the target from the debugger"), "the stacks closed: hint at a reset with the debugger, {e}");
    assert_eq!(word(&s, 0x7000_0410), 0x11, "nothing written");
    assert_eq!(json(&s, "reverse status -json")["checkpoints"], 1, "history kept");
    // Open memory is restored as usual.
    tcl(&s, "reverse off");
    json(&s, "reverse on -notrace -region 0x70000400 0x100 -private -region 0x7000B400 0x100 -json");
    tcl(&s, "mww 0x7000B410 0x22");
    json(&s, "reverse back -json");
    assert_eq!(word(&s, 0x7000_B410), 0);
}

#[test]
fn a_run_already_going_is_recorded_up_to_its_halt() {
    let s = server_running();
    assert_eq!(tcl(&s, "tc37x.cpu0 curstate"), "running");
    json(&s, "reverse on -region 0x70000000 0x40 -json");
    let st = json(&s, "mcds status -json");
    assert_eq!(st["armed"], true, "{st}");
    assert_eq!(st["owner"], "reverse", "{st}");
    assert_eq!(json(&s, "reverse status -json")["recording_run"], true);
    tcl(&s, "halt");
    tcl(&s, "wait_halt 2000");
    let st = json(&s, "mcds status -json");
    assert_eq!(st["armed"], false, "read at the halt: {st}");
    let r = json(&s, "reverse status -json");
    assert_eq!(r["recording_run"], false);
    assert_eq!(r["checkpoints"], 0, "no action, no checkpoint");
    assert!(json(&s, "mcds decode -json 10")["total"].as_u64().is_some(), "the recording is the last one");
}

fn with_sim<R>(s: &Server, f: impl FnOnce(&mut aurix_core::sim::SimAdapter) -> R) -> R {
    s.cx().with(|c| f(c.adapter.as_mut().unwrap().as_any_mut().downcast_mut::<aurix_core::sim::SimAdapter>().unwrap()))
}

/// The word at `addr` in the snapshot of history entry `k`.
fn saved(s: &Server, k: usize, addr: u32) -> u32 {
    let snap = s.cx().with(|c| aurix_core::reverse::snapshot(c, k)).unwrap();
    let (start, b) = snap.iter().find(|(st, b)| addr >= *st && addr + 4 <= st + b.len() as u32).unwrap();
    let o = (addr - start) as usize;
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn last_id(s: &Server) -> u64 {
    s.cx().with(|c| c.reverse.history.last().unwrap().id)
}

fn snapshot_bytes(s: &Server) -> u64 {
    s.cx().with(|c| c.reverse.history.last().unwrap().cost.snapshot_bytes)
}

#[test]
fn the_private_regions_are_read_only_where_the_last_action_wrote() {
    let s = server();
    json(&s, "reverse on -notrace -region 0x70000000 0x100 -region 0x70001000 0x40 -private -json");
    tcl(&s, "step");
    assert_eq!(snapshot_bytes(&s), 0x140);
    // The core stores a word in its own region and one in shared memory (the simulator's memory
    // changes without a debugger write); the decoded step says where it wrote.
    with_sim(&s, |m| {
        m.set_mem(0x7000_1008, 0xAA);
        m.set_mem(0x7000_0020, 0xBB);
    });
    let id = last_id(&s);
    s.cx().with(|c| aurix_core::reverse::note_private_writes(c, id, vec![(0x7000_1008, 4)], true));
    tcl(&s, "step");
    assert_eq!(snapshot_bytes(&s), 0x100 + 4, "shared in full, of the private region only the noted word");
    assert_eq!(saved(&s, 1, 0x7000_1008), 0xAA);
    assert_eq!(saved(&s, 1, 0x7000_0020), 0xBB);

    // A note from before a RAM write of the debugger's is not used.
    let id = last_id(&s);
    s.cx().with(|c| c.io().unwrap().write(0x7000_1010, &[1, 2, 3, 4]).unwrap());
    s.cx().with(|c| aurix_core::reverse::note_private_writes(c, id, Vec::new(), true));
    tcl(&s, "step");
    assert_eq!(snapshot_bytes(&s), 0x140);
    assert_eq!(saved(&s, 2, 0x7000_1010), 0x0403_0201);

    // Nor one from before another run of the core.
    let id = last_id(&s);
    s.cx().with(|c| {
        let t = c.reverse.target().unwrap();
        aurix_core::reverse::note_run(c, t);
        aurix_core::reverse::note_private_writes(c, id, Vec::new(), true);
    });
    tcl(&s, "step");
    assert_eq!(snapshot_bytes(&s), 0x140);

    // Step Back through the patched checkpoints restores what was there.
    json(&s, "reverse back -json");
    json(&s, "reverse back -json");
    json(&s, "reverse back -json");
    assert_eq!(word(&s, 0x7000_1008), 0xAA);
    json(&s, "reverse back -json");
    assert_eq!(word(&s, 0x7000_1008), 0);
    assert_eq!(word(&s, 0x7000_0020), 0);
}

#[test]
fn verify_reads_the_private_regions_anyway_and_trusts_the_full_read() {
    let s = server();
    json(&s, "reverse on -notrace -verify -region 0x70000000 0x100 -region 0x70001000 0x40 -private -json");
    tcl(&s, "step");
    with_sim(&s, |m| m.set_mem(0x7000_1020, 0x55));
    // A wrong note (claims nothing was written).
    let id = last_id(&s);
    s.cx().with(|c| aurix_core::reverse::note_private_writes(c, id, Vec::new(), true));
    tcl(&s, "step");
    assert_eq!(saved(&s, 1, 0x7000_1020), 0x55);
}

#[test]
fn step_back_restores_the_private_regions_where_the_notes_say() {
    let s = server();
    json(&s, "reverse on -notrace -region 0x70000000 0x100 -region 0x70001000 0x40 -private -json");
    tcl(&s, "step");
    tcl(&s, "step");
    // The last step stored a word in the private region, and says so.
    with_sim(&s, |m| m.set_mem(0x7000_100C, 0xCC));
    let id = last_id(&s);
    s.cx().with(|c| aurix_core::reverse::note_private_writes(c, id, vec![(0x7000_100C, 4)], true));
    // A change the note does not name (a wrong note, for the test): a restore by difference would
    // take it back, a restore from the notes leaves it.
    with_sim(&s, |m| m.set_mem(0x7000_1030, 0x77));
    let b = json(&s, "reverse back -json");
    assert_eq!(b["bytes"], 4, "{b}");
    assert_eq!(word(&s, 0x7000_100C), 0);
    assert_eq!(word(&s, 0x7000_1030), 0x77);
    // Without a note (the step before): by difference.
    let b = json(&s, "reverse back -json");
    assert_eq!(word(&s, 0x7000_1030), 0, "{b}");
}
