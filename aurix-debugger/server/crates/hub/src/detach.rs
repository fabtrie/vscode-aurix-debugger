//! Processes without inherited handles (Windows).
//!
//! A process started by Node (Claude Code, VS Code) or a shell inherits that parent's
//! inheritable handles: the pipes of the stdio wrapper, a console. A hub that kept them would
//! keep Claude's pipe open after the wrapper exited, and the shell's prompt would not return.
//! `std::process::Command` always creates children with `bInheritHandles = TRUE`, so the hub
//! fixes it from its own side: on its first start it starts a copy of itself that inherits
//! nothing and exits.

#![cfg(windows)]

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows_sys::Win32::Foundation::{CloseHandle, FALSE};
use windows_sys::Win32::System::Threading::{CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW};

const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Env marker of the re-started copy, so it runs instead of starting another one.
pub const MARKER_ENV: &str = "AURIX_HUB_DETACHED";

fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::from("\"");
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

/// `CreateProcessW` with `bInheritHandles = FALSE`, no console and outside the parent's job
/// where allowed, in `cwd` (default: ours). `env` adds variables to the inherited environment
/// (they are set on this process: only for a process that has no other threads yet). Returns
/// the process ID.
pub fn create_process(exe: &Path, args: &[&str], cwd: Option<&Path>, env: &[(&str, &str)]) -> std::io::Result<u32> {
    // The child inherits the environment block; set the additions on ourselves first.
    for (k, v) in env {
        // SAFETY: called before other threads look at these variables (startup, or a fresh copy).
        unsafe { std::env::set_var(k, v) };
    }
    let mut line = quote(&exe.to_string_lossy());
    for a in args {
        line.push(' ');
        line.push_str(&quote(a));
    }
    let mut cmdline: Vec<u16> = line.encode_utf16().chain([0]).collect();
    let app: Vec<u16> = exe.as_os_str().encode_wide().chain([0]).collect();
    let dir: Option<Vec<u16>> = cwd.map(|d| d.as_os_str().encode_wide().chain([0]).collect());
    let dir_ptr = dir.as_ref().map_or(std::ptr::null(), |d| d.as_ptr());
    let base = DETACHED_PROCESS | CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
    for flags in [base | CREATE_BREAKAWAY_FROM_JOB, base] {
        let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: the buffers outlive the call; null pointers inherit environment and directory.
        let ok = unsafe {
            CreateProcessW(app.as_ptr(), cmdline.as_mut_ptr(), std::ptr::null(), std::ptr::null(), FALSE, flags, std::ptr::null(), dir_ptr, &si, &mut pi)
        };
        if ok != 0 {
            unsafe {
                CloseHandle(pi.hProcess);
                CloseHandle(pi.hThread);
            }
            return Ok(pi.dwProcessId);
        }
    }
    Err(std::io::Error::last_os_error())
}

/// On the first start, starts a copy without inherited handles and exits. Returns when this
/// process is that copy (or the copy could not be started: then it runs attached).
pub fn self_detach(args: &[String]) {
    if std::env::var_os(MARKER_ENV).is_some() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match create_process(&exe, &args, None, &[(MARKER_ENV, "1")]) {
        Ok(_) => std::process::exit(0),
        Err(e) => crate::log::line(&format!("could not detach ({e}); running attached")),
    }
}

#[cfg(test)]
mod tests {
    use super::quote;

    #[test]
    fn quoting() {
        assert_eq!(quote("a"), "a");
        assert_eq!(quote(r"C:\Program Files\x"), r#""C:\Program Files\x""#);
        assert_eq!(quote(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote(r"dir\ "), r#""dir\ ""#);
        assert_eq!(quote(""), r#""""#);
    }
}
