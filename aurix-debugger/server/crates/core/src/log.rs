//! Minimal leveled logger with OpenOCD-style prefixes and pluggable sinks.

use std::cell::RefCell;
use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;

thread_local! {
    static CAPTURE: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

/// Runs `f` and returns the log lines it produced on this thread. Captured
/// lines still go to stderr and the log file, but not to the sinks (so a
/// console session gets them in order, with its command result).
pub fn capture<R>(f: impl FnOnce() -> R) -> (R, Vec<String>) {
    let prev = CAPTURE.with(|c| c.replace(Some(Vec::new())));
    let r = f();
    let lines = CAPTURE.with(|c| c.replace(prev)).unwrap_or_default();
    (r, lines)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Output of `echo` and similar - always shown, no prefix.
    User = 0,
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
}

/// OpenOCD debug_level: 0 errors, 1 +warnings, 2 +info (default), 3 +debug.
static DEBUG_LEVEL: AtomicU8 = AtomicU8::new(2);

type Sink = Box<dyn Fn(Level, &str) + Send>;
static SINKS: Mutex<Vec<Sink>> = Mutex::new(Vec::new());
static FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);
static STDERR: AtomicU8 = AtomicU8::new(1);

pub fn set_debug_level(level: u8) {
    DEBUG_LEVEL.store(level.min(4), Ordering::Relaxed);
}

pub fn debug_level() -> u8 {
    DEBUG_LEVEL.load(Ordering::Relaxed)
}

/// Enables/disables mirroring log lines to stderr.
pub fn set_stderr(on: bool) {
    STDERR.store(on as u8, Ordering::Relaxed);
}

pub fn set_output_file(path: &str) -> std::io::Result<()> {
    let f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    *FILE.lock().unwrap_or_else(|p| p.into_inner()) = Some(f);
    Ok(())
}

pub fn add_sink(sink: Sink) {
    SINKS.lock().unwrap_or_else(|p| p.into_inner()).push(sink);
}

pub fn enabled(level: Level) -> bool {
    level == Level::User || (level as u8 - 1) <= debug_level()
}

pub fn log(level: Level, msg: &str) {
    if !enabled(level) {
        return;
    }
    let line = match level {
        Level::User => msg.to_string(),
        Level::Error => format!("Error: {msg}"),
        Level::Warn => format!("Warn : {msg}"),
        Level::Info => format!("Info : {msg}"),
        Level::Debug => format!("Debug: {msg}"),
    };
    if STDERR.load(Ordering::Relaxed) != 0 {
        eprintln!("{line}");
    }
    if let Some(f) = FILE.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
        let _ = writeln!(f, "{line}");
    }
    let captured = CAPTURE.with(|c| match c.borrow_mut().as_mut() {
        Some(v) => {
            v.push(line.clone());
            true
        }
        None => false,
    });
    if !captured {
        for s in SINKS.lock().unwrap_or_else(|p| p.into_inner()).iter() {
            s(level, &line);
        }
    }
}

/// Writes an error line to the log file (and to stderr with `to_stderr`, where the caller has no
/// other output) from a crash handler: a panic hook, an unhandled exception. No sinks, since their
/// locks may be what the crashed thread held, and the file lock only for as long as another
/// thread may reasonably keep it. A poisoned lock is still used: losing the reason for a crash is
/// worse than an interleaved line.
pub fn log_fatal(msg: &str, to_stderr: bool) {
    use std::sync::TryLockError;
    let line = format!("Error: {msg}");
    if to_stderr && STDERR.load(Ordering::Relaxed) != 0 {
        let _ = writeln!(std::io::stderr(), "{line}");
    }
    for _ in 0..100 {
        let mut guard = match FILE.try_lock() {
            Ok(g) => g,
            Err(TryLockError::Poisoned(p)) => p.into_inner(),
            Err(TryLockError::WouldBlock) => {
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
        };
        if let Some(f) = guard.as_mut() {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
        return;
    }
}

#[macro_export]
macro_rules! log_error { ($($a:tt)*) => { $crate::log::log($crate::log::Level::Error, &format!($($a)*)) } }
#[macro_export]
macro_rules! log_warn { ($($a:tt)*) => { $crate::log::log($crate::log::Level::Warn, &format!($($a)*)) } }
#[macro_export]
macro_rules! log_info { ($($a:tt)*) => { $crate::log::log($crate::log::Level::Info, &format!($($a)*)) } }
#[macro_export]
macro_rules! log_debug { ($($a:tt)*) => { if $crate::log::enabled($crate::log::Level::Debug) { $crate::log::log($crate::log::Level::Debug, &format!($($a)*)) } } }
#[macro_export]
macro_rules! log_user { ($($a:tt)*) => { $crate::log::log($crate::log::Level::User, &format!($($a)*)) } }
