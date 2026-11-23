//! Installing the hub to a fixed per-user location, and starting it at logon.
//!
//! The VS Code extension lives in a versioned directory that every update replaces, so a logon
//! entry or an MCP configuration cannot point there. `install` copies `aurix-hub`, `aurix-dbg`,
//! the OpenOCD scripts and the SVD files to `%LOCALAPPDATA%\aurix-dbg\versions\<version>-<hash>\` and points
//! the junction `current` at it; the logon entry and MCP configurations name
//! `current\aurix-hub.exe`, which never changes. A junction needs no admin rights, and moving it
//! does not disturb running processes (they hold files in the version directory).
//!
//! Logon start: the per-user `Run` key (`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`),
//! which needs no elevation either. It starts the hub while the user is logged on.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::config::HubConfig;
use crate::{http1, paths, spawn};

/// Name of the `Run` value.
pub const RUN_VALUE: &str = "AurixDbgHub";

/// The `Run` value of this installation: with `AURIX_HUB_HOME` (tests, portable copies) one of
/// its own, so it never touches the real one.
pub fn run_value() -> String {
    match std::env::var("AURIX_HUB_HOME").ok().filter(|h| !h.is_empty()) {
        Some(h) => {
            let hash: String = Sha256::digest(h.as_bytes()).iter().take(4).map(|b| format!("{b:02x}")).collect();
            format!("{RUN_VALUE}-{hash}")
        }
        None => RUN_VALUE.to_string(),
    }
}

/// Files of an installation besides the OpenOCD scripts.
const FILES: &[&str] = &["aurix-hub", "aurix-dbg"];
const EXTRA_FILES: &[&str] = &["THIRD_PARTY_LICENSES.txt"];

/// The hub to start: the installed one, or this one when nothing is installed.
pub fn installed_hub_exe() -> PathBuf {
    let p = paths::current_dir().join(paths::exe_name("aurix-hub"));
    if p.is_file() {
        p
    } else {
        std::env::current_exe().unwrap_or(p)
    }
}

/// `<version>-<first 8 hex digits of the SHA-256 of the executables>`.
pub fn build_id(dir: &Path) -> Result<String, String> {
    let mut h = Sha256::new();
    for f in FILES {
        let p = dir.join(paths::exe_name(f));
        let data = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        h.update(&data);
    }
    let hex: String = h.finalize().iter().take(4).map(|b| format!("{b:02x}")).collect();
    Ok(format!("{}-{hex}", env!("CARGO_PKG_VERSION")))
}

fn version_of_build(build: &str) -> Vec<u64> {
    build.split('-').next().unwrap_or_default().split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// The build `current` points at.
pub fn current_build() -> Option<String> {
    std::fs::read_to_string(paths::current_dir().join("build.txt")).ok().map(|s| s.lines().next().unwrap_or_default().trim().to_string()).filter(|s| !s.is_empty())
}

/// Exclusive install lock (a stale one older than two minutes is taken over).
struct Lock(PathBuf);

impl Lock {
    fn take() -> Result<Lock, String> {
        let p = paths::local_dir().join("install.lock");
        std::fs::create_dir_all(paths::local_dir()).map_err(|e| e.to_string())?;
        let t0 = Instant::now();
        loop {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&p) {
                Ok(_) => return Ok(Lock(p)),
                Err(_) => {
                    let stale = std::fs::metadata(&p).and_then(|m| m.modified()).ok().and_then(|m| m.elapsed().ok()).is_some_and(|a| a > Duration::from_secs(120));
                    if stale {
                        let _ = std::fs::remove_file(&p);
                        continue;
                    }
                    if t0.elapsed() > Duration::from_secs(10) {
                        return Err(format!("another installation is running ({} exists)", p.display()));
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let target = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &target)?;
        } else {
            std::fs::copy(e.path(), target)?;
        }
    }
    Ok(())
}

/// Points `link` at `target`: a junction on Windows, a symbolic link elsewhere.
fn point(link: &Path, target: &Path) -> Result<(), String> {
    if std::fs::symlink_metadata(link).is_ok() {
        // Removes the junction only, not what it points at.
        std::fs::remove_dir(link).or_else(|_| std::fs::remove_file(link)).map_err(|e| format!("{}: {e}", link.display()))?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let out = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .creation_flags(0x0800_0000)
            .output()
            .map_err(|e| format!("mklink: {e}"))?;
        if !out.status.success() {
            return Err(format!("mklink /J {} {}: {}", link.display(), target.display(), String::from_utf8_lossy(&out.stdout).trim()));
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        std::os::unix::fs::symlink(target, link).map_err(|e| format!("{}: {e}", link.display()))
    }
}

fn installs_file() -> PathBuf {
    paths::local_dir().join("installs.json")
}

fn read_installs() -> Vec<String> {
    std::fs::read_to_string(installs_file()).ok().and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok()).unwrap_or_default()
}

fn write_installs(list: &[String]) {
    let _ = std::fs::write(installs_file(), serde_json::to_string_pretty(list).unwrap_or_default());
}

/// The hub of this machine, if one answers on the configured port.
pub fn running_hub() -> Option<(String, Value)> {
    let cfg = HubConfig::load().ok()?;
    let url = format!("http://127.0.0.1:{}", cfg.port);
    let (status, v) = http1::json(&url, "GET", "/health", &[], None, Duration::from_secs(2)).ok()?;
    (status == 200 && v["role"] == "aurix-hub").then_some((url, v))
}

/// Stops the running hub and waits until it is gone. `now` closes its sessions first.
pub fn stop_hub(now: bool) -> Result<bool, String> {
    let Some((url, _)) = running_hub() else { return Ok(false) };
    let _ = http1::json(&url, "POST", "/shutdown", &[], Some(&json!({ "now": now })), Duration::from_secs(30));
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(15) {
        if running_hub().is_none() {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("the hub did not stop within 15 s".into())
}

/// Starts the installed hub in the background unless one runs.
pub fn start_hub() -> Result<bool, String> {
    if running_hub().is_some() {
        return Ok(false);
    }
    let exe = installed_hub_exe();
    spawn::spawn_detached(&exe, &["serve"]).map_err(|e| format!("cannot start {}: {e}", exe.display()))?;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(10) {
        if running_hub().is_some() {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err(format!("{} did not answer within 10 s (see {})", exe.display(), paths::hub_log().display()))
}

pub struct InstallOptions {
    pub autostart: bool,
    pub remote: bool,
    /// The extension directory that asks (recorded so uninstalling from one VS Code profile does
    /// not remove what another still uses); default: where this executable is.
    pub extension: Option<String>,
}

/// Copies this build to the fixed location (unless it is there), points `current` at it, and
/// optionally registers the logon start and sets up remote access. Returns what it did.
pub fn install(o: &InstallOptions) -> Result<Vec<String>, String> {
    let _lock = Lock::take()?;
    let mut did = Vec::new();
    let src = paths::exe_dir();
    let versions = paths::versions_dir();
    std::fs::create_dir_all(&versions).map_err(|e| format!("{}: {e}", versions.display()))?;
    for dir in [paths::local_dir(), versions.clone()] {
        if let Some(to) = redirected(&dir) {
            return Err(format!(
                "{} is redirected to {} for this process: it runs inside an app container (an MSIX-packaged app such as the Claude desktop app) that keeps its own copy of the folder, so hubs and VS Code outside the app would not see this installation. Nothing was changed; run `aurix-hub install` from a terminal outside that app.",
                dir.display(),
                to.display()
            ));
        }
    }
    let installed_here = std::fs::canonicalize(&src).ok().zip(std::fs::canonicalize(&versions).ok()).is_some_and(|(s, v)| s.starts_with(v));
    if !installed_here {
        let build = build_id(&src)?;
        let dest = versions.join(&build);
        if !dest.join("build.txt").is_file() {
            let tmp = versions.join(format!("{build}.tmp"));
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir_all(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
            for f in FILES {
                let n = paths::exe_name(f);
                std::fs::copy(src.join(&n), tmp.join(&n)).map_err(|e| format!("{}: {e}", src.join(&n).display()))?;
            }
            for f in EXTRA_FILES {
                if src.join(f).is_file() {
                    let _ = std::fs::copy(src.join(f), tmp.join(f));
                }
            }
            if src.join("openocd").is_dir() {
                copy_dir(&src.join("openocd"), &tmp.join("openocd")).map_err(|e| format!("copying the OpenOCD scripts: {e}"))?;
            }
            // The SVD descriptions the peripheral operations read, next to the worker.
            if src.join("svd").is_dir() {
                copy_dir(&src.join("svd"), &tmp.join("svd")).map_err(|e| format!("copying the SVD files: {e}"))?;
            }
            std::fs::write(tmp.join("build.txt"), format!("{build}\n")).map_err(|e| e.to_string())?;
            let _ = std::fs::remove_dir_all(&dest);
            std::fs::rename(&tmp, &dest).map_err(|e| format!("{}: {e}", dest.display()))?;
            did.push(format!("copied {build} to {}", dest.display()));
        }
        let newer = match current_build() {
            None => true,
            Some(cur) => cur == build || version_of_build(&build) >= version_of_build(&cur),
        };
        if newer && current_build().as_deref() != Some(build.as_str()) {
            point(&paths::current_dir(), &dest)?;
            did.push(format!("current -> {build}"));
            if let Some((url, _)) = running_hub() {
                let exe = paths::current_dir().join(paths::exe_name("aurix-hub"));
                let _ = http1::json(&url, "POST", "/shutdown", &[], Some(&json!({ "when_idle": true, "exe": exe.to_string_lossy() })), Duration::from_secs(5));
                did.push("the running hub restarts from the new version once no session is open".into());
            }
        } else if !newer {
            did.push(format!("kept current at {} (newer than {build})", current_build().unwrap_or_default()));
        }
    }
    let ext = o.extension.clone().unwrap_or_else(|| src.to_string_lossy().into_owned());
    let mut installs = read_installs();
    if !installs.contains(&ext) {
        installs.push(ext);
        write_installs(&installs);
    }
    if o.remote {
        let mut cfg = HubConfig::load_file(&paths::hub_toml())?;
        if cfg.token().is_none() {
            let mut b = [0u8; 24];
            getrandom::fill(&mut b).map_err(|e| format!("no randomness for a token: {e}"))?;
            cfg.token = Some(b.iter().map(|x| format!("{x:02x}")).collect());
        }
        if cfg.loopback_only() {
            cfg.bind = "0.0.0.0".into();
        }
        cfg.save(&paths::hub_toml())?;
        did.push(format!(
            "remote access: {} binds {}:{}; clients send the header `Authorization: Bearer {}`. Restart the hub to apply.",
            paths::hub_toml().display(),
            cfg.bind,
            cfg.port,
            cfg.token().unwrap_or_default()
        ));
    }
    if o.autostart {
        register_autostart()?;
        did.push(format!("registered the logon start ({})", run_value()));
        if start_hub()? {
            did.push("started the hub".into());
        }
    }
    cleanup();
    Ok(did)
}

/// Where new files in `dir` really go when that differs from `dir`: inside an MSIX app container
/// (the Claude desktop app runs Claude Code and its shells in one) Windows redirects writes to
/// a per-app copy of the folder under `%LOCALAPPDATA%\Packages\<app>\LocalCache` as soon as that
/// copy has the folder, while deletes still reach the real one. `install` would then remove the
/// real `current` junction and create the new one where only that app sees it.
pub fn redirected(dir: &Path) -> Option<PathBuf> {
    let probe = dir.join(format!(".redirect-probe-{}", std::process::id()));
    std::fs::write(&probe, b"").ok()?;
    let real = std::fs::canonicalize(&probe);
    let _ = std::fs::remove_file(&probe);
    let real_dir = real.ok()?.parent()?.to_path_buf();
    let in_app_copy = |p: &Path| {
        let parts: Vec<String> = p.components().map(|c| c.as_os_str().to_string_lossy().to_ascii_lowercase()).collect();
        parts.windows(3).any(|w| w[0] == "packages" && w[2] == "localcache")
    };
    let moved = std::fs::canonicalize(dir).ok().is_some_and(|d| d != real_dir);
    (moved || (in_app_copy(&real_dir) && !in_app_copy(dir))).then_some(real_dir)
}

/// Removes version directories nothing uses (a locked one stays until the next install).
fn cleanup() {
    let keep = std::fs::canonicalize(paths::current_dir()).ok();
    let Ok(rd) = std::fs::read_dir(paths::versions_dir()) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "tmp") {
            continue;
        }
        if std::fs::canonicalize(&p).ok() != keep {
            let _ = std::fs::remove_dir_all(&p);
        }
    }
}

/// Stops the hub and removes the logon start; without `autostart_only` also the files, unless
/// another recorded installation still exists.
pub fn uninstall(autostart_only: bool, extension: Option<String>) -> Result<Vec<String>, String> {
    let mut did = Vec::new();
    if stop_hub(true)? {
        did.push("stopped the hub".into());
    }
    if unregister_autostart()? {
        did.push(format!("removed the logon start ({})", run_value()));
    }
    if autostart_only {
        return Ok(did);
    }
    let _lock = Lock::take()?;
    let ext = extension.unwrap_or_else(|| paths::exe_dir().to_string_lossy().into_owned());
    let mut installs = read_installs();
    installs.retain(|i| *i != ext);
    installs.retain(|i| Path::new(i).exists());
    write_installs(&installs);
    if installs.is_empty() {
        let _ = std::fs::remove_dir(paths::current_dir()).or_else(|_| std::fs::remove_file(paths::current_dir()));
        let _ = std::fs::remove_dir_all(paths::versions_dir());
        let _ = std::fs::remove_file(installs_file());
        did.push(format!("removed {}", paths::versions_dir().display()));
    } else {
        did.push(format!("kept the files: still used by {}", installs.join(", ")));
    }
    Ok(did)
}

pub fn status() -> Value {
    let hub = running_hub().map(|(_, v)| v);
    json!({
        "running": hub,
        "autostart": autostart_registered(),
        "autostart_value": run_value(),
        "current": current_build(),
        "installed_exe": paths::current_dir().join(paths::exe_name("aurix-hub")),
        "installs": read_installs(),
        "config": paths::hub_toml(),
        "profiles": paths::profiles_dir(),
        "log": paths::hub_log(),
    })
}

#[cfg(windows)]
fn run_key(write: bool) -> std::io::Result<winreg::RegKey> {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
    let flags = if write { KEY_SET_VALUE | KEY_READ } else { KEY_READ };
    winreg::RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(r"Software\Microsoft\Windows\CurrentVersion\Run", flags)
}

pub fn register_autostart() -> Result<(), String> {
    #[cfg(windows)]
    {
        let exe = paths::current_dir().join(paths::exe_name("aurix-hub"));
        let exe = if exe.is_file() { exe } else { std::env::current_exe().map_err(|e| e.to_string())? };
        run_key(true).and_then(|k| k.set_value(run_value(), &format!("\"{}\" serve", exe.display()))).map_err(|e| format!("writing the Run key: {e}"))
    }
    #[cfg(not(windows))]
    {
        Err("logon start is implemented for Windows only".into())
    }
}

pub fn unregister_autostart() -> Result<bool, String> {
    #[cfg(windows)]
    {
        match run_key(true).and_then(|k| k.delete_value(run_value())) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(format!("removing the Run value: {e}")),
        }
    }
    #[cfg(not(windows))]
    {
        Ok(false)
    }
}

pub fn autostart_registered() -> bool {
    #[cfg(windows)]
    {
        run_key(false).and_then(|k| k.get_value::<String, _>(run_value())).is_ok()
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert!(version_of_build("0.2.0-aaaa") > version_of_build("0.1.9-bbbb"));
        assert!(version_of_build("0.10.0-a") > version_of_build("0.9.0-a"));
    }
}
