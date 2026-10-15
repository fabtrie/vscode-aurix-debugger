//! Links the Jim Tcl C library (`libjimtcl`) into the final binary.
//!
//! There is no Rust API here: the build script downloads the pinned Jim Tcl
//! release, generates what `configure` would (config headers, static extension
//! table, Tcl extensions as C) into `OUT_DIR` and compiles it. Crates that need
//! the C headers get the include directories as `DEP_JIMTCL_INCLUDE`.
//!
//! Environment:
//! - `JIMTCL_SRC_DIR`: use this unpacked jimtcl source tree instead of
//!   downloading (offline builds, local patches).
//! - `HTTPS_PROXY`: honoured by `curl` for the download.

/// The Jim Tcl release this crate builds.
pub const JIM_VERSION: &str = env!("JIM_VERSION");
