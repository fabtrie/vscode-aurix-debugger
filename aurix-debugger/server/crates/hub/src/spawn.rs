//! Starting processes that must outlive their parent: the workers (they keep a session through a
//! hub restart) and the hub itself (started by the stdio wrapper or the installer). Neither may
//! inherit handles: a worker holding the hub's (or a test runner's) pipes would keep them open
//! after the hub ended.

use std::path::Path;
#[cfg(not(windows))]
use std::process::{Command, Stdio};

/// Starts `exe args` fully detached: no console, no inherited handles (see [`crate::detach`]),
/// null stdio. Returns the process ID.
pub fn spawn_detached(exe: &Path, args: &[&str]) -> std::io::Result<u32> {
    #[cfg(windows)]
    {
        crate::detach::create_process(exe, args, None, &[])
    }
    #[cfg(not(windows))]
    {
        let child = Command::new(exe).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
        Ok(child.id())
    }
}

/// Starts a worker in `cwd`: detached like [`spawn_detached`] (it logs to the file its `-l`
/// names). Returns the process ID.
pub fn spawn_worker(exe: &Path, args: &[String], cwd: &Path) -> std::io::Result<u32> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    #[cfg(windows)]
    {
        crate::detach::create_process(exe, &args, Some(cwd), &[])
    }
    #[cfg(not(windows))]
    {
        let child = Command::new(exe).args(&args).current_dir(cwd).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
        Ok(child.id())
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

/// Watches a process that this hub did not start (the owner of a session). On Windows it holds a
/// handle, so the process ID cannot be reused by another process while it is watched.
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
