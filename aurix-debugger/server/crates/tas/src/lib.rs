//! Pure-Rust client for Infineon's TAS (Tool Access Socket) protocol.
//!
//! Ported from the Apache-2.0 TAS Client API (`third_party/tas_client_api-main`),
//! read/write client only. All values on the wire are little endian and
//! word (4 byte) aligned. Three packet layers:
//! - PL2: a leading u32 with the overall packet length in bytes (including itself)
//! - PL1: server commands, or a PL0 frame (`PL0_START` .. `PL0_END`)
//! - PL0: read/write transactions inside a PL0 frame

mod batch;
mod client;
mod proto;

pub use batch::{Batch, BatchOp, OpHandle, OpResult};
pub use client::{ConInfo, ConnectMode, ServerInfo, TargetInfo, TasClient};
pub use proto::{
    pl_err_name, DEFAULT_PORT, PL0_ERR_ADDR_BLOCKED, PL0_ERR_CONSEQUENTIAL, PL0_ERR_DATA,
    PL0_ERR_NO_ERROR,
};

use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// Unexpected packet layout from the server.
    Protocol(String),
    /// Server or device reported an error for a PL1 command.
    Command { cmd: u8, err: u8, msg: String },
    /// Device access failed (power-down, reset active, no connection, ...).
    DeviceAccess,
    /// Device is locked (e.g. OCDS not enabled after a reset).
    DeviceLocked,
    /// Session name or password mismatch - another client holds the device.
    Session,
    /// A PL0 read/write failed. `op` is the index in the batch.
    Rw { op: usize, addr: u64, err: u8, write: bool },
    Usage(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "TAS I/O error: {e}"),
            Error::Protocol(m) => write!(f, "TAS protocol error: {m}"),
            Error::Command { cmd, err, msg } => {
                write!(f, "TAS command 0x{cmd:02X} failed: {} ({msg})", pl_err_name(*err))
            }
            Error::DeviceAccess => write!(f, "TAS: device access failed (power-down, reset active, not connected)"),
            Error::DeviceLocked => write!(f, "TAS: device is locked"),
            Error::Session => write!(f, "TAS: session name or password mismatch (device used by another client?)"),
            Error::Rw { addr, err, write, .. } => write!(
                f,
                "{} of address 0x{addr:08X} failed: {}",
                if *write { "Write" } else { "Read" },
                pl_err_name(*err)
            ),
            Error::Usage(m) => write!(f, "TAS usage error: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
