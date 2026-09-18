//! Generates the artifacts derived from the API definition (`api-spec`): the OpenAPI document
//! (served by the server, checked in as `server/openapi/openapi.json`) and the generated part of
//! the Python client (`python/aurix_dbg/_generated.py`).
//!
//! A file is only written when its content changes, so an unchanged API touches nothing. The
//! script runs again whenever `api-spec` (a build dependency) changes.

use std::path::{Path, PathBuf};
use std::{env, fs};

fn write_if_changed(path: &Path, content: &str) {
    if fs::read(path).is_ok_and(|old| old == content.as_bytes()) {
        return;
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).unwrap_or_else(|e| panic!("cannot create {}: {e}", dir.display()));
    }
    fs::write(path, content).unwrap_or_else(|e| panic!("cannot write {}: {e}", path.display()));
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let openapi = api_spec::gen::openapi_json();
    let python = api_spec::gen::python();

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    write_if_changed(&out.join("openapi.json"), &openapi);

    // server/crates/api -> server
    let server = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    write_if_changed(&server.join("openapi/openapi.json"), &openapi);
    write_if_changed(&server.join("../python/aurix_dbg/_generated.py"), &python);
}
