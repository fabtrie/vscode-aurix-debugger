//! Fetches OpenOCD's Tcl scripts into `<target dir>/openocd-tcl/`.
//!
//! The folder lives next to the build output (not in `OUT_DIR`) so that tools
//! outside Cargo, like the VS Code extension's packaging script, find it at a
//! fixed place: `server/target/openocd-tcl/{tcl,COPYING,LICENSES}`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

/// Commit of https://github.com/tricore-oss/openocd (branch aurix_support).
const OPENOCD_COMMIT: &str = "58c6c6197ec162c3e51937219c72ebcdb2f227c3";
/// SHA-256 of `https://github.com/tricore-oss/openocd/archive/<OPENOCD_COMMIT>.tar.gz`.
const OPENOCD_SHA256: &str = "e0356b0394bad793a365968aa15f5d5cc7aac63e5ae5cde0075c3977f84cd849";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=OPENOCD_TCL_DIR");

    if let Some(dir) = env::var_os("OPENOCD_TCL_DIR") {
        let dir = PathBuf::from(dir);
        assert!(dir.join("board").is_dir(), "openocd-tcl: OPENOCD_TCL_DIR={} has no board/ folder", dir.display());
        println!("cargo:rustc-env=OPENOCD_TCL_PATH={}", dir.display());
        println!("cargo:rustc-env=OPENOCD_COMMIT=");
        return;
    }

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let dest = target_dir(&out).join("openocd-tcl");
    // Holds the checksum of the archive the folder was unpacked from; rerun if it goes missing.
    let stamp = dest.join(".unpacked");
    println!("cargo:rerun-if-changed={}", stamp.display());
    if !fs::read_to_string(&stamp).is_ok_and(|s| s == OPENOCD_SHA256) {
        fetch(&out, &dest);
        fs::write(&stamp, OPENOCD_SHA256).unwrap();
    }

    println!("cargo:rustc-env=OPENOCD_TCL_PATH={}", dest.join("tcl").display());
    println!("cargo:rustc-env=OPENOCD_COMMIT={OPENOCD_COMMIT}");
}

/// Cargo's target directory: OUT_DIR is `<target>/[<triple>/]<profile>/build/<pkg>-<hash>/out`.
fn target_dir(out: &Path) -> PathBuf {
    out.ancestors()
        .find(|p| p.join("CACHEDIR.TAG").is_file())
        .unwrap_or(out)
        .to_path_buf()
}

/// Downloads the pinned commit and replaces `dest` with its `tcl/`, `COPYING` and `LICENSES/`.
fn fetch(out: &Path, dest: &Path) {
    let archive = format!("openocd-{OPENOCD_COMMIT}.tar.gz");
    if !out.join(&archive).is_file() || sha256_file(&out.join(&archive)) != OPENOCD_SHA256 {
        let url = format!("https://github.com/tricore-oss/openocd/archive/{OPENOCD_COMMIT}.tar.gz");
        let part = format!("{archive}.part");
        // Relative paths: an MSYS tar/curl first on PATH would read "C:\..." as host:path.
        run(
            Command::new("curl").current_dir(out).args(["-sSfL", "--retry", "3", "-o", &part, &url]),
            &format!("downloading {url} (set OPENOCD_TCL_DIR to use a local OpenOCD tcl folder)"),
        );
        let got = sha256_file(&out.join(&part));
        if got != OPENOCD_SHA256 {
            panic!("openocd-tcl: checksum mismatch for {url}\n  expected {OPENOCD_SHA256}\n  got      {got}");
        }
        fs::rename(out.join(&part), out.join(&archive)).unwrap();
    }

    // Unpacked in Rust: tar.exe and MSYS tar (without a UTF-8 locale) mangle the
    // non-ASCII file names in tcl/target.
    let unpack = out.join("unpack");
    if unpack.exists() {
        fs::remove_dir_all(&unpack).unwrap();
    }
    fs::create_dir_all(&unpack).unwrap();
    let root = format!("openocd-{OPENOCD_COMMIT}");
    let wanted = ["tcl", "COPYING", "LICENSES"].map(|m| Path::new(&root).join(m));
    let gz = flate2::read::GzDecoder::new(fs::File::open(out.join(&archive)).unwrap());
    let mut tar = tar::Archive::new(gz);
    for entry in tar.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().into_owned();
        if wanted.iter().any(|w| path.starts_with(w)) {
            entry.unpack_in(&unpack).unwrap_or_else(|e| panic!("openocd-tcl: unpacking {}: {e}", path.display()));
        }
    }

    if dest.exists() {
        fs::remove_dir_all(dest).unwrap();
    }
    fs::rename(unpack.join(&root), dest).unwrap();
    assert!(dest.join("tcl/board").is_dir(), "openocd-tcl: {archive} has no tcl/board folder");
}

fn run(cmd: &mut Command, what: &str) {
    match cmd.status() {
        Ok(s) if s.success() => {}
        Ok(s) => panic!("openocd-tcl: {what} failed ({s})"),
        Err(e) => panic!("openocd-tcl: {what} failed: could not run {:?}: {e}", cmd.get_program()),
    }
}

fn sha256_file(path: &Path) -> String {
    let data = fs::read(path).unwrap();
    Sha256::digest(&data).iter().map(|b| format!("{b:02x}")).collect()
}
