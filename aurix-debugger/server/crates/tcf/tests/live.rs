//! Live sets (`AurixTarget.setLiveSet` / `getLiveValues`): the server, not the client, decides per
//! region whether a value comes from the probe's sampler or from a plain read.
//!
//! Without a sampler (a TAS probe; the simulator by default) every region must be a plain read
//! through `Memory.get`'s code, and no DAQ session may exist.

use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::thread;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::Value;

use aurix_core::sim::{SimAdapter, SimSampler, SIM_UNMAPPED};
use aurix_core::{Server, ServerHandle};
use tcf::live::{self, MemGet, Region};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize().unwrap()
}

fn tcl_path(p: PathBuf) -> String {
    p.to_string_lossy().trim_start_matches(r"\\?\").replace('\\', "/")
}

/// The simulated TC37x with the real board cfg, served on its own thread (the real server loop,
/// so the sampler runs as in production).
fn server(sampler: bool) -> ServerHandle {
    let (tx, rx) = channel();
    thread::spawn(move || {
        let s = Server::new();
        s.cx().with(|c| {
            c.adapter_override = Some("sim".into());
            c.search_dirs.push(PathBuf::from(openocd_tcl::TCL_DIR));
        });
        s.interp.eval_global("script board/infineon/kit_a2g_tc375_lite.cfg").unwrap();
        s.interp
            .eval_global(&format!("script {}", jim::quote(&tcl_path(repo().join("tc37x_ocds_init.cfg")))))
            .unwrap();
        s.interp.eval_global("set tc3xx_hsm_ready_ms 0").unwrap();
        s.interp.eval_global("init").unwrap();
        if sampler {
            s.cx().with(|c| c.adapter.as_mut().unwrap().as_any_mut().downcast_mut::<SimAdapter>().unwrap().sampler = Some(SimSampler::default()));
        }
        tx.send(s.handle()).unwrap();
        s.run();
    });
    rx.recv().unwrap()
}

fn set_mem(h: &ServerHandle, addr: u32, v: u32) {
    h.call(move |cx| cx.with(|c| c.adapter.as_mut().unwrap().as_any_mut().downcast_mut::<SimAdapter>().unwrap().set_mem(addr, v))).unwrap();
}

fn daq_sessions(h: &ServerHandle) -> usize {
    h.call(|cx| cx.with(|c| c.daq.sessions().count())).unwrap()
}

const A: u32 = 0x7000_0100;
/// An HSM peripheral: read through the HSM (core-relative), never by the probe.
const HSM_PERI: u32 = 0xE000_0000;

fn region(ctx: &str, addr: u32, size: usize) -> Region {
    Region { ctx: ctx.into(), addr, size }
}

fn data(v: &Value) -> Option<Vec<u8>> {
    v.get("data").and_then(Value::as_str).map(|s| B64.decode(s).unwrap())
}

fn source(v: &Value) -> &str {
    v.get("source").and_then(Value::as_str).unwrap_or("")
}

/// What `Memory.get` reads for the region, as bytes and whether it reported an error.
fn memory_get(sh: &tcf::Shared, r: &Region) -> (Vec<u8>, bool) {
    match live::memory_get(sh, &r.ctx, r.addr, r.size) {
        MemGet::Partial(Ok(p)) => (p.data, !p.unreadable.is_empty()),
        MemGet::Op(Ok(o)) => {
            let mut d = o.data.clone();
            d.resize(r.size, 0);
            (d, !o.ok())
        }
        MemGet::Partial(Err(_)) | MemGet::Op(Err(_)) => (Vec::new(), true),
    }
}

fn wait_for(mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < Duration::from_secs(5), "timed out");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn without_a_sampler_every_region_is_read_as_memory_get_reads_it() {
    let h = server(false);
    let sh = tcf::session(h.clone());
    let core0 = tcf::cores(&h).into_iter().find(|c| c.idx == 0).unwrap().id;
    set_mem(&h, A, 0x1234_5678);
    let regions = vec![region(&core0, A, 4), region(&core0, A, 128), region(&core0, SIM_UNMAPPED.start, 4), region(&core0, HSM_PERI, 4)];
    live::set(&sh, 1, "view", regions.clone(), 200_000).unwrap();
    assert_eq!(daq_sessions(&h), 0, "no DAQ session without a sampler");
    for _ in 0..3 {
        let v = live::values(&sh, 1, "view").unwrap();
        let v = v.as_array().unwrap();
        assert_eq!(v.len(), regions.len());
        for (val, r) in v.iter().zip(&regions) {
            assert_eq!(source(val), "read", "{r:?}");
            let (bytes, failed) = memory_get(&sh, r);
            assert_eq!(val.get("error").is_some(), failed, "{r:?}: {val}");
            if !failed {
                assert_eq!(data(val).unwrap(), bytes, "{r:?}");
            }
        }
        assert_eq!(&data(&v[0]).unwrap(), &0x1234_5678u32.to_le_bytes());
        assert!(v[2].get("error").is_some(), "unmapped memory reports an error");
    }
    live::clear(&sh, 1, "view");
    assert_eq!(daq_sessions(&h), 0);
}

#[test]
fn with_a_sampler_plain_bus_regions_come_from_the_probe() {
    let h = server(true);
    let sh = tcf::session(h.clone());
    let core0 = tcf::cores(&h).into_iter().find(|c| c.idx == 0).unwrap().id;
    set_mem(&h, A, 0x0000_0011);
    let regions = vec![region(&core0, A, 4), region(&core0, A + 0x100, 128), region(&core0, HSM_PERI, 4)];
    live::set(&sh, 1, "view", regions, 100_000).unwrap();
    assert_eq!(daq_sessions(&h), 1);
    // Before the probe sends a value the client still gets one, read.
    let first = live::values(&sh, 1, "view").unwrap();
    assert_eq!(data(&first[0]).unwrap(), 0x11u32.to_le_bytes());
    wait_for(|| source(&live::values(&sh, 1, "view").unwrap()[0]) == "daq");
    let v = live::values(&sh, 1, "view").unwrap();
    assert_eq!(data(&v[0]).unwrap(), 0x11u32.to_le_bytes());
    assert_eq!(source(&v[1]), "read", "too long for the probe");
    assert_eq!(source(&v[2]), "read", "core-relative");
    set_mem(&h, A, 0x0000_0022);
    wait_for(|| data(&live::values(&sh, 1, "view").unwrap()[0]) == Some(0x22u32.to_le_bytes().to_vec()));
    assert_eq!(source(&live::values(&sh, 1, "view").unwrap()[0]), "daq");
}

#[test]
fn the_period_and_the_regions_follow_the_last_set() {
    let h = server(true);
    let sh = tcf::session(h.clone());
    let core0 = tcf::cores(&h).into_iter().find(|c| c.idx == 0).unwrap().id;
    live::set(&sh, 1, "view", vec![region(&core0, A, 4)], 200_000).unwrap();
    let periods = |h: &ServerHandle| h.call(|cx| cx.with(|c| c.daq.sessions().flat_map(|s| s.points.iter().map(|p| (p.addr, p.interval_us()))).collect::<Vec<_>>())).unwrap();
    assert_eq!(periods(&h), vec![(A, 200_000)]);
    // The interval picker moved to 1 s: the same point, retimed.
    live::set(&sh, 1, "view", vec![region(&core0, A, 4)], 1_000_000).unwrap();
    assert_eq!(periods(&h), vec![(A, 1_000_000)]);
    // An expression added and the first removed.
    live::set(&sh, 1, "view", vec![region(&core0, A + 8, 4)], 1_000_000).unwrap();
    assert_eq!(periods(&h), vec![(A + 8, 1_000_000)]);
    // Two views keep their own sets.
    live::set(&sh, 1, "other", vec![region(&core0, A, 4)], 500_000).unwrap();
    assert_eq!(daq_sessions(&h), 2);
    live::clear(&sh, 1, "view");
    live::clear_owner(&sh, 1);
    assert_eq!(daq_sessions(&h), 0);
}

#[test]
fn a_region_the_probe_cannot_read_becomes_a_plain_read_with_its_error() {
    let h = server(true);
    let sh = tcf::session(h.clone());
    let core0 = tcf::cores(&h).into_iter().find(|c| c.idx == 0).unwrap().id;
    let bad = SIM_UNMAPPED.start;
    live::set(&sh, 1, "view", vec![region(&core0, bad, 4), region(&core0, A, 4)], 100_000).unwrap();
    // The probe never answers for it: after its wait it is refused and read, which reports why.
    wait_for(|| {
        let still = h.call(|cx| cx.with(|c| c.daq.sessions().flat_map(|s| s.points.iter().map(|p| p.addr)).any(|a| a == SIM_UNMAPPED.start))).unwrap();
        let v = live::values(&sh, 1, "view").unwrap();
        !still && source(&v[0]) == "read" && v[0].get("error").is_some()
    });
    wait_for(|| source(&live::values(&sh, 1, "view").unwrap()[1]) == "daq");
}
