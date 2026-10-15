//! Location of OpenOCD's Tcl scripts (`board/`, `target/`, `interface/`, ...).
//!
//! The build script downloads the pinned commit of the tricore-oss OpenOCD fork
//! (upstream OpenOCD has no TriCore support) and unpacks `tcl/`, `COPYING` and
//! `LICENSES/` into `<target dir>/openocd-tcl/`. The integration tests use the
//! scripts from there, and the VS Code extension packages that folder.
//!
//! Environment:
//! - `OPENOCD_TCL_DIR`: use this `tcl` folder instead of downloading (offline
//!   builds, a local OpenOCD checkout).
//! - `HTTPS_PROXY`: honoured by `curl` for the download.

/// The OpenOCD `tcl` folder, to be used as a script search directory (`-s`).
pub const TCL_DIR: &str = env!("OPENOCD_TCL_PATH");

/// The OpenOCD commit the scripts come from (empty with `OPENOCD_TCL_DIR`).
pub const COMMIT: &str = env!("OPENOCD_COMMIT");
