//! Where the hub keeps things.
//!
//! ```text
//! %LOCALAPPDATA%\aurix-dbg\   versions\<build>\, current (junction), installs.json, install.lock,
//!                             logs\, cache\ (uploaded files)
//! %APPDATA%\aurix-dbg\        hub.toml, profiles\  (user data: install and uninstall never touch it)
//! ```
//!
//! `AURIX_HUB_HOME` puts both under one directory (tests, portable use).

use std::path::PathBuf;

fn home_override() -> Option<PathBuf> {
    std::env::var_os("AURIX_HUB_HOME").filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn env_dir(var: &str) -> PathBuf {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(std::env::temp_dir)
}

/// Program files and machine-local state: `%LOCALAPPDATA%\aurix-dbg`.
pub fn local_dir() -> PathBuf {
    home_override().map(|h| h.join("local")).unwrap_or_else(|| env_dir("LOCALAPPDATA").join("aurix-dbg"))
}

/// User settings: `%APPDATA%\aurix-dbg`.
pub fn config_dir() -> PathBuf {
    home_override().map(|h| h.join("config")).unwrap_or_else(|| env_dir("APPDATA").join("aurix-dbg"))
}

pub fn versions_dir() -> PathBuf {
    local_dir().join("versions")
}

/// The junction to the installed version.
pub fn current_dir() -> PathBuf {
    local_dir().join("current")
}

pub fn logs_dir() -> PathBuf {
    local_dir().join("logs")
}

pub fn cache_dir() -> PathBuf {
    local_dir().join("cache")
}

pub fn profiles_dir() -> PathBuf {
    config_dir().join("profiles")
}

pub fn hub_toml() -> PathBuf {
    config_dir().join("hub.toml")
}

pub fn hub_log() -> PathBuf {
    logs_dir().join("hub.log")
}

/// The working directory of a worker without a profile: the settings directory when it exists.
pub fn config_dir_or_exe() -> PathBuf {
    let c = config_dir();
    if c.is_dir() {
        c
    } else {
        exe_dir()
    }
}

/// The directory of the running executable.
pub fn exe_dir() -> PathBuf {
    std::env::current_exe().ok().and_then(|e| e.parent().map(PathBuf::from)).unwrap_or_else(|| PathBuf::from("."))
}

/// File name of an executable on this platform.
pub fn exe_name(base: &str) -> String {
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base.to_string()
    }
}
