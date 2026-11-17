//! The core thread of a worker against the simulator, driven from another thread as the
//! frontends do: Jim's own `after` (a blocking wait like `sleep`, and timer events) keeps the
//! worker running, and a panic in a request is logged and ends only that request.
//!
//! One test function on purpose: the log file and the panic hook belong to the whole process.

use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use aurix_core::{crash, log, Server, ServerHandle};

/// Starts a server on the simulator on its own thread (the interpreter stays on the thread that
/// made it) and returns the handle frontends use.
fn start() -> ServerHandle {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let s = Server::new();
        s.cx().with(|c| c.adapter_override = Some("sim".into()));
        s.interp.eval_global("adapter driver sim").unwrap();
        s.interp.eval_global("init").unwrap();
        tx.send(s.handle()).unwrap();
        s.run();
    });
    rx.recv_timeout(Duration::from_secs(30)).expect("the server started")
}

#[test]
fn after_and_panics_keep_the_worker() {
    let log_file = std::env::temp_dir().join(format!("aurix-worker-sim-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log_file);
    log::set_debug_level(0);
    log::set_stderr(false);
    log::set_output_file(log_file.to_str().unwrap()).unwrap();
    crash::install();
    let h = start();

    // `after <ms>` waits like `sleep`, in the console path (with captured output) and the plain one.
    let t = Instant::now();
    let (r, out) = h.tcl_with_output_from_api("echo a; after 5; echo b; after 1");
    assert_eq!(r, Ok(String::new()));
    assert_eq!(out, ["a", "b"]);
    assert!(t.elapsed() >= Duration::from_millis(6), "{:?}", t.elapsed());
    assert_eq!(h.tcl("after 0; sleep 1; set x ok"), Ok("ok".into()));
    // Timer events: scheduled, listed, cancelled, run by `update`.
    assert_eq!(h.tcl("set ::fired no; after 1 {set ::fired yes}; after 20; update; set ::fired"), Ok("yes".into()));
    assert_eq!(h.tcl("set id [after 60000 {set ::never 1}]; set n [llength [after info]]; after cancel $id; list $n [llength [after info]]"), Ok("1 0".into()));
    assert!(h.tcl("after").unwrap_err().contains("wrong # args"));
    assert!(h.tcl("after bogus").unwrap_err().contains("bogus"));
    // Negative: no wait (Jim's own `after` waited about 71 minutes); too long: an error.
    let t = Instant::now();
    assert_eq!(h.tcl("after -1; after -0.5; after -0x10; list ok"), Ok("ok".into()));
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    assert!(h.tcl("after 1e300").unwrap_err().contains("too long"));
    assert!(h.tcl("after inf").is_err());

    // A panic in a request: that request gets no answer, the log says why, the worker carries on.
    let lost: Option<()> = h.call(|_| panic!("injected test panic"));
    assert!(lost.is_none());
    assert_eq!(h.tcl("set still alive"), Ok("alive".into()));
    let text = std::fs::read_to_string(&log_file).unwrap();
    assert!(text.contains("internal error: thread") && text.contains("injected test panic"), "{text}");

    assert!(h.tcl("shutdown").is_ok());
    let _ = std::fs::remove_file(&log_file);
}
