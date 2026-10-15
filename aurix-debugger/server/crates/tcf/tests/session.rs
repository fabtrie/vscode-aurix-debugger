//! The session state shared by TCF clients and the REST/MCP API: breakpoints of different
//! owners on the same address, `Breakpoints.set` only replacing the caller's own breakpoints.

use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::thread;

use serde_json::{json, Map, Value};

use aurix_core::{ServerHandle, Server};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize().unwrap()
}

fn tcl_path(p: PathBuf) -> String {
    p.to_string_lossy().trim_start_matches(r"\\?\").replace('\\', "/")
}

/// The simulated TC37x with the real board cfg, initialized, served on its own thread.
fn server() -> ServerHandle {
    let (tx, rx) = channel();
    thread::spawn(move || {
        let s = Server::new();
        s.cx().with(|c| {
            c.adapter_override = Some("sim".into());
            c.search_dirs.push(PathBuf::from(openocd_tcl::TCL_DIR));
        });
        s.interp.eval_global("script board/infineon/kit_a2g_tc377tx_3v3_trb.cfg").unwrap();
        s.interp
            .eval_global(&format!("script {}", jim::quote(&tcl_path(repo().join("tc37x_ocds_init.cfg")))))
            .unwrap();
        s.interp.eval_global("set tc3xx_hsm_ready_ms 0").unwrap();
        s.interp.eval_global("init").unwrap();
        tx.send(s.handle()).unwrap();
        s.run();
    });
    rx.recv().unwrap()
}

fn bp(id: &str, addr: u32, ctx: &str) -> Map<String, Value> {
    json!({ "ID": id, "Enabled": true, "Address": addr, "ContextIds": [ctx] }).as_object().unwrap().clone()
}

/// Core breakpoint addresses of target 0.
fn planted(h: &ServerHandle) -> Vec<u32> {
    h.call(|cx| cx.with(|c| c.targets[0].breakpoints.iter().map(|b| b.addr).collect())).unwrap()
}

#[test]
fn owners_share_core_breakpoints() {
    let h = server();
    let s = tcf::session(h.clone());
    let events = s.subscribe();
    let core0 = tcf::cores(&h).into_iter().find(|c| c.idx == 0).unwrap().id;
    const ADDR: u32 = 0x8000_1000;

    // A TCF client (channel 1) and the API on the same address: one core breakpoint.
    s.apply_breakpoints(vec![bp("vs-1", ADDR, &core0)], true, 1).unwrap();
    let api_id = s.next_api_breakpoint_id();
    s.apply_breakpoints(vec![bp(&api_id, ADDR, &core0)], false, tcf::API_OWNER).unwrap();
    assert_eq!(planted(&h), vec![ADDR]);
    assert!(s.breakpoint(&api_id).unwrap().0.planted());

    // The client replaces its list: the API breakpoint stays.
    s.apply_breakpoints(vec![], true, 1).unwrap();
    assert_eq!(s.breakpoints().len(), 1);
    assert_eq!(planted(&h), vec![ADDR]);

    // The client enables/changes the API breakpoint: it stays the API's.
    let mut changed = bp(&api_id, ADDR, &core0);
    changed.insert("Condition".into(), json!(""));
    s.apply_breakpoints(vec![changed], false, 1).unwrap();
    s.apply_breakpoints(vec![], true, 1).unwrap();
    assert_eq!(s.breakpoint(&api_id).unwrap().0.owner, tcf::API_OWNER);

    // Removing the last user removes the core breakpoint.
    assert_eq!(s.remove_breakpoints(vec![api_id.clone()]), vec![api_id.clone()]);
    assert!(planted(&h).is_empty());

    // Non-TCF frontends saw the changes.
    let evs: Vec<tcf::SessionEvent> = events.try_iter().collect();
    let removed: Vec<String> = evs
        .iter()
        .flat_map(|e| match e {
            tcf::SessionEvent::Breakpoints { removed, .. } => removed.clone(),
        })
        .collect();
    assert_eq!(removed, vec!["vs-1".to_string(), api_id]);
}
