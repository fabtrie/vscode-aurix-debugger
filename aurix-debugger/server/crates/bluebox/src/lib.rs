//! Direct USB access to an iSYSTEM BlueBox (iC5700 or iC5000), without winIDEA.
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
//!   loading the probe's kernel (`iC6kKrnl.s`, or `iC3kKrnl.s` for an iC5000,
//!   from a winIDEA install) after a power cycle, or in place of another
//!   winIDEA version's.
//! * [`fpga`]: which debug FPGA image an iC5700 holds; the iC5000's upload.
//! * [`Model`]: what differs between the two BlueBoxes.
//! * [`replay`]: plays back a captured winIDEA attach for what follows (target
//!   connection, core discovery), which is not decoded yet.
//!
//! What is not decoded yet: the target connection, reset, and the services behind run control, which
//! [`Probe`](probe::Probe) does not need because the TriCore debug registers
//! are reachable as memory.

pub mod attach;
#[cfg(test)]
mod fixture;
pub mod fpga;
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
pub use memory::{Access, MemReply, MAX_CHUNK};
pub use kernel::Kernel;
pub use lowlevel::{bring_up, Handshake, KernelState};
pub use probe::{Exchange, Fetch, Probe};

/// USB vendor id of iSYSTEM probes.
pub const VID: u16 = 0x06f9;

/// The BlueBox models this crate drives. Both speak the same frames, messages
/// and services (the memory service byte for byte); they differ in the bring-up
/// (kernel file, session value, answer padding, debug FPGA) and in the attach.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Model {
    /// iC5700 (USB PID 5700): kernel `iC6kKrnl.s`, debug FPGA kept in flash.
    #[default]
    Ic5700,
    /// iC5000 (USB PID 3200): kernel `iC3kKrnl.s`, debug FPGA uploaded on
    /// every attach ([`fpga::ic5000`]).
    Ic5000,
}

impl Model {
    pub const ALL: [Model; 2] = [Model::Ic5700, Model::Ic5000];

    pub fn pid(self) -> u16 {
        match self {
            Model::Ic5700 => 0x5700,
            Model::Ic5000 => 0x3200,
        }
    }

    pub fn from_pid(pid: u16) -> Option<Model> {
        Model::ALL.into_iter().find(|m| m.pid() == pid)
    }

    pub fn name(self) -> &'static str {
        match self {
            Model::Ic5700 => "iC5700",
            Model::Ic5000 => "iC5000",
        }
    }

    /// The kernel file in a winIDEA install.
    pub fn kernel_file(self) -> &'static str {
        match self {
            Model::Ic5700 => "iC6kKrnl.s",
            Model::Ic5000 => "iC3kKrnl.s",
        }
    }

    /// What a frame's payload is padded to a multiple of.
    pub fn frame_pad(self) -> usize {
        match self {
            Model::Ic5700 => 4,
            Model::Ic5000 => 2,
        }
    }

    /// Capacity of the read that fetches an `n`-byte low-level answer, as
    /// winIDEA asks for it: `08` plus the answer, padded to 4 bytes on the
    /// iC5700; `n` rounded up to even on the iC5000, which answers that many
    /// bytes, `08` included, so an even-length answer loses its last byte.
    pub fn answer_capacity(self, n: usize) -> usize {
        match self {
            Model::Ic5700 => frame::padded(n + 1),
            Model::Ic5000 => n.next_multiple_of(2),
        }
    }
}

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
    /// The probe's debug FPGA: its image, or loading one.
    Fpga(String),
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
            Error::Fpga(s) => write!(f, "probe FPGA: {s}"),
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
