//! Compiles Jim Tcl from OpenOCD's jimtcl tree, which already contains the
//! MSYS2/MinGW configure output (jimautoconf.h, jim-config.h, _load-static-exts.c
//! and the C files generated from the Tcl extensions).

use std::env;
use std::path::PathBuf;

const SOURCES: &[&str] = &[
    "_load-static-exts.c",
    "jim-subcmd.c",
    "jim-interactive.c",
    "jim-format.c",
    "jim.c",
    "utf8.c",
    "jimregexp.c",
    "jimiocompat.c",
    "jim-win32compat.c",
    "jsmn/jsmn.c",
    "jim-nosignal.c",
    "jim-aio.c",
    "jim-array.c",
    "jim-clock.c",
    "jim-eventloop.c",
    "jim-exec.c",
    "jim-file.c",
    "jim-history.c",
    "jim-interp.c",
    "jim-json.c",
    "jim-load.c",
    "jim-namespace.c",
    "jim-pack.c",
    "jim-package.c",
    "jim-readdir.c",
    "jim-regexp.c",
    "_glob.c",
    "_jsonencode.c",
    "_nshelper.c",
    "_oo.c",
    "_stdlib.c",
    "_tclcompat.c",
    "_tree.c",
];

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let jim_dir = env::var_os("JIMTCL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../../../third_party/openocd/jimtcl"));
    let jim_dir = jim_dir.canonicalize().expect("jimtcl directory not found (set JIMTCL_DIR)");
    let jim_dir = PathBuf::from(jim_dir.to_str().unwrap().trim_start_matches(r"\\?\"));

    let mut b = cc::Build::new();
    b.include(&jim_dir)
        .define("_GNU_SOURCE", None)
        .define("_WIN32_WINNT", "0x0601")
        .flag_if_supported("-Wno-deprecated-declarations")
        .flag_if_supported("-fno-unwind-tables")
        .flag_if_supported("-fno-asynchronous-unwind-tables")
        .opt_level(2)
        .warnings(false);
    for s in SOURCES {
        b.file(jim_dir.join(s));
        println!("cargo:rerun-if-changed={}", jim_dir.join(s).display());
    }
    b.file(manifest.join("src/shim.c"));
    b.compile("jim");

    println!("cargo:rerun-if-changed=src/shim.c");
    println!("cargo:rerun-if-env-changed=JIMTCL_DIR");
}
