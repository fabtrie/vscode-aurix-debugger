//! `aurix-hub install` / `uninstall` from an extension-like directory: the versioned copy, the
//! `current` junction, the record of installations, the logon start (a test value of its own:
//! `AURIX_HUB_HOME` is set) and the update of a running hub.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn run(exe: &Path, home: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(exe).args(args).env("AURIX_HUB_HOME", home).env("AURIX_HUB_PORT", "1").output().unwrap();
    (out.status.code().unwrap_or(-1), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
}

/// A directory laid out like the extension's `bin`: the hub, a worker, the OpenOCD scripts.
fn extension(dir: &Path, worker: &[u8]) -> PathBuf {
    std::fs::create_dir_all(dir.join("openocd").join("tcl").join("board")).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_aurix-hub"), dir.join("aurix-hub.exe")).unwrap();
    std::fs::write(dir.join("aurix-dbg.exe"), worker).unwrap();
    std::fs::write(dir.join("openocd").join("tcl").join("board").join("x.cfg"), "# test\n").unwrap();
    dir.join("aurix-hub.exe")
}

#[test]
fn install_and_uninstall() {
    let root = std::env::temp_dir().join(format!("aurix-hub-install-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    let a = extension(&root.join("ext-a"), b"worker 1");
    let b = extension(&root.join("ext-b"), b"worker 1");

    let (code, out) = run(&a, &home, &["install", "--extension", "ext-a"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("copied") && out.contains("current ->"), "{out}");
    let current = home.join("local").join("current");
    assert!(current.join("aurix-hub.exe").is_file() && current.join("aurix-dbg.exe").is_file());
    assert!(current.join("openocd").join("tcl").join("board").join("x.cfg").is_file());
    let build = std::fs::read_to_string(current.join("build.txt")).unwrap().trim().to_string();
    assert!(build.starts_with(env!("CARGO_PKG_VERSION")), "{build}");

    // The same build again copies nothing; a second extension is recorded.
    let (code, out) = run(&b, &home, &["install", "--extension", b.parent().unwrap().to_str().unwrap()]);
    assert_eq!(code, 0, "{out}");
    assert!(!out.contains("copied"), "{out}");
    let installs = std::fs::read_to_string(home.join("local").join("installs.json")).unwrap();
    assert!(installs.contains("ext-a") && installs.contains("ext-b"), "{installs}");

    // Another build (another worker) of the same version: a second directory, `current` moves,
    // and the old one goes once nothing uses it.
    let c = extension(&root.join("ext-c"), b"worker 2");
    let (code, out) = run(&c, &home, &["install", "--extension", c.parent().unwrap().to_str().unwrap()]);
    assert_eq!(code, 0, "{out}");
    let build2 = std::fs::read_to_string(current.join("build.txt")).unwrap().trim().to_string();
    assert_ne!(build, build2);
    let versions: Vec<_> = std::fs::read_dir(home.join("local").join("versions")).unwrap().flatten().map(|e| e.file_name()).collect();
    assert_eq!(versions.len(), 1, "{versions:?}");

    // The logon start (the test's own value) and the status.
    let installed = current.join("aurix-hub.exe");
    let (code, out) = run(&installed, &home, &["status"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains(&build2) && out.contains("AurixDbgHub-"), "{out}");

    // Uninstalling from one extension keeps the files the others use.
    let (code, out) = run(&a, &home, &["uninstall", "--extension", "ext-a"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("kept the files"), "{out}");
    assert!(current.join("aurix-hub.exe").is_file());
    for ext in [&b, &c] {
        let (code, out) = run(ext, &home, &["uninstall", "--extension", ext.parent().unwrap().to_str().unwrap()]);
        assert_eq!(code, 0, "{out}");
    }
    let t0 = Instant::now();
    while current.exists() && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!current.exists(), "current is gone");
    assert!(!home.join("local").join("versions").exists());
    let _ = std::fs::remove_dir_all(&root);
}
