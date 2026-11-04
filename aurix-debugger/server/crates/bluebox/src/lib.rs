//! Direct USB access to an iSYSTEM BlueBox (iC5700), without winIDEA.
//!
//! Everything here was read off USB captures of winIDEA 9.21.356 driving a
//! TC377 through the probe; nothing comes from iSYSTEM documentation. The
//! layers, bottom up:
//!
//! * [`link`]: moving bytes over the probe's three bulk pipes. On Windows,
//!   [`usb::UsbLink`] does it through WinUSB, the driver the probe is installed
//!   with.
//! * [`frame`]: the 8-byte frame header every OUT transfer starts with.
//! * [`message`]: service messages, addressed by a text path such as `//2///16/`.
//! * [`memory`]: the memory service, request and reply.
//! * [`probe::Probe`]: a session on top of a link: it keeps the toggle bit and
//!   splits long accesses.
//! * [`lowlevel`] and [`kernel`]: the link bring-up: hello, the handshake, and
//!   loading the probe's kernel (`iC6kKrnl.s` from a winIDEA install) after a
//!   power cycle.
//! * [`replay`]: plays back a captured winIDEA attach for what follows (target
//!   connection, core discovery), which is not decoded yet.
//!
//! What is not decoded yet: the target connection, reset, and the services behind run control, which
//! [`Probe`](probe::Probe) does not need because the TriCore debug registers
//! are reachable as memory.

pub mod attach;
pub mod frame;
pub mod kernel;
pub mod link;
pub mod lowlevel;
pub mod memory;
pub mod message;
pub mod probe;
pub mod replay;
pub mod reset;
#[cfg(windows)]
pub mod usb;

pub use link::Link;
pub use memory::{MemReply, MAX_CHUNK};
pub use kernel::Kernel;
pub use lowlevel::{bring_up, Handshake};
pub use probe::{Exchange, Fetch, Probe};

/// USB ids of the iC5700 BlueBox.
pub const VID: u16 = 0x06f9;
pub const PID: u16 = 0x5700;

#[derive(Debug)]
pub enum Error {
    /// A Win32/WinUSB call failed; `code` is `GetLastError()`.
    Usb { op: &'static str, code: u32 },
    /// No probe with the requested ids or serial is plugged in.
    NotFound(String),
    /// The probe answered with something this crate cannot parse.
    Protocol(String),
    /// Reading a replay trace failed.
    Trace(String),
    /// Loading the probe's kernel failed, or it is needed and missing.
    Kernel(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Usb { op, code } => {
                let hint = match *code {
                    5 => " (access denied: is winIDEA, or another debugger, holding the probe?)",
                    121 => " (timed out: the probe sent nothing)",
                    _ => "",
                };
                write!(f, "{op} failed: Win32 error {code}{hint}")
            }
            Error::NotFound(s) => write!(f, "no BlueBox found: {s}"),
            Error::Protocol(s) => write!(f, "unexpected answer from the probe: {s}"),
            Error::Trace(s) => write!(f, "replay trace: {s}"),
            Error::Kernel(s) => write!(f, "probe kernel: {s}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// Hex of the first `max` bytes, for error messages.
pub(crate) fn hex(b: &[u8], max: usize) -> String {
    let mut s: String = b.iter().take(max).map(|x| format!("{x:02x} ")).collect();
    if b.len() > max {
        s.push_str(&format!("... ({} bytes)", b.len()));
    }
    s.trim_end().to_string()
}
