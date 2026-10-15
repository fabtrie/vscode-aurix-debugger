//! Compiles the C shim against the Jim Tcl headers of the `jimtcl` crate, which
//! builds and links the interpreter itself.

use std::env;

fn main() {
    let includes = env::var_os("DEP_JIMTCL_INCLUDE").expect("jimtcl did not export its include directories");

    let mut b = cc::Build::new();
    for dir in env::split_paths(&includes) {
        b.include(dir);
    }
    b.file("src/shim.c").opt_level(2).warnings(false).compile("jimshim");

    println!("cargo:rerun-if-changed=src/shim.c");
}
