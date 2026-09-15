use std::fmt;

#[derive(Debug, Clone)]
pub enum Error {
    /// Connection or protocol error of the debug adapter.
    Adapter(String),
    /// Device locked (e.g. OCDS disabled by a reset).
    DeviceLocked,
    /// Device not accessible (power-down, reset active).
    DeviceAccess,
    Memory { addr: u64, write: bool, detail: String },
    NotHalted(String),
    NotExamined(String),
    Resource(String),
    Timeout(String),
    Usage(String),
    Other(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Adapter(m) => f.write_str(m),
            Error::DeviceLocked => f.write_str("Device is locked"),
            Error::DeviceAccess => f.write_str("Device access failed (power-down, reset active?)"),
            Error::Memory { addr, write, detail } => write!(
                f,
                "Failed to {} memory at 0x{addr:08x}: {detail}",
                if *write { "write" } else { "read" }
            ),
            Error::NotHalted(t) => write!(f, "[{t}] Target not halted"),
            Error::NotExamined(t) => write!(f, "[{t}] Target not examined yet"),
            Error::Resource(m) | Error::Timeout(m) | Error::Usage(m) | Error::Other(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Error {}

impl From<tas::Error> for Error {
    fn from(e: tas::Error) -> Self {
        match e {
            tas::Error::DeviceLocked => Error::DeviceLocked,
            tas::Error::DeviceAccess => Error::DeviceAccess,
            tas::Error::Rw { addr, err, write, .. } => {
                Error::Memory { addr, write, detail: tas::pl_err_name(err).to_string() }
            }
            other => Error::Adapter(other.to_string()),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub fn usage(msg: impl Into<String>) -> Error {
    Error::Usage(msg.into())
}

pub fn other(msg: impl Into<String>) -> Error {
    Error::Other(msg.into())
}
