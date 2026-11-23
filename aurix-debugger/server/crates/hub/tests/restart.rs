//! The update-on-idle restart (`POST /shutdown {"when_idle": true, "exe": ...}`): a hub that
//! cannot start the new executable keeps running instead of leaving the machine without a hub,
//! and the restarted hub runs in its own folder, not in the folder its starter ran in.

use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use hub::http1;

fn health(url: &str) -> Option<Value> {
    http1::json(url, "GET", "/health", &[], None, Duration::from_secs(2)).ok().filter(|(s, _)| *s == 200).map(|(_, v)| v)
}

fn wait_for(what: &str, secs: u64, mut ok: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !ok() {
        assert!(t0.elapsed() < Duration::from_secs(secs), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn restart_from(url: &str, exe: &Path) {
    let (status, v) = http1::json(url, "POST", "/shutdown", &[], Some(&json!({ "when_idle": true, "exe": exe.to_string_lossy() })), Duration::from_secs(5)).unwrap();
    assert_eq!(status, 200, "{v}");
}

#[test]
fn update_restart_keeps_a_hub_running() {
    let root = std::env::temp_dir().join(format!("aurix-hub-restart-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    let starter = root.join("starter");
    let next = root.join("next");
    std::fs::create_dir_all(&starter).unwrap();
    std::fs::create_dir_all(&next).unwrap();
    let next_exe = next.join(Path::new(env!("CARGO_BIN_EXE_aurix-hub")).file_name().unwrap());
    std::fs::copy(env!("CARGO_BIN_EXE_aurix-hub"), &next_exe).unwrap();
    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let url = format!("http://127.0.0.1:{port}");

    let mut first = Command::new(env!("CARGO_BIN_EXE_aurix-hub"))
        .args(["serve", "--foreground"])
        .current_dir(&starter)
        .env("AURIX_HUB_HOME", &home)
        .env("AURIX_HUB_PORT", port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for("the hub", 15, || health(&url).is_some());
    let pid = health(&url).unwrap()["pid"].as_u64().unwrap();

    // The new executable is not there (as when `current` is not visible to this hub): it stays.
    restart_from(&url, &root.join("missing").join("aurix-hub.exe"));
    let log = home.join("local").join("logs").join("hub.log");
    wait_for("the failed restart in the log", 10, || std::fs::read_to_string(&log).unwrap_or_default().contains("cannot start"));
    std::thread::sleep(Duration::from_millis(1500));
    assert!(first.try_wait().unwrap().is_none(), "the hub stopped although no new one started");
    assert_eq!(health(&url).map(|v| v["pid"].as_u64()), Some(Some(pid)));

    // A good one: it takes over, and the old one ends.
    restart_from(&url, &next_exe);
    wait_for("the old hub to end", 15, || first.try_wait().unwrap().is_some());
    wait_for("the new hub", 15, || health(&url).is_some_and(|v| v["pid"].as_u64() != Some(pid)));
    let v = health(&url).unwrap();
    assert_eq!(std::fs::canonicalize(v["exe"].as_str().unwrap()).unwrap(), std::fs::canonicalize(&next_exe).unwrap(), "{v}");

    // The new hub does not hold the folder the first one was started in (Windows refuses to
    // delete a process's working directory).
    std::fs::remove_dir(&starter).expect("the restarted hub holds the starter's working directory");

    let _ = http1::json(&url, "POST", "/shutdown", &[], Some(&json!({})), Duration::from_secs(5));
    wait_for("the new hub to stop", 15, || health(&url).is_none());
    std::thread::sleep(Duration::from_millis(500));
    let _ = std::fs::remove_dir_all(&root);
}
