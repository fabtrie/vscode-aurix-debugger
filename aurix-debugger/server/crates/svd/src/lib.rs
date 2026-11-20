//! CMSIS-SVD peripheral descriptions: the registers and fields of a device, read from its SVD
//! file (and extra files merged into it, e.g. the HSM of the TC3xx).
//!
//! The conversion is the one the VS Code extension did in TypeScript, itself a port of the
//! peripheral viewer's (mcu-debug.peripheral-viewer 1.6.5) SVD parser: the same peripherals,
//! registers, fields and enumerations, named the same way (`STM0.CMP[0]`). [`PeripheralOptions`]
//! writes them as the definitions that view takes.
//!
//! The files are large (TC4Dx: 154 MB of XML), so they are read as a stream, straight from the
//! `.gz`, one peripheral at a time; only the model is kept. Files that need more than one
//! peripheral at a time (a peripheral `derivedFrom` another, a register derived from another
//! peripheral's, two peripherals of one name) are read again with all peripherals in memory.

mod convert;
mod options;
mod xml;

use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::Path;
use std::sync::Arc;

pub use options::PeripheralOptions;

/// A fast hash for the many short keys of a conversion (tag names, interned strings).
#[derive(Default)]
pub(crate) struct Fx(u64);

impl std::hash::Hasher for Fx {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0.rotate_left(5) ^ b as u64).wrapping_mul(0x517c_c1b7_2722_0a95);
        }
    }
}

pub(crate) type FastHash = std::hash::BuildHasherDefault<Fx>;

/// A shared (interned) string of the model.
pub type Str = Arc<str>;

/// Access type, as the peripheral viewer has it (`writeOnce` counts as write-only, an unknown or
/// empty `<access>` as read-only).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Access {
    ReadOnly = 1,
    ReadWrite = 2,
    WriteOnly = 3,
}

/// Side effect of reading (`<readAction>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReadAction {
    Clear = 1,
    Set = 2,
    Modify = 3,
    ModifyExternal = 4,
}

/// What writing a value does (`<modifiedWriteValues>`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModifiedWrite {
    OneToClear,
    OneToSet,
    OneToToggle,
    ZeroToClear,
    ZeroToSet,
    ZeroToToggle,
    Clear,
    Set,
    Modify,
}

/// A device: its peripherals sorted by group, then name.
#[derive(Clone, Debug, Default)]
pub struct Device {
    /// `<name>` of the device file.
    pub name: Option<Str>,
    pub peripherals: Vec<Peripheral>,
}

#[derive(Clone, Debug)]
pub struct Peripheral {
    pub name: Str,
    pub base_address: u64,
    /// End of the furthest address block.
    pub total_length: u64,
    pub description: Str,
    pub group_name: Option<Str>,
    /// The peripheral's, else the device's default (read-write).
    pub access: Access,
    /// Register size in bits: the peripheral's, else the device's default (32).
    pub size: u32,
    pub reset_value: u64,
    pub registers: Vec<Register>,
    pub clusters: Vec<Cluster>,
}

#[derive(Clone, Debug)]
pub struct Cluster {
    pub name: Str,
    pub description: Str,
    /// From the parent (peripheral or cluster).
    pub address_offset: u64,
    pub access: Option<Access>,
    pub size: Option<u32>,
    pub reset_value: Option<u64>,
    pub registers: Vec<Register>,
    pub clusters: Vec<Cluster>,
}

#[derive(Clone, Debug)]
pub struct Register {
    pub name: Str,
    pub description: Str,
    /// From the parent (peripheral or cluster).
    pub address_offset: u64,
    /// Only where the register declares them; the parent's apply otherwise.
    pub access: Option<Access>,
    pub size: Option<u32>,
    pub reset_value: Option<u64>,
    pub read_action: Option<ReadAction>,
    pub modified_write: Option<ModifiedWrite>,
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug)]
pub struct Field {
    pub name: Str,
    pub description: Str,
    /// Least significant bit.
    pub offset: u32,
    pub width: u32,
    pub enumeration: Option<Arc<Enumeration>>,
    pub access: Option<Access>,
    pub read_action: Option<ReadAction>,
    pub modified_write: Option<ModifiedWrite>,
}

/// The named values of a field, by value.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Enumeration {
    /// Ascending by value.
    pub values: Vec<EnumValue>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EnumValue {
    pub value: u64,
    pub name: Str,
    pub description: Str,
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// Not well-formed XML.
    Xml(String),
    /// Not an SVD file the conversion understands.
    Svd(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Xml(e) => write!(f, "XML: {e}"),
            Error::Svd(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

/// An extra SVD file whose peripherals are merged into the device's.
#[derive(Clone, Copy, Debug)]
pub struct Extra<'a, S> {
    pub source: S,
    /// Device peripherals it replaces.
    pub replace: &'a [String],
    /// Its peripherals that are not merged.
    pub exclude: &'a [String],
}

type Opener<'a> = Box<dyn Fn() -> io::Result<Box<dyn BufRead + 'a>> + 'a>;

/// An input of the conversion: re-openable, as a file that needs all peripherals at once is
/// read a second time.
pub(crate) struct Input<'a> {
    pub open: Opener<'a>,
    pub replace: &'a [String],
    pub exclude: &'a [String],
}

fn open_file(path: &Path) -> io::Result<Box<dyn BufRead>> {
    let f = File::open(path)?;
    let gz = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("gz"));
    Ok(if gz { Box::new(BufReader::with_capacity(1 << 16, flate2::read::GzDecoder::new(f))) } else { Box::new(BufReader::with_capacity(1 << 16, f)) })
}

/// Reads a device's SVD file (`.svd` or `.svd.gz`) and merges the peripherals of `extras` into
/// it, as the extension merged them: the peripherals they `replace` removed, theirs appended
/// except those they `exclude`. A merged peripheral of a name the device already has is an error.
pub fn load(device: &Path, extras: &[Extra<'_, &Path>]) -> Result<Device, Error> {
    let none: &[String] = &[];
    let mut inputs = vec![Input { open: Box::new(move || open_file(device)), replace: none, exclude: none }];
    for x in extras {
        let path = x.source;
        inputs.push(Input { open: Box::new(move || open_file(path)), replace: x.replace, exclude: x.exclude });
    }
    convert::convert(&inputs)
}

/// [`load`] from SVD text.
pub fn parse(device: &str, extras: &[Extra<'_, &str>]) -> Result<Device, Error> {
    let none: &[String] = &[];
    let mut inputs = vec![Input { open: Box::new(move || Ok(Box::new(device.as_bytes()) as Box<dyn BufRead>)), replace: none, exclude: none }];
    for x in extras {
        let text = x.source;
        inputs.push(Input { open: Box::new(move || Ok(Box::new(text.as_bytes()) as Box<dyn BufRead>)), replace: x.replace, exclude: x.exclude });
    }
    convert::convert(&inputs)
}
