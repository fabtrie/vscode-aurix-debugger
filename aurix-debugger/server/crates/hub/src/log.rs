//! The hub's log: `logs\hub.log` (the hub has no console), and stderr when it has one.

use std::io::Write;
use std::sync::Mutex;

static FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);
static STDERR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Appends to `path` from now on.
pub fn to_file(path: &std::path::Path) {
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        *FILE.lock().unwrap() = Some(f);
    }
}

/// Stops writing to stderr (the stdio wrapper's stdout/stderr belong to the MCP client).
pub fn quiet() {
    STDERR.store(false, std::sync::atomic::Ordering::Relaxed);
}

fn stamp() -> String {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default();
    let (h, m, s) = ((t / 3600) % 24, (t / 60) % 60, t % 60);
    format!("{h:02}:{m:02}:{s:02}Z")
}

pub fn line(msg: &str) {
    let l = format!("{} [pid {}] {msg}", stamp(), std::process::id());
    if STDERR.load(std::sync::atomic::Ordering::Relaxed) {
        eprintln!("{l}");
    }
    if let Some(f) = FILE.lock().unwrap().as_mut() {
        let _ = writeln!(f, "{l}");
    }
}

#[macro_export]
macro_rules! hlog { ($($a:tt)*) => { $crate::log::line(&format!($($a)*)) } }
