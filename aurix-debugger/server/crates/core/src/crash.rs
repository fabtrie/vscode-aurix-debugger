//! Puts the reason for a crash in the log.
//!
//! A worker started by the hub has no console: its stderr goes nowhere, so the message Rust
//! prints for a panic, or the one Windows has for an access violation, is lost, and the session
//! just ends. [`install`] sends both to the log file instead (the session log), next to what the
//! worker was doing.

use std::backtrace::Backtrace;
use std::sync::Once;

use crate::log;

static INSTALLED: Once = Once::new();

/// Installs the panic hook and, on Windows, the unhandled-exception filter. Call it first thing
/// in `main`; later calls do nothing.
pub fn install() {
    INSTALLED.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let thread = std::thread::current();
            let msg = info
                .payload()
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| info.payload().downcast_ref::<String>().map(String::as_str))
                .unwrap_or("(no message)");
            let at = info.location().map(|l| format!(" at {}:{}", l.file(), l.line())).unwrap_or_default();
            let mut text = format!("internal error: thread '{}' panicked{at}: {msg}", thread.name().unwrap_or("<unnamed>"));
            if let Some(bt) = backtrace() {
                text.push_str("\nbacktrace:\n");
                text.push_str(&bt);
            }
            // The default hook prints the panic to stderr: the log file only, not twice.
            log::log_fatal(&text, false);
            default(info);
        }));
        #[cfg(windows)]
        seh::install();
    });
}

/// The current backtrace, when it names functions (a build without symbols only has addresses,
/// which say nothing without the exact binary).
fn backtrace() -> Option<String> {
    let bt = Backtrace::force_capture().to_string();
    let named: Vec<&str> = bt.lines().filter(|l| !l.contains("<unknown>")).collect();
    named.iter().any(|l| l.contains("::")).then(|| named.join("\n"))
}

/// Exceptions nothing handled: access violations in C code (Jim, a probe DLL), stack overflows,
/// illegal instructions. The filter logs the code, the address and the module it lies in, then
/// lets Windows end the process as it would have (the hub reports the exit code).
#[cfg(windows)]
mod seh {
    use std::ffi::c_void;

    #[repr(C)]
    struct ExceptionRecord {
        code: u32,
        flags: u32,
        record: *mut ExceptionRecord,
        address: *mut c_void,
        number_parameters: u32,
        information: [usize; 15],
    }

    #[repr(C)]
    struct ExceptionPointers {
        record: *mut ExceptionRecord,
        context: *mut c_void,
    }

    type Filter = unsafe extern "system" fn(*mut ExceptionPointers) -> i32;

    #[link(name = "kernel32")]
    extern "system" {
        fn SetUnhandledExceptionFilter(filter: Option<Filter>) -> Option<Filter>;
        fn GetModuleHandleExW(flags: u32, address: *const c_void, module: *mut *mut c_void) -> i32;
        fn GetModuleFileNameW(module: *mut c_void, name: *mut u16, size: u32) -> u32;
    }

    const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
    const GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS: u32 = 0x4;
    const GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT: u32 = 0x2;
    const EXCEPTION_ACCESS_VIOLATION: u32 = 0xC000_0005;

    pub fn install() {
        unsafe { SetUnhandledExceptionFilter(Some(filter)) };
    }

    unsafe extern "system" fn filter(info: *mut ExceptionPointers) -> i32 {
        let Some(rec) = info.as_ref().and_then(|i| i.record.as_ref()) else { return EXCEPTION_CONTINUE_SEARCH };
        let mut text = format!(
            "fatal: unhandled exception 0x{:08X} ({}) at 0x{:X}{} in thread '{}'",
            rec.code,
            name(rec.code),
            rec.address as usize,
            module_of(rec.address),
            std::thread::current().name().unwrap_or("<unnamed>")
        );
        if rec.code == EXCEPTION_ACCESS_VIOLATION && rec.number_parameters >= 2 {
            let op = match rec.information[0] {
                0 => "reading",
                1 => "writing",
                8 => "executing",
                _ => "accessing",
            };
            text.push_str(&format!(", {op} 0x{:X}", rec.information[1]));
        }
        crate::log::log_fatal(&text, true);
        EXCEPTION_CONTINUE_SEARCH
    }

    fn name(code: u32) -> &'static str {
        match code {
            0xC000_0005 => "access violation",
            0xC000_001D => "illegal instruction",
            0xC000_0094 => "integer division by zero",
            0xC000_00FD => "stack overflow",
            0xC000_0374 => "heap corruption",
            0xC000_0409 => "fast fail",
            0x8000_0003 => "breakpoint",
            _ => "exception",
        }
    }

    /// ` (<module>+0x<offset>)` for an address inside a loaded module, so the crash site can be
    /// looked up in that exact binary.
    fn module_of(address: *mut c_void) -> String {
        let mut module = std::ptr::null_mut();
        let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
        if unsafe { GetModuleHandleExW(flags, address, &mut module) } == 0 || module.is_null() {
            return String::new();
        }
        let mut buf = [0u16; 260];
        let n = unsafe { GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32) } as usize;
        let path = String::from_utf16_lossy(&buf[..n.min(buf.len())]);
        let file = path.rsplit(['\\', '/']).next().unwrap_or(&path);
        format!(" ({file}+0x{:X})", address as usize - module as usize)
    }
}
