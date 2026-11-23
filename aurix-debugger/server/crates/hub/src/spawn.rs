//! Starting processes that must outlive their parent: the workers (they keep a session through a
//! hub restart) and the hub itself (started by the stdio wrapper or the installer). Neither may
//! inherit handles: a worker holding the hub's (or a test runner's) pipes would keep them open
//! after the hub ended.

use std::path::Path;
#[cfg(not(windows))]
use std::process::{Command, Stdio};

/// Starts `exe args` fully detached: no console, no inherited handles (see [`crate::detach`]),
/// null stdio, in [`own_dir`]. Returns the process ID.
pub fn spawn_detached(exe: &Path, args: &[&str]) -> std::io::Result<u32> {
    let dir = own_dir(exe);
    #[cfg(windows)]
    {
        crate::detach::create_process(exe, args, dir, &[])
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new(exe);
        if let Some(d) = dir {
            cmd.current_dir(d);
        }
        let child = cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
        Ok(child.id())
    }
}

/// The working directory of a detached hub: the folder of its executable. It exists while the
/// process runs, and the hub does not pin whatever folder its starter ran in (a worktree, an
/// extension folder that an update replaces): Windows refuses to delete a process's working
/// directory.
pub fn own_dir(exe: &Path) -> Option<&Path> {
    exe.parent().filter(|d| !d.as_os_str().is_empty())
}

/// Starts a worker in `cwd`: detached like [`spawn_detached`] (it logs to the file its `-l`
/// names). Returns the process ID and a watch on the process that also reads its exit code: the
/// worker's stdio goes nowhere, so how it ended is all a crash leaves behind.
pub fn spawn_worker(exe: &Path, args: &[String], cwd: &Path) -> std::io::Result<(u32, ProcessWatch)> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    #[cfg(windows)]
    {
        let (pid, handle) = crate::detach::create_process_handle(exe, &args, Some(cwd), &[])?;
        Ok((pid, ProcessWatch { handle }))
    }
    #[cfg(not(windows))]
    {
        let child = Command::new(exe).args(&args).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
        Ok((child.id(), ProcessWatch { pid: child.id() }))
    }
}

/// Whether process `pid` still runs.
pub fn alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if h.is_null() {
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(h, &mut code);
            CloseHandle(h);
            ok != 0 && code == STILL_ACTIVE as u32
        }
    }
    #[cfg(not(windows))]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
}

/// Watches a process: the owner of a session, or a worker. On Windows it holds a handle, so the
/// process ID cannot be reused by another process while it is watched.
pub struct ProcessWatch {
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
    #[cfg(not(windows))]
    pid: u32,
}

// The handle is only a kernel object reference: usable from any thread.
#[cfg(windows)]
unsafe impl Send for ProcessWatch {}

impl ProcessWatch {
    /// Starts watching process `pid`; `None` when no such process runs.
    pub fn open(pid: u32) -> Option<ProcessWatch> {
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE};
            let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE, 0, pid) };
            let w = (!handle.is_null()).then_some(ProcessWatch { handle })?;
            (!w.ended()).then_some(w)
        }
        #[cfg(not(windows))]
        {
            alive(pid).then_some(ProcessWatch { pid })
        }
    }

    /// Whether the process ended.
    pub fn ended(&self) -> bool {
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
            use windows_sys::Win32::System::Threading::WaitForSingleObject;
            unsafe { WaitForSingleObject(self.handle, 0) != WAIT_TIMEOUT }
        }
        #[cfg(not(windows))]
        {
            !alive(self.pid)
        }
    }

    /// The exit code of the ended process (`None` while it runs, or where it cannot be read).
    pub fn exit_code(&self) -> Option<u32> {
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Threading::GetExitCodeProcess;
            let mut code = 0u32;
            (self.ended() && unsafe { GetExitCodeProcess(self.handle, &mut code) } != 0).then_some(code)
        }
        #[cfg(not(windows))]
        {
            None
        }
    }
}

/// How a process ended, from its exit code: the code, and what it means when it says more than
/// "failed" (a Rust panic, a Windows exception).
pub fn describe_exit(code: Option<u32>) -> String {
    let Some(code) = code else { return "exit code unknown".into() };
    let what = match code {
        101 => "a Rust panic, see the log",
        0xC000_0005 => "access violation",
        0xC000_001D => "illegal instruction",
        0xC000_0094 => "integer division by zero",
        0xC000_00FD => "stack overflow",
        0xC000_0135 => "a DLL was not found",
        0xC000_0139 => "a DLL entry point was not found",
        0xC000_013A => "ended with Ctrl+C",
        0xC000_0374 => "heap corruption",
        0xC000_0409 => "fast fail: abort or stack buffer overrun",
        0x4001_0004 => "the debugger ended it",
        _ => "",
    };
    match (code, what.is_empty()) {
        (0..=0xFFFF, true) => format!("exit code {code}"),
        (_, true) => format!("exit code 0x{code:08X}"),
        (0..=0xFFFF, false) => format!("exit code {code}: {what}"),
        (_, false) => format!("exit code 0x{code:08X}: {what}"),
    }
}

#[cfg(windows)]
impl Drop for ProcessWatch {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.handle) };
    }
}

/// Ends process `pid` the hard way.
pub fn kill(pid: u32) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
        unsafe {
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if !h.is_null() {
                TerminateProcess(h, 1);
                CloseHandle(h);
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).stdin(Stdio::null()).status();
    }
}
