//! TC3xx User Configuration Blocks (UCBs): description, readout, view and export.
//!
//! The 48 UCBs of 512 bytes at 0xAF400000 hold the boot mode headers, the flash and debug
//! protection, the HSM and OTP configuration and Infineon's production data. The layout is the
//! one of TC3xx UM Part 1 V2.0, UCB chapter (Tables 215-229); the bit fields are those of the
//! `DMU_*_PROCON*` registers each UCB word is loaded into (identical in all TC3xx SVDs except for
//! the number of PFLASH banks and CPUs, which [`Device`] supplies). Writing is in
//! [`crate::ucb_write`].
//!
//! Reading respects the access protection (UM 6.5.4.3): words the DMU would answer with a bus
//! error (passwords of a CONFIRMED UCB, a CONFIRMED read-protected UCB) are not read but reported
//! as unreadable, judged from `DMU_HF_CONFIRM0..2` and `DMU_HF_PROTECT` before the read.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::adapter::Io;
use crate::error::{other, usage, Result};
use crate::image::{Image, Section};

pub const BASE: u32 = 0xAF40_0000;
pub const SIZE: usize = 0x200;
pub const WORDS: usize = SIZE / 4;
pub const COUNT: usize = 48;
pub const END: u64 = BASE as u64 + (COUNT * SIZE) as u64;
pub const UNLOCKED: u32 = 0x4321_1234;
pub const CONFIRMED: u32 = 0x57B5_327F;
/// Offset of the main confirmation code in a UCB.
pub const CONF: u16 = 0x1F0;
/// Offset of PW0 in the UCBs with a password.
pub const PW: u16 = 0x100;

pub const SCU_CHIPID: u32 = 0xF003_6140;
pub const HF_PROTECT: u32 = 0xF804_001C;
pub const HF_CONFIRM: [u32; 3] = [0xF804_0020, 0xF804_0024, 0xF804_0028];
pub const HF_PROCONUSR: u32 = 0xF804_0074;
pub const HF_PROCONPF: u32 = 0xF804_0080;
pub const HF_PROCONTP: u32 = 0xF804_0084;
pub const HF_PROCONDF: u32 = 0xF804_0088;
pub const HF_PROCONRAM: u32 = 0xF804_008C;
pub const HF_PROCONDBG: u32 = 0xF804_0090;
pub const SP_PROCONHSMCFG: u32 = 0xF807_0000;
pub const SP_PROCONHSMCBS: u32 = 0xF807_0004;
pub const SP_PROCONHSM: u32 = 0xF807_0040;
/// The installed protection registers read with every readout.
pub const INSTALLED: &[u32] =
    &[HF_PROCONUSR, HF_PROCONPF, HF_PROCONTP, HF_PROCONDF, HF_PROCONRAM, HF_PROCONDBG, SP_PROCONHSMCFG, SP_PROCONHSMCBS, SP_PROCONHSM];

/// `DMU_HF_PROTECT` bits: protection temporarily disabled with the password.
pub const PRODISP: u8 = 0;
pub const PRODISD: u8 = 1;
pub const PRODISDBG: u8 = 2;
pub const PRODISEC: u8 = 3;
pub const PRODISBMHD: u8 = 4;
pub const PRODISSWAP: u8 = 5;

/// Address of UCB `index` (0..47).
pub fn ucb_addr(index: u8) -> u32 {
    BASE + index as u32 * SIZE as u32
}

// --- Description -----------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    Boot,
    Protection,
    Hsm,
    Otp,
    Infineon,
    Spare,
}

impl Class {
    pub fn key(self) -> &'static str {
        match self {
            Class::Boot => "boot",
            Class::Protection => "protection",
            Class::Hsm => "hsm",
            Class::Otp => "otp",
            Class::Infineon => "infineon",
            Class::Spare => "spare",
        }
    }
    pub fn group(self) -> &'static str {
        match self {
            Class::Boot => "Boot mode headers",
            Class::Protection => "Protection",
            Class::Hsm => "HSM",
            Class::Otp => "OTP / WOP - permanent",
            Class::Infineon => "Infineon data",
            Class::Spare => "Spare",
        }
    }
    pub fn parse(s: &str) -> Option<Class> {
        [Class::Boot, Class::Protection, Class::Hsm, Class::Otp, Class::Infineon, Class::Spare].into_iter().find(|c| c.key() == s)
    }
}

/// Whether the debugger may change a UCB at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Writable through the DMU, subject to its confirmation state and the checks.
    Writable,
    /// The hardware never accepts a write (UM 6.5.4.3); the reason is shown.
    ReadOnly(&'static str),
}

pub struct UcbDef {
    /// Name without the `UCB_` prefix, e.g. `BMHD0`.
    pub name: &'static str,
    /// The UCB index (0..47) of ORIG, or of the single UCB.
    pub orig: u8,
    /// The UCB index of COPY, for the ORIG/COPY pairs.
    pub copy: Option<u8>,
    pub class: Class,
    /// Offsets of the confirmation codes inside each UCB of the definition.
    pub confirm: &'static [u16],
    /// Disable Protection UC code, for the UCBs with a password (PW0..PW7 at 0x100).
    pub uc: Option<u8>,
    /// `DMU_HF_CONFIRMx`: register, ORIG field position, COPY field position.
    pub conf_bits: Option<(usize, u8, Option<u8>)>,
    /// `DMU_HF_PROTECT` bit set while its protection is disabled.
    pub prodis: Option<u8>,
    pub access: Access,
    pub about: &'static str,
}

impl UcbDef {
    pub fn indices(&self) -> impl Iterator<Item = u8> {
        std::iter::once(self.orig).chain(self.copy)
    }
    pub fn has_password(&self) -> bool {
        self.uc.is_some()
    }
}

const C1: &[u16] = &[0x1F0];
const C2: &[u16] = &[0x1F0, 0x1F8];

macro_rules! ucb {
    ($name:literal, $o:literal, $c:expr, $class:ident, $confirm:expr, $uc:expr, $bits:expr, $prodis:expr, $access:expr, $about:literal) => {
        UcbDef { name: $name, orig: $o, copy: $c, class: Class::$class, confirm: $confirm, uc: $uc, conf_bits: $bits, prodis: $prodis, access: $access, about: $about }
    };
}

const W: Access = Access::Writable;

/// All UCBs of the TC3xx, in the order the views list them.
pub static UCBS: &[UcbDef] = &[
    ucb!("BMHD0", 0, Some(8), Boot, C1, Some(0x00), Some((0, 0, Some(16))), Some(PRODISBMHD), W,
        "Where and how the device starts. The SSW uses BMHD0..3 in order and takes the first valid one. Its password protects all four."),
    ucb!("BMHD1", 1, Some(9), Boot, C1, None, Some((0, 2, Some(18))), Some(PRODISBMHD), W, "Second boot mode header (protected by BMHD0's password)."),
    ucb!("BMHD2", 2, Some(10), Boot, C1, None, Some((0, 4, Some(20))), Some(PRODISBMHD), W, "Third boot mode header."),
    ucb!("BMHD3", 3, Some(11), Boot, C1, None, Some((0, 6, Some(22))), Some(PRODISBMHD), W, "Fourth boot mode header."),
    ucb!("DBG", 18, Some(26), Protection, C1, Some(0x12), Some((1, 4, Some(20))), Some(PRODISDBG), W,
        "Locks the debug interface. The SSW unlocks it at reset when the debugger sends this password (adapter debug_password)."),
    ucb!("PFLASH", 16, Some(24), Protection, C1, Some(0x10), Some((1, 0, Some(16))), Some(PRODISP), W,
        "Write protection per PFLASH logical sector and read protection of all PFLASH."),
    ucb!("DFLASH", 17, Some(25), Protection, C1, Some(0x11), Some((1, 2, Some(18))), Some(PRODISD), W,
        "DFLASH protection, DF0 read mode, oscillator and RAM initialization done by the SSW."),
    ucb!("ECPRIO", 22, Some(30), Protection, C1, Some(0x16), Some((1, 12, Some(28))), Some(PRODISEC), W, "High-priority PFLASH sectors for the erase counter."),
    ucb!("SWAP", 23, Some(31), Protection, C1, Some(0x17), Some((1, 14, Some(30))), Some(PRODISSWAP), W,
        "Bank swap log for SOTA. Only used when SOTA is enabled in UCB_OTP (SWAPEN). Entries are appended."),
    ucb!("HSM", 19, Some(27), Hsm, C1, None, Some((1, 6, Some(22))), None, W, "HSM debug, test and trace settings. Only used when the HSM boots."),
    ucb!("HSMCOTP0", 20, Some(28), Hsm, C1, None, Some((1, 8, Some(24))), None, W,
        "HSM boot, HSM-exclusive and one-time-programmable HSM sectors. Erasable only while both HSMCOTP sets are UNLOCKED; once one is CONFIRMED this is permanent."),
    ucb!("HSMCOTP1", 21, Some(29), Hsm, C1, None, Some((1, 10, Some(26))), None, W,
        "Second HSMCOTP set, OR-ed onto HSMCOTP0. Once HSMCOTP0 is CONFIRMED only the HSM can program it."),
    ucb!("HSMCFG", 7, None, Hsm, C2, None, Some((0, 14, None)), None, W,
        "HSM keys and configuration written by Infineon's HSM DLT. Readable and writable only while UNLOCKED; all zero if the HSM is not configured."),
    ucb!("OTP0", 32, Some(40), Otp, C1, None, Some((2, 0, Some(16))), None, W,
        "OTP/WOP set 0 and the tuning, SOTA and boot mode lock settings. Permanent: once any OTP UCB is CONFIRMED or ERRORED, none can be erased again."),
    ucb!("OTP1", 33, Some(41), Otp, C1, None, Some((2, 2, Some(18))), None, W, "OTP/WOP set 1, OR-ed onto the sets before it. Permanent."),
    ucb!("OTP2", 34, Some(42), Otp, C1, None, Some((2, 4, Some(20))), None, W, "OTP/WOP set 2, OR-ed onto the sets before it. Permanent."),
    ucb!("OTP3", 35, Some(43), Otp, C1, None, Some((2, 6, Some(22))), None, W, "OTP/WOP set 3, OR-ed onto the sets before it. Permanent."),
    ucb!("OTP4", 36, Some(44), Otp, C1, None, Some((2, 8, Some(24))), None, W, "OTP/WOP set 4, OR-ed onto the sets before it. Permanent."),
    ucb!("OTP5", 37, Some(45), Otp, C1, None, Some((2, 10, Some(26))), None, W, "OTP/WOP set 5, OR-ed onto the sets before it. Permanent."),
    ucb!("OTP6", 38, Some(46), Otp, C1, None, Some((2, 12, Some(28))), None, W, "OTP/WOP set 6, OR-ed onto the sets before it. Permanent."),
    ucb!("OTP7", 39, Some(47), Otp, C1, None, Some((2, 14, Some(30))), None, W, "OTP/WOP set 7, OR-ed onto the sets before it. Permanent."),
    ucb!("SSW", 4, None, Infineon, C2, None, Some((0, 8, None)), None, Access::ReadOnly("UCB_SSW is read only (UM 6.5.4.3.3)"),
        "Written by Infineon in production. Only the LBIST words are published."),
    ucb!("USER", 5, None, Infineon, C2, None, Some((0, 10, None)), None, Access::ReadOnly("UCB_USER is read only (UM 6.5.4.3.4)"),
        "Unique chip ID, bill of materials and ADC trimming values from Infineon. Compared with this chip's first readout."),
    ucb!("TEST", 6, None, Infineon, C2, None, Some((0, 12, None)), None, Access::ReadOnly("UCB_TEST is read only (UM 6.5.4.3.5)"),
        "Infineon test data; offset 0x180 holds the test pass marker 0x80658383."),
    ucb!("REDSEC", 12, None, Infineon, C2, None, Some((0, 24, None)), None,
        Access::ReadOnly("UCB_REDSEC is read only (UM 6.5.4.3.7); free entries are taken with the DMU command Replace Logical Sector"),
        "Flash redundancy. Entries Infineon used in production are marked USED, unusable ones FAILED."),
    ucb!("RETEST", 15, None, Infineon, C2, None, Some((0, 30, None)), None, Access::ReadOnly("UCB_RETEST is reserved by Infineon and not usable by the customer (UM 6.8.2.5)"),
        "Reserved by Infineon (secure retest)."),
    ucb!("SPARE13", 13, None, Spare, &[], None, None, None, Access::ReadOnly("spare UCBs are read only (UM 6.5.4.3.16)"), "Not used."),
    ucb!("SPARE14", 14, None, Spare, &[], None, None, None, Access::ReadOnly("spare UCBs are read only (UM 6.5.4.3.16)"), "Not used."),
];

/// A UCB definition by name (`BMHD0`, `UCB_BMHD0`, case-insensitive) or by one of its indices.
pub fn find(name: &str) -> Option<usize> {
    let n = name.trim();
    let n = n.strip_prefix("UCB_").or_else(|| n.strip_prefix("ucb_")).unwrap_or(n);
    if let Ok(i) = n.parse::<u8>() {
        return UCBS.iter().position(|d| d.indices().any(|x| x == i));
    }
    UCBS.iter().position(|d| d.name.eq_ignore_ascii_case(n))
}

/// The definition a UCB index belongs to.
pub fn def_of_index(index: u8) -> Option<usize> {
    UCBS.iter().position(|d| d.indices().any(|x| x == index))
}

// --- Device ------------------------------------------------------------------------------------

/// What differs between the TC3xx derivatives as far as the UCBs are concerned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub chip_id: u32,
    /// `SCU_CHIPID.CHID`: 3 = TC33x ... 9 = TC39x, 0xE = TC3Ex.
    pub variant: u32,
    pub name: String,
    /// Logical sectors (16 KB) of each PFLASH bank, PF0 first.
    pub pf_sectors: Vec<u32>,
    /// TriCore CPUs.
    pub cores: u32,
}

impl Device {
    /// From `SCU_CHIPID`. `cfg_banks` are the PFLASH banks of the cfg (base, size), used for
    /// variants without an entry in the flash driver's table; `cfg_cores` the TriCore targets of
    /// the cfg (0: unknown).
    pub fn from_chip_id(chip_id: u32, cfg_banks: &[(u32, u32)], cfg_cores: u32) -> Result<Device> {
        if chip_id & 0xC0 != 0x80 {
            return Err(other(format!("SCU_CHIPID 0x{chip_id:08x} is not an AURIX TC3xx device: UCBs are only supported on the TC3xx")));
        }
        let variant = (chip_id >> 12) & 0xF;
        let table = crate::flash::pflash_banks(variant);
        let mut pf_sectors: Vec<u32> = table.iter().map(|b| b.size / crate::flash::SECTOR_SIZE).collect();
        if pf_sectors.is_empty() {
            let mut banks: Vec<(u32, u32)> = cfg_banks.iter().filter(|(b, _)| matches!(b >> 28, 0x8 | 0xA) && b & 0x0F00_0000 == 0).copied().collect();
            banks.sort_by_key(|(b, _)| b & 0x0FFF_FFFF);
            pf_sectors = banks.iter().map(|(_, s)| (s / crate::flash::SECTOR_SIZE).min(192)).collect();
        }
        if pf_sectors.is_empty() {
            pf_sectors = vec![192];
        }
        let cores = if cfg_cores > 0 {
            cfg_cores
        } else {
            match variant {
                0x3 => 1,
                0x6 => 2,
                0x7 => 3,
                0x8 => 4,
                0x9 => 6,
                _ => 6,
            }
        };
        Ok(Device { chip_id, variant, name: format!("TC3{variant:X}x"), pf_sectors, cores })
    }
}

// --- Fields --------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Bool,
    /// Values with a meaning; `strict`: other values are invalid rather than undescribed.
    Enum(&'static [(u32, &'static str)], bool),
    Num,
    /// One bit per logical sector, starting at sector `first`.
    Sectors { prefix: &'static str, first: u32 },
    /// Computed by the tool when the UCB is written, not edited.
    Auto(&'static str),
    /// A confirmation code.
    Confirm,
}

#[derive(Clone, Debug)]
pub struct Field {
    pub name: String,
    pub label: String,
    pub msb: u8,
    pub lsb: u8,
    pub kind: Kind,
}

impl Field {
    pub fn width(&self) -> u32 {
        (self.msb - self.lsb + 1) as u32
    }
    pub fn mask(&self) -> u32 {
        let w = self.width();
        (if w == 32 { u32::MAX } else { (1u32 << w) - 1 }) << self.lsb
    }
    pub fn get(&self, word: u32) -> u32 {
        (word & self.mask()) >> self.lsb
    }
    pub fn put(&self, word: u32, v: u32) -> u32 {
        (word & !self.mask()) | ((v << self.lsb) & self.mask())
    }
    pub fn max(&self) -> u32 {
        self.mask() >> self.lsb
    }

    /// The value as the views show it.
    pub fn text(&self, v: u32) -> String {
        match &self.kind {
            Kind::Bool => if v != 0 { "yes" } else { "no" }.into(),
            Kind::Enum(e, _) => e.iter().find(|(x, _)| *x == v).map(|(_, t)| t.to_string()).unwrap_or_else(|| hex(v, self.width())),
            Kind::Sectors { prefix, first } => sectors_text(v, self.width(), prefix, *first),
            Kind::Confirm => conf_text(v).to_string(),
            Kind::Num | Kind::Auto(_) => hex(v, self.width()),
        }
    }

    /// Parses a value: a number, `yes`/`no`, an enum label or a sector list.
    pub fn parse(&self, s: &str) -> std::result::Result<u32, String> {
        let s = s.trim();
        let v = match &self.kind {
            Kind::Bool => match s.to_ascii_lowercase().as_str() {
                "yes" | "true" | "on" | "1" => 1,
                "no" | "false" | "off" | "0" => 0,
                _ => return Err(format!("{}: '{s}' is not yes/no", self.name)),
            },
            Kind::Enum(e, _) => match e.iter().find(|(_, t)| t.eq_ignore_ascii_case(s)) {
                Some((v, _)) => *v,
                None => crate::util::parse_u32(s).map_err(|_| format!("{}: '{s}' is neither a number nor one of: {}", self.name, enum_list(e)))?,
            },
            Kind::Confirm => match s.to_ascii_uppercase().as_str() {
                "UNLOCKED" => UNLOCKED,
                "CONFIRMED" => CONFIRMED,
                _ => crate::util::parse_u32(s)?,
            },
            Kind::Sectors { prefix, first } => parse_sectors(s, self.width(), prefix, *first)?,
            Kind::Num | Kind::Auto(_) => crate::util::parse_u32(s)?,
        };
        if v > self.max() {
            return Err(format!("{}: 0x{v:x} does not fit in {} bits", self.name, self.width()));
        }
        Ok(v)
    }

    pub fn to_json(&self) -> Value {
        let (kind, extra) = match &self.kind {
            Kind::Bool => ("bool", json!({})),
            Kind::Enum(e, strict) => ("enum", json!({ "options": e.iter().map(|(v, t)| json!([v, t])).collect::<Vec<_>>(), "strict": strict })),
            Kind::Num => ("num", json!({})),
            Kind::Sectors { prefix, first } => ("sectors", json!({ "prefix": prefix, "first": first })),
            Kind::Auto(why) => ("auto", json!({ "auto": why })),
            Kind::Confirm => ("confirm", json!({ "options": [[UNLOCKED, "UNLOCKED"], [CONFIRMED, "CONFIRMED"]] })),
        };
        let mut o = json!({ "name": self.name, "label": self.label, "msb": self.msb, "lsb": self.lsb, "kind": kind });
        if let (Some(o), Some(e)) = (o.as_object_mut(), extra.as_object()) {
            o.extend(e.clone());
        }
        o
    }
}

fn enum_list(e: &[(u32, &str)]) -> String {
    e.iter().map(|(_, t)| *t).collect::<Vec<_>>().join(", ")
}

pub fn hex(v: u32, bits: u32) -> String {
    format!("0x{:0w$X}", v, w = bits.div_ceil(4).max(1) as usize)
}

pub fn conf_text(v: u32) -> &'static str {
    match v {
        UNLOCKED => "UNLOCKED",
        CONFIRMED => "CONFIRMED",
        0 => "ERASED",
        _ => "ERRORED",
    }
}

/// `S0-S3, S10` for the set bits of a sector bitmap; `none` when empty.
pub fn sectors_text(v: u32, width: u32, prefix: &str, first: u32) -> String {
    let set: Vec<u32> = (0..width).filter(|b| (v >> b) & 1 != 0).map(|b| first + b).collect();
    if set.is_empty() {
        return "none".into();
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < set.len() {
        let mut j = i;
        while j + 1 < set.len() && set[j + 1] == set[j] + 1 {
            j += 1;
        }
        out.push(if i == j { format!("{prefix}{}", set[i]) } else { format!("{prefix}{}-{prefix}{}", set[i], set[j]) });
        i = j + 1;
    }
    out.join(", ")
}

pub fn parse_sectors(s: &str, width: u32, prefix: &str, first: u32) -> std::result::Result<u32, String> {
    let s = s.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("none") {
        return Ok(0);
    }
    let strip = |t: &str| -> std::result::Result<u32, String> {
        let t = t.trim();
        let t = if t.len() >= prefix.len() && t[..prefix.len()].eq_ignore_ascii_case(prefix) { &t[prefix.len()..] } else { t };
        t.parse::<u32>().map_err(|_| format!("'{t}' is not a sector number"))
    };
    let mut v = 0u32;
    for part in s.split(',') {
        let part = part.trim().replace('–', "-");
        let (a, b) = match part.split_once('-') {
            Some((a, b)) => (strip(a)?, strip(b)?),
            None => {
                let a = strip(&part)?;
                (a, a)
            }
        };
        if a > b {
            return Err(format!("'{part}': the range is reversed"));
        }
        for n in a..=b {
            if n < first || n >= first + width {
                return Err(format!("{prefix}{n} is not in this word ({prefix}{first}-{prefix}{})", first + width - 1));
            }
            v |= 1 << (n - first);
        }
    }
    Ok(v)
}

/// One 32-bit word of a UCB.
#[derive(Clone, Debug)]
pub struct Word {
    pub off: u16,
    pub name: String,
    pub fields: Vec<Field>,
    /// The `DMU` register the word is installed into at startup (compared in the views).
    pub installed: Option<u32>,
    /// Shown masked unless revealed (passwords, HSM keys).
    pub secret: bool,
    /// Content not published: shown as a raw word.
    pub raw: bool,
}

impl Word {
    /// Bits covered by fields; the others are reserved and must stay 0.
    pub fn defined(&self) -> u32 {
        self.fields.iter().fold(0, |m, f| m | f.mask())
    }
}

fn f(name: &str, label: &str, msb: u8, lsb: u8, kind: Kind) -> Field {
    Field { name: name.into(), label: label.into(), msb, lsb, kind }
}
fn b(name: &str, label: &str, bit: u8) -> Field {
    f(name, label, bit, bit, Kind::Bool)
}
fn n32(name: &str, label: &str) -> Field {
    f(name, label, 31, 0, Kind::Num)
}
fn word(off: u16, name: impl Into<String>, fields: Vec<Field>) -> Word {
    Word { off, name: name.into(), fields, installed: None, secret: false, raw: false }
}
fn installed(mut w: Word, reg: u32) -> Word {
    w.installed = Some(reg);
    w
}

const HWCFG: &[(u32, &str)] = &[(7, "Internal flash"), (6, "Alternate boot mode"), (4, "Generic bootstrap loader"), (3, "ASC bootstrap loader")];
const PINDIS: &[(u32, &str)] = &[(0, "allowed"), (1, "disabled")];
const CHSWENA: &[(u32, &str)] = &[(0, "runs"), (5, "skipped")];
const EDM: &[(u32, &str)] = &[(0, "no"), (3, "yes (CAN/FlexRay off)")];
const TIC: &[(u32, &str)] = &[(0, "off unless tool enabled it"), (1, "standard CAN pins"), (2, "alternative CAN pins")];
const DFMODE: &[(u32, &str)] = &[(0, "single ended"), (1, "complement sensing")];
const OSCMODE: &[(u32, &str)] = &[(0, "external crystal"), (2, "external input"), (3, "disabled")];
const RAMIN: &[(u32, &str)] =
    &[(0, "after cold power-on reset"), (1, "after warm power-on reset"), (2, "after cold power-on reset"), (3, "never")];
const TRDIS: &[(u32, &str)] = &[(0, "enabled"), (1, "disabled"), (3, "disabled")];
const TRTYPE: &[(u32, &str)] = &[(0, "addresses and data"), (1, "addresses only")];
const RAMKEEP: &[(u32, &str)] = &[
    (0, "after every power-on and system reset"),
    (1, "after power-on resets"),
    (2, "after power-on resets"),
    (3, "only after cold power-on"),
];
const YES3: &[(u32, &str)] = &[(0, "no"), (3, "yes")];
const BML: &[(u32, &str)] = &[(0, "standard boot flow"), (1, "restricted boot flow"), (3, "restricted boot flow")];
const SWAPEN: &[(u32, &str)] = &[(0, "disabled"), (2, "disabled"), (3, "enabled")];
const MARKER: &[(u32, &str)] = &[(0, "-"), (0x55, "standard map (A)"), (0xAA, "alternate map (B)")];
const SWAPCONF: &[(u32, &str)] = &[(0, "-"), (CONFIRMED, "CONFIRMED"), (0xFFFF_FFFF, "invalidated")];

fn confirm_words(def: &UcbDef) -> Vec<Word> {
    def.confirm
        .iter()
        .map(|&off| {
            let name = match (def.confirm.len(), off) {
                (2, 0x1F0) => "CONFIRMATION_ORIG",
                (2, _) => "CONFIRMATION_COPY",
                _ => "CONFIRMATION",
            };
            word(off, name, vec![f("CODE", "State", 31, 0, Kind::Confirm)])
        })
        .collect()
}

fn password_words() -> Vec<Word> {
    (0..8)
        .map(|i| {
            let mut w = word(PW + 4 * i as u16, format!("PW{i}"), vec![n32(&format!("PW{i}"), &format!("Password word {i}"))]);
            w.secret = true;
            w
        })
        .collect()
}

/// Sector bitmap words of all PFLASH banks at `base + p * 0x20 + s * 4`.
fn bank_words(dev: &Device, base: u16, prefix: &str, what: &str) -> Vec<Word> {
    let mut out = Vec::new();
    for (p, &sectors) in dev.pf_sectors.iter().enumerate() {
        for s in 0..6u32 {
            let have = sectors.saturating_sub(s * 32).min(32);
            if have == 0 {
                continue;
            }
            let fld = f("S", &format!("PF{p} {what}"), (have - 1) as u8, 0, Kind::Sectors { prefix: "S", first: s * 32 });
            out.push(word(base + (p as u16) * 0x20 + (s as u16) * 4, format!("{prefix}{p}{s}"), vec![fld]));
        }
    }
    out
}

fn bmi_fields(dev: &Device) -> Vec<Field> {
    let mut v = vec![
        f("HWCFG", "Start-up mode", 3, 1, Kind::Enum(HWCFG, true)),
        f("PINDIS", "Boot mode from HWCFG pins", 0, 0, Kind::Enum(PINDIS, true)),
    ];
    for c in 0..dev.cores.min(4) {
        v.push(b(&format!("LSENA{c}"), &format!("Lockstep CPU{c}"), 4 + c as u8));
    }
    v.push(b("LBISTENA", "LBIST at cold power-on", 8));
    v.push(f("CHSWENA", "Checker software", 11, 9, Kind::Enum(CHSWENA, false)));
    v.push(f("BMHDID", "Header ID", 31, 16, Kind::Auto("always 0xB359")));
    v
}

fn hsmcotp_words() -> Vec<Word> {
    let bootsel = |i: u8| f(&format!("BOOTSEL{i}"), &format!("HSM boot sector {i} (HSMnX = PF0 Sn)"), 5 + 8 * i, 8 * i, Kind::Num);
    vec![
        word(0x000, "PROCONUSR", vec![f("MODE", "DF1 read mode", 1, 0, Kind::Enum(DFMODE, false))]),
        word(0x004, "PROCONHSMCBS", (0..4).map(bootsel).collect()),
        word(0x008, "PROCONHSMCX0", vec![f("S", "HSM-exclusive sectors", 31, 0, Kind::Sectors { prefix: "HSM", first: 0 })]),
        word(0x00C, "PROCONHSMCX1", vec![f("S", "HSM-exclusive sectors", 7, 0, Kind::Sectors { prefix: "HSM", first: 32 })]),
        word(0x010, "PROCONHSMCOTP0", vec![f("S", "HSM one-time-programmable sectors", 31, 0, Kind::Sectors { prefix: "HSM", first: 0 })]),
        word(0x014, "PROCONHSMCOTP1", vec![f("S", "HSM one-time-programmable sectors", 7, 0, Kind::Sectors { prefix: "HSM", first: 32 })]),
        word(
            0x018,
            "PROCONHSMCFG",
            vec![
                b("HSMBOOTEN", "HSM boots", 0),
                b("SSWWAIT", "CPU0 waits for the HSM", 1),
                b("HSMDX", "DFLASH1 HSM-exclusive", 2),
                f("HSMRAMKEEP", "HSM RAM cleared", 5, 4, Kind::Enum(RAMKEEP, true)),
                f("HSMENPINS", "HSM may force pins HSM1/2", 8, 7, Kind::Enum(YES3, false)),
                f("HSMENRES", "HSM may trigger resets", 10, 9, Kind::Enum(YES3, false)),
                f("DESTDBG", "Debug entry destructive", 12, 11, Kind::Enum(YES3, false)),
                b("BLKFLAN", "Block flash analysis on HSM ranges", 13),
            ],
        ),
    ]
}

fn otp_words(dev: &Device) -> Vec<Word> {
    let mut v = bank_words(dev, 0x000, "PROCONOTP", "one-time-programmable sectors");
    v.extend(bank_words(dev, 0x100, "PROCONWOP", "write-once sectors"));
    let mut tp = vec![
        b("TP", "Tuning protection", 0),
        f("BML", "Boot mode lock", 9, 8, Kind::Enum(BML, false)),
        f("SWAPEN", "SOTA (bank swap)", 17, 16, Kind::Enum(SWAPEN, false)),
    ];
    for c in 0..dev.cores.min(6) {
        tp.push(b(&format!("CPU{c}DDIS"), &format!("CPU{c} direct PFLASH access disabled"), 18 + c as u8));
    }
    v.push(word(0x1E8, "PROCONTP", tp));
    v
}

fn redsec_words(dev: &Device) -> Vec<Word> {
    let mut v = Vec::new();
    let entry = |v: &mut Vec<Word>, name: String, off: u16, addr_bits: u8| {
        v.push(word(off, format!("{name}L"), vec![b("FAIL", "Failed", 15), b("USED", "Used", 14), f("ADDR", "Sector", addr_bits - 1, 0, Kind::Num)]));
        v.push(word(off + 4, format!("{name}H"), vec![f("FLAGS", "Flags", 31, 24, Kind::Num), f("ID", "ID", 23, 16, Kind::Num), f("MISR", "MISR", 15, 8, Kind::Num)]));
    };
    // Six PFLASH bank slots on every device; Infineon marks the entries of missing banks FAILED.
    let _ = dev;
    for p in 0..6u16 {
        for s in 0..8u16 {
            entry(&mut v, format!("PF{p}_{s}"), p * 0x40 + s * 8, 6);
        }
    }
    for s in 0..8u16 {
        entry(&mut v, format!("DF0_{s}"), 0x180 + s * 8, 8);
    }
    for s in 0..6u16 {
        entry(&mut v, format!("DF1_{s}"), 0x1C0 + s * 8, 5);
    }
    v
}

/// Raw words for every offset `words` does not cover (unpublished content).
fn fill_raw(mut words: Vec<Word>, secret: bool) -> Vec<Word> {
    let have: std::collections::BTreeSet<u16> = words.iter().map(|w| w.off).collect();
    for off in (0..SIZE as u16).step_by(4) {
        if !have.contains(&off) {
            let mut w = word(off, format!("+0x{off:03X}"), vec![n32("RAW", "raw")]);
            w.raw = true;
            w.secret = secret;
            words.push(w);
        }
    }
    words.sort_by_key(|w| w.off);
    words
}

/// The words of a UCB definition on a device, in offset order.
pub fn words(def: &UcbDef, dev: &Device) -> Vec<Word> {
    let mut v: Vec<Word> = match def.name {
        n if n.starts_with("BMHD") => {
            let mut v = vec![
                word(0x000, "BMI_BMHDID", bmi_fields(dev)),
                word(0x004, "STAD", vec![n32("STAD", "Start address")]),
                word(0x008, "CRCBMHD", vec![f("CRC", "Checksum", 31, 0, Kind::Auto("computed on write"))]),
                word(0x00C, "CRCBMHD_N", vec![f("CRCN", "Inverted checksum", 31, 0, Kind::Auto("computed on write"))]),
            ];
            if def.uc.is_some() {
                v.extend(password_words());
            }
            v
        }
        "DBG" => {
            let mut v = vec![installed(
                word(
                    0x000,
                    "PROCONDBG",
                    vec![
                        b("OCDSDIS", "OCDS disabled", 0),
                        b("DBGIFLCK", "Debug interface locked", 1),
                        f("EDM", "Destructive debug entered", 3, 2, Kind::Enum(EDM, false)),
                        f("TIC", "DAP over CAN (DXCPL)", 11, 8, Kind::Enum(TIC, false)),
                    ],
                ),
                HF_PROCONDBG,
            )];
            v.extend(password_words());
            v
        }
        "PFLASH" => {
            let mut v = bank_words(dev, 0x000, "PROCONP", "write-protected sectors");
            v.push(installed(word(0x0C0, "PROCONPF", vec![b("RPRO", "PFLASH read protection", 31)]), HF_PROCONPF));
            v.extend(password_words());
            v
        }
        "DFLASH" => {
            let mut v = vec![
                installed(word(0x000, "PROCONUSR", vec![f("MODE", "DF0 read mode", 1, 0, Kind::Enum(DFMODE, false))]), HF_PROCONUSR),
                installed(
                    word(
                        0x004,
                        "PROCONDF",
                        vec![
                            b("L", "DF0 EEPROM write locked", 0),
                            b("HYSEN", "Oscillator hysteresis", 3),
                            f("HYSCTL", "Hysteresis control", 5, 4, Kind::Num),
                            f("AMPCTL", "Amplitude control", 7, 6, Kind::Num),
                            b("OSCCFG", "Oscillator set up by SSW", 8),
                            f("MODE", "Oscillator mode", 10, 9, Kind::Enum(OSCMODE, false)),
                            b("APREN", "Amplitude regulation", 11),
                            b("CAP0EN", "Load capacitance 0", 12),
                            b("CAP1EN", "Load capacitance 1", 13),
                            b("CAP2EN", "Load capacitance 2", 14),
                            b("CAP3EN", "Load capacitance 3", 15),
                            f("ESR0CNT", "ESR0 prolongation", 27, 16, Kind::Num),
                            b("RPRO", "DFLASH read protection", 31),
                        ],
                    ),
                    HF_PROCONDF,
                ),
                installed(
                    word(
                        0x008,
                        "PROCONRAM",
                        vec![
                            f("RAMIN", "RAM init by SSW", 1, 0, Kind::Enum(RAMIN, true)),
                            f("RAMINSEL", "RAMs selected", 21, 16, Kind::Num),
                            f("LMUINSEL", "LMU RAMs selected", 30, 24, Kind::Num),
                        ],
                    ),
                    HF_PROCONRAM,
                ),
            ];
            v.extend(password_words());
            v
        }
        "ECPRIO" => {
            let mut v = bank_words(dev, 0x000, "ECPRIO", "high-priority sectors");
            v.extend(password_words());
            v
        }
        "SWAP" => {
            let mut v = Vec::new();
            for x in 0..16u16 {
                let o = x * 0x10;
                v.push(word(o, format!("MARKERL{x}"), vec![f("SWAP", "Marker", 31, 0, Kind::Enum(MARKER, false))]));
                v.push(word(o + 4, format!("MARKERH{x}"), vec![n32("ADDR", "Marker address")]));
                v.push(word(o + 8, format!("CONFIRMATIONL{x}"), vec![f("CODE", "Entry confirmation", 31, 0, Kind::Enum(SWAPCONF, false))]));
                v.push(word(o + 12, format!("CONFIRMATIONH{x}"), vec![n32("ADDR", "Confirmation address")]));
            }
            v.extend(password_words());
            v
        }
        "HSM" => vec![installed(
            word(
                0x000,
                "PROCONHSM",
                vec![
                    b("HSMDBGDIS", "HSM debug disabled", 0),
                    b("DBGIFLCK", "HSM keeps the debug interface locked", 1),
                    b("TSTIFLCK", "Test interface locked", 2),
                    b("HSMTSTDIS", "HSM test disabled", 3),
                    f("HSMTRDIS", "HSM trace", 5, 4, Kind::Enum(TRDIS, false)),
                    f("HSMTRTYPE", "HSM trace type", 6, 6, Kind::Enum(TRTYPE, true)),
                ],
            ),
            SP_PROCONHSM,
        )],
        "HSMCOTP0" | "HSMCOTP1" => hsmcotp_words(),
        n if n.starts_with("OTP") => otp_words(dev),
        "REDSEC" => redsec_words(dev),
        "SSW" => fill_raw(
            vec![
                word(0x0E8, "SCU_LBISTCTRL3_BODY0", vec![n32("SIG", "LBIST signature (BODY=0)")]),
                word(0x0EC, "SCU_LBISTCTRL3_BODY1", vec![n32("SIG", "LBIST signature (BODY=1)")]),
                word(0x160, "SCU_LBISTCTRL0", vec![n32("V", "LBIST configuration A, word 0")]),
                word(0x164, "SCU_LBISTCTRL1", vec![n32("V", "LBIST configuration A, word 1")]),
                word(0x168, "SCU_LBISTCTRL2", vec![n32("V", "LBIST configuration A, word 2")]),
            ]
            .into_iter()
            .chain(confirm_words(def))
            .collect(),
            false,
        ),
        "USER" => {
            let mut v: Vec<Word> = (0..4).map(|i| word(4 * i, format!("UID{i}"), vec![n32("V", &format!("Unique chip ID, word {i}"))])).collect();
            v.push(word(0x010, "BOM", vec![n32("V", "Mold, bond wire and ball material")]));
            for g in 0..12u16 {
                v.push(word(0x020 + g * 8, format!("EVADC_G{g:02}_VDDK"), vec![n32("V", &format!("EVADC group {g}: VDDK"))]));
                v.push(word(0x024 + g * 8, format!("EVADC_G{g:02}_RES"), vec![n32("V", &format!("EVADC group {g}: reserved"))]));
            }
            for c in 0..14u16 {
                v.push(word(0x080 + c * 8, format!("EDSADC_CH{c:02}_RES"), vec![n32("V", &format!("EDSADC channel {c}: reserved"))]));
                v.push(word(0x084 + c * 8, format!("EDSADC_CH{c:02}_IRMS"), vec![n32("V", &format!("EDSADC channel {c}: IRMS"))]));
            }
            fill_raw(v.into_iter().chain(confirm_words(def)).collect(), false)
        }
        "TEST" => fill_raw(
            std::iter::once(word(0x180, "TEST_PASS_MARKER", vec![n32("V", "Test pass marker (0x80658383)")])).chain(confirm_words(def)).collect(),
            false,
        ),
        "HSMCFG" => fill_raw(confirm_words(def), true),
        _ => fill_raw(confirm_words(def), false),
    };
    if !v.iter().any(|w| w.fields.iter().any(|f| f.kind == Kind::Confirm)) {
        v.extend(confirm_words(def));
    }
    v.sort_by_key(|w| w.off);
    v
}

/// Whether the words describe the whole UCB (unpublished content is listed raw), so that offsets
/// without a word are reserved and must be 0.
pub fn reserved_offsets(words: &[Word]) -> Vec<u16> {
    let have: std::collections::BTreeSet<u16> = words.iter().map(|w| w.off).collect();
    (0..SIZE as u16).step_by(4).filter(|o| !have.contains(o)).collect()
}

// --- Content -------------------------------------------------------------------------------------

pub type Content = [u8; SIZE];

pub fn get32(c: &Content, off: u16) -> u32 {
    let o = off as usize;
    u32::from_le_bytes([c[o], c[o + 1], c[o + 2], c[o + 3]])
}

pub fn put32(c: &mut Content, off: u16, v: u32) {
    let o = off as usize;
    c[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

/// Confirmation state of one code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Unlocked,
    Confirmed,
    Errored,
    /// Not known (not read, or the DMU reports UNREAD).
    Unknown,
}

impl State {
    pub fn of_code(code: u32) -> State {
        match code {
            UNLOCKED => State::Unlocked,
            CONFIRMED => State::Confirmed,
            _ => State::Errored,
        }
    }
    /// A `DMU_HF_CONFIRMx` field.
    pub fn of_field(v: u32) -> State {
        match v & 3 {
            1 => State::Unlocked,
            2 => State::Confirmed,
            3 => State::Errored,
            _ => State::Unknown,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            State::Unlocked => "UNLOCKED",
            State::Confirmed => "CONFIRMED",
            State::Errored => "ERRORED",
            State::Unknown => "UNKNOWN",
        }
    }
    pub fn parse(s: &str) -> Option<State> {
        match s.to_ascii_lowercase().as_str() {
            "unlocked" | "unconfirmed" => Some(State::Unlocked),
            "confirmed" => Some(State::Confirmed),
            "errored" | "erased" => Some(State::Errored),
            _ => None,
        }
    }
    /// The state of an ORIG/COPY pair (UM 6.5.4.3.1): ORIG unless it is ERRORED.
    pub fn pair(orig: State, copy: Option<State>) -> State {
        match (orig, copy) {
            (State::Errored, Some(c)) => c,
            (o, _) => o,
        }
    }
}

/// The content of UCB `index` of a definition whose ORIG holds `orig`. Equal to `orig`, except
/// for UCB_SWAP: its entries hold system addresses of their own words (MARKERHx = address of
/// MARKERLx, CONFIRMATIONHx = address of CONFIRMATIONLx), so in COPY they point into COPY.
pub fn content_for(def: &UcbDef, index: u8, orig: &Content) -> Content {
    relocate_swap(def, orig, def.orig, index)
}

/// UCB_SWAP content with the entry addresses moved from UCB `from` to UCB `to`.
pub fn relocate_swap(def: &UcbDef, c: &Content, from: u8, to: u8) -> Content {
    let mut c = *c;
    if def.name == "SWAP" && from != to {
        let (a, b) = (ucb_addr(from), ucb_addr(to));
        for x in 0..16u16 {
            for off in [x * 0x10 + 4, x * 0x10 + 12] {
                let v = get32(&c, off);
                if (a..a + SIZE as u32).contains(&v) {
                    put32(&mut c, off, v - a + b);
                }
            }
        }
    }
    c
}

/// The delivery default of a UCB: confirmation codes UNLOCKED, everything else erased (UM, UCB
/// chapter, "Delivery State"). `None` for Infineon data, which has no general default.
pub fn delivery_default(def: &UcbDef) -> Option<Content> {
    match def.class {
        Class::Infineon => None,
        Class::Spare => Some([0; SIZE]),
        _ => {
            let mut c = [0u8; SIZE];
            for &off in def.confirm {
                put32(&mut c, off, UNLOCKED);
            }
            Some(c)
        }
    }
}

/// The CRC the SSW checks in a boot mode header: CRC-32 (IEEE 802.3, as the TriCore `CRC32`
/// instruction) over the words BMI_BMHDID and STAD, each taken most significant byte first.
pub fn bmhd_crc(bmi_bmhdid: u32, stad: u32) -> u32 {
    let mut crc = u32::MAX;
    for byte in bmi_bmhdid.to_be_bytes().into_iter().chain(stad.to_be_bytes()) {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// Whether a boot mode header is valid as the SSW checks it (ID, mode, CRC).
pub fn bmhd_valid(c: &Content) -> bool {
    let (bmi, stad) = (get32(c, 0), get32(c, 4));
    bmi >> 16 == 0xB359 && matches!((bmi >> 1) & 7, 7 | 6 | 4 | 3) && get32(c, 8) == bmhd_crc(bmi, stad) && get32(c, 12) == !bmhd_crc(bmi, stad)
}

// --- Readout -------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Readout {
    pub device: Device,
    /// Content of every UCB index (0 where unreadable).
    pub data: Vec<Content>,
    pub unreadable: Vec<[bool; WORDS]>,
    pub confirm_regs: [u32; 3],
    pub protect: u32,
    /// Installed protection registers (address, value) that could be read.
    pub installed: BTreeMap<u32, u32>,
    /// Seconds since the Unix epoch.
    pub time: u64,
}

impl Readout {
    pub fn word(&self, index: u8, off: u16) -> u32 {
        get32(&self.data[index as usize], off)
    }
    pub fn readable(&self, index: u8, off: u16) -> bool {
        !self.unreadable[index as usize][off as usize / 4]
    }
    pub fn fully_readable(&self, index: u8) -> bool {
        !self.unreadable[index as usize].iter().any(|&u| u)
    }
    /// The state `DMU_HF_CONFIRMx` reports for ORIG and COPY (installed at the last startup).
    pub fn installed_states(&self, def: &UcbDef) -> (State, Option<State>) {
        match def.conf_bits {
            Some((r, o, c)) => {
                let reg = self.confirm_regs[r];
                (State::of_field(reg >> o), c.map(|c| State::of_field(reg >> c)))
            }
            None => (State::Unknown, None),
        }
    }
    /// The state the codes in the content give, ORIG and COPY (first code of each UCB).
    pub fn content_states(&self, def: &UcbDef) -> (State, Option<State>) {
        let st = |i: u8| match def.confirm.first() {
            Some(&off) if self.readable(i, off) => State::of_code(self.word(i, off)),
            _ => State::Unknown,
        };
        (st(def.orig), def.copy.map(st))
    }
    /// The effective state: from the content, else from the DMU.
    pub fn state(&self, def: &UcbDef) -> State {
        let (o, c) = self.content_states(def);
        let s = State::pair(o, c);
        if s == State::Unknown {
            let (o, c) = self.installed_states(def);
            State::pair(o, c)
        } else {
            s
        }
    }
    pub fn prodis(&self, def: &UcbDef) -> bool {
        def.prodis.is_some_and(|b| self.protect >> b & 1 != 0)
    }
    /// The 128-bit unique chip ID from UCB_USER, as 32 hex digits.
    pub fn uid(&self) -> Option<String> {
        if !(0..16).step_by(4).all(|o| self.readable(5, o)) {
            return None;
        }
        let w: Vec<u32> = (0..4).map(|i| self.word(5, 4 * i)).collect();
        if w.iter().all(|&x| x == 0) {
            return None;
        }
        Some(w.iter().map(|x| format!("{x:08X}")).collect())
    }
    /// The whole readout as an image (readable words only).
    pub fn image(&self) -> Image {
        let mut sections = Vec::new();
        for (i, c) in self.data.iter().enumerate() {
            let mut start: Option<usize> = None;
            for w in 0..=WORDS {
                let ok = w < WORDS && !self.unreadable[i][w];
                match (ok, start) {
                    (true, None) => start = Some(w),
                    (false, Some(s)) => {
                        sections.push(Section { addr: ucb_addr(i as u8) + 4 * s as u32, data: c[4 * s..4 * w].to_vec() });
                        start = None;
                    }
                    _ => {}
                }
            }
        }
        Image::from_sections(sections)
    }
}

/// Which words of a UCB the DMU refuses to read, judged from the installed states and
/// `DMU_HF_PROTECT` (UM 6.5.4.3): (whole UCB unreadable, password words unreadable).
fn read_protection(def: &UcbDef, confirm_regs: &[u32; 3], protect: u32, installed: &BTreeMap<u32, u32>) -> (bool, bool) {
    let (o, c) = match def.conf_bits {
        Some((r, ob, cb)) => (State::of_field(confirm_regs[r] >> ob), cb.map(|cb| State::of_field(confirm_regs[r] >> cb))),
        None => return (false, false),
    };
    let state = State::pair(o, c);
    let disabled = def.prodis.is_some_and(|b| protect >> b & 1 != 0);
    let locked = (state == State::Confirmed && !disabled) || state == State::Errored;
    match def.name {
        "PFLASH" | "DFLASH" | "ECPRIO" => (locked, false),
        "DBG" => {
            let edm = installed.get(&HF_PROCONDBG).is_some_and(|v| (v >> 2) & 3 == 3);
            (locked || edm, false)
        }
        "BMHD0" | "SWAP" => (false, locked),
        "HSMCFG" => (o != State::Unlocked, false),
        // UM 6.5.2.2.4: UCB_RETEST analysis is blocked once it is CONFIRMED or ERRORED.
        "RETEST" => (matches!(o, State::Confirmed | State::Errored), false),
        _ => (false, false),
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Reads all UCBs and the DMU state.
pub fn read(io: &mut Io, device: Device) -> Result<Readout> {
    let confirm_regs = {
        let v = io.read32_many(&HF_CONFIRM)?;
        [v[0], v[1], v[2]]
    };
    let protect = io.read32(HF_PROTECT)?;
    let mut installed = BTreeMap::new();
    for &r in INSTALLED {
        if let Ok(v) = io.read32(r) {
            installed.insert(r, v);
        }
    }
    let mut data = vec![[0u8; SIZE]; COUNT];
    let mut unreadable = vec![[false; WORDS]; COUNT];
    for def in UCBS {
        let (whole, pw) = read_protection(def, &confirm_regs, protect, &installed);
        for idx in def.indices() {
            let skip = &mut unreadable[idx as usize];
            if whole {
                *skip = [true; WORDS];
            } else if pw && def.uc.is_some() {
                for i in 0..8 {
                    skip[PW as usize / 4 + i] = true;
                }
            }
        }
    }
    for idx in 0..COUNT {
        let base = ucb_addr(idx as u8);
        let mut w = 0;
        while w < WORDS {
            if unreadable[idx][w] {
                w += 1;
                continue;
            }
            let start = w;
            while w < WORDS && !unreadable[idx][w] {
                w += 1;
            }
            let (a, len) = (base + 4 * start as u32, 4 * (w - start));
            match io.read(a, len) {
                Ok(bytes) => data[idx][4 * start..4 * w].copy_from_slice(&bytes),
                Err(_) => {
                    // A protection the rules above did not foresee: word by word, one attempt each;
                    // four failures in a row mark the rest of the run unreadable (a protected
                    // UCB answers every word with a bus error).
                    let mut failures = 0;
                    for k in start..w {
                        if failures >= 4 {
                            unreadable[idx][k] = true;
                            continue;
                        }
                        match io.read32(base + 4 * k as u32) {
                            Ok(v) => {
                                data[idx][4 * k..4 * k + 4].copy_from_slice(&v.to_le_bytes());
                                failures = 0;
                            }
                            Err(_) => {
                                unreadable[idx][k] = true;
                                failures += 1;
                            }
                        }
                    }
                    crate::log_debug!("ucb: UCB{idx} not readable as one block; read word by word");
                }
            }
        }
    }
    Ok(Readout { device, data, unreadable, confirm_regs, protect, installed, time: now_secs() })
}

// --- Storage: baseline, backups, log ---------------------------------------------------------

/// The default directory for baselines, backups and the write log: `~/.aurix-dbg/ucb`.
pub fn default_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".aurix-dbg").join("ucb")
}

fn baseline_file(dir: &Path, uid: &str) -> PathBuf {
    dir.join("baseline").join(format!("{uid}.hex"))
}

/// The chip's first readout (Infineon data is compared with it); written on the first readout of
/// a chip and never overwritten.
pub fn baseline(dir: &Path, r: &Readout) -> Option<Image> {
    let uid = r.uid()?;
    let file = baseline_file(dir, &uid);
    if let Ok(text) = std::fs::read_to_string(&file) {
        return Image::from_ihex(&text).ok();
    }
    let img = r.image();
    if std::fs::create_dir_all(file.parent()?).is_ok() && std::fs::write(&file, img.to_ihex()).is_ok() {
        crate::log_info!("ucb: baseline of chip {uid} saved to {}", file.display());
    }
    Some(img)
}

/// The content of UCB `index` from an image, if the image covers all of it.
pub fn content_from(img: &Image, index: u8) -> Option<Content> {
    let base = ucb_addr(index);
    let mut c = [0u8; SIZE];
    for (k, byte) in c.iter_mut().enumerate() {
        *byte = img.byte_at(base + k as u32)?;
    }
    Some(c)
}

/// Writes a timestamped backup of the readout; returns its path.
pub fn backup(dir: &Path, r: &Readout) -> Result<PathBuf> {
    let d = dir.join("backup");
    std::fs::create_dir_all(&d).map_err(|e| other(format!("{}: {e}", d.display())))?;
    let file = d.join(format!("{}_{}_{}.hex", r.device.name, r.uid().unwrap_or_else(|| "nouid".into()), stamp(r.time)));
    std::fs::write(&file, r.image().to_ihex()).map_err(|e| other(format!("{}: {e}", file.display())))?;
    Ok(file)
}

/// `YYYYMMDD-HHMMSS` (UTC) of a Unix time.
pub fn stamp(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let (h, m, s) = (secs % 86400 / 3600, secs % 3600 / 60, secs % 60);
    // Civil from days (H. Hinnant).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if mo <= 2 { 1 } else { 0 };
    format!("{y:04}{mo:02}{d:02}-{h:02}{m:02}{s:02}")
}

// --- View ------------------------------------------------------------------------------------------

/// Edits that are not written yet: the new ORIG content per definition (COPY gets the same).
pub type Staged = BTreeMap<usize, Content>;

pub struct ViewOptions<'a> {
    pub reveal: bool,
    pub only: Option<usize>,
    pub baseline: Option<&'a Image>,
    pub write_enabled: bool,
    pub known_passwords: &'a [&'static str],
}

fn word_json(def: &UcbDef, w: &Word, r: &Readout, staged: Option<&Content>, default: Option<&Content>, reveal: bool) -> Value {
    let o = r.word(def.orig, w.off);
    let c = def.copy.map(|i| r.word(i, w.off));
    // What COPY should hold for this ORIG (UCB_SWAP's addresses differ by design).
    let want_c = def.copy.map(|i| get32(&content_for(def, i, &r.data[def.orig as usize]), w.off));
    let readable = r.readable(def.orig, w.off);
    let copy_readable = def.copy.is_none_or(|i| r.readable(i, w.off));
    let d = default.map(|d| get32(d, w.off));
    let s = staged.map(|s| get32(s, w.off)).filter(|&s| s != o);
    let hide = w.secret && !reveal;
    let fields: Vec<Value> = w
        .fields
        .iter()
        .map(|f| {
            let mut j = f.to_json();
            let m = j.as_object_mut().unwrap();
            if hide {
                m.insert("value".into(), Value::Null);
                m.insert("text".into(), json!(if !readable { "unreadable" } else if f.get(o) == 0 { "zero" } else { "set" }));
            } else {
                m.insert("value".into(), json!(f.get(o)));
                m.insert("text".into(), json!(if readable { f.text(f.get(o)) } else { "unreadable".into() }));
                if let Some(c) = c {
                    m.insert("copy".into(), json!(f.get(c)));
                }
                if let Some(d) = d {
                    m.insert("default".into(), json!(f.get(d)));
                    m.insert("default_text".into(), json!(f.text(f.get(d))));
                }
                if let Some(s) = s {
                    if f.get(s) != f.get(o) {
                        m.insert("staged".into(), json!(f.get(s)));
                        m.insert("staged_text".into(), json!(f.text(f.get(s))));
                    }
                }
            }
            m.insert("changed".into(), json!(readable && d.is_some_and(|d| f.get(d) != f.get(o))));
            m.insert("copy_differs".into(), json!(readable && copy_readable && c.zip(want_c).is_some_and(|(c, want)| f.get(c) != f.get(want))));
            j
        })
        .collect();
    let mut j = json!({
        "off": format!("0x{:03X}", w.off),
        "name": w.name,
        "raw": w.raw,
        "secret": w.secret,
        "readable": readable,
        "value": if hide { Value::Null } else { json!(hex(o, 32)) },
        "fields": fields,
    });
    if let Some(reg) = w.installed {
        if let Some(v) = r.installed.get(&reg) {
            j["installed"] = json!(hex(*v, 32));
            j["pending_reset"] = json!(readable && *v != o);
        }
    }
    j
}

/// Risk class of changing a UCB: how hard the change is to undo.
pub fn risk(def: &UcbDef) -> &'static str {
    match (def.access, def.class) {
        (Access::ReadOnly(_), _) => "read_only",
        (_, Class::Otp) | (_, Class::Infineon) => "permanent",
        _ if def.name.starts_with("HSMCOTP") || def.name == "HSMCFG" => "permanent",
        _ if def.has_password() || def.class == Class::Boot => "password",
        _ => "reversible",
    }
}

/// One UCB definition as the views show it.
pub fn ucb_json(i: usize, r: &Readout, staged: &Staged, opt: &ViewOptions) -> Value {
    let def = &UCBS[i];
    let words = words(def, &r.device);
    let default = delivery_default(def).or_else(|| opt.baseline.and_then(|b| content_from(b, def.orig)));
    let st = staged.get(&i);
    let (o, c) = r.content_states(def);
    let (io, ic) = r.installed_states(def);
    let wj: Vec<Value> = words.iter().map(|w| word_json(def, w, r, st, default.as_ref(), opt.reveal)).collect();
    let count = |key: &str| wj.iter().flat_map(|w| w["fields"].as_array().cloned().unwrap_or_default()).filter(|f| f[key] == json!(true)).count();
    let staged_n = wj.iter().flat_map(|w| w["fields"].as_array().cloned().unwrap_or_default()).filter(|f| f.get("staged").is_some()).count();
    let reserved: Vec<Value> = reserved_offsets(&words)
        .into_iter()
        .filter(|&off| def.indices().any(|x| r.readable(x, off) && r.word(x, off) != 0))
        .map(|off| json!({ "off": format!("0x{off:03X}"), "orig": hex(r.word(def.orig, off), 32), "copy": def.copy.map(|x| hex(r.word(x, off), 32)) }))
        .collect();
    let mut messages: Vec<Value> = Vec::new();
    if def.class == Class::Boot && r.fully_readable(def.orig) {
        let oc = r.data[def.orig as usize];
        let cc = def.copy.map(|x| r.data[x as usize]);
        let defined = get32(&oc, 0) != 0 || get32(&oc, 4) != 0;
        if defined {
            if bmhd_valid(&oc) {
                messages.push(json!({ "level": "ok", "text": format!("Valid boot header: start 0x{:08X}, mode {}.", get32(&oc, 4), Field { name: String::new(), label: String::new(), msb: 3, lsb: 1, kind: Kind::Enum(HWCFG, true) }.text((get32(&oc, 0) >> 1) & 7)) }));
            } else {
                messages.push(json!({ "level": "warn", "text": "ORIG holds a boot header the SSW does not accept (header ID, mode or checksum wrong)." }));
            }
            if let Some(cc) = cc {
                if cc[..16] != oc[..16] {
                    messages.push(json!({ "level": "warn", "text": "COPY differs from ORIG. If ORIG gets damaged, the SSW uses COPY. The next write makes them equal." }));
                }
            }
        }
    }
    if !reserved.is_empty() {
        messages.push(json!({ "level": "warn", "text": format!("{} reserved word(s) are not erased.", reserved.len()) }));
    }
    if words.iter().any(|w| !r.readable(def.orig, w.off)) {
        messages.push(json!({ "level": "info", "text": if def.has_password() { "Read-protected: set the password with `ucb password` and run `ucb unlock` to read it." } else { "Not readable by the debugger in this state." } }));
    }
    let state = r.state(def);
    json!({
        "name": def.name,
        "group": def.class.group(),
        "class": def.class.key(),
        "orig": def.orig,
        "copy": def.copy,
        "addr": def.indices().map(|x| format!("0x{:08X}", ucb_addr(x))).collect::<Vec<_>>(),
        "state": state.name(),
        "orig_state": o.name(),
        "copy_state": c.map(|s| s.name()),
        "installed_state": State::pair(io, ic).name(),
        "protection_disabled": r.prodis(def),
        "risk": risk(def),
        "writable": matches!(def.access, Access::Writable),
        "read_only_reason": match def.access { Access::ReadOnly(why) => Some(why), _ => None },
        "about": def.about,
        "password": def.uc.map(|_| json!({ "known": opt.known_passwords.contains(&def.name) })),
        "changed": if def.class == Class::Infineon && opt.baseline.is_none() { 0 } else { count("changed") },
        "orig_copy_differ": count("copy_differs") > 0,
        "staged": staged_n,
        "default": if default.is_some() { if def.class == Class::Infineon { "baseline" } else { "delivery" } } else { "none" },
        "reserved_nonzero": reserved,
        "messages": messages,
        "words": wj,
    })
}

/// Plain-language status of the device for the summary cards.
pub fn status_json(r: &Readout) -> Value {
    let g = |reg: u32| r.installed.get(&reg).copied();
    let dbg = g(HF_PROCONDBG);
    let debug = match dbg {
        Some(v) if v & 2 != 0 => "locked (password)",
        Some(v) if v & 1 != 0 => "OCDS disabled",
        Some(_) => "open",
        None => "unknown",
    };
    let rpro = g(HF_PROCONPF).is_some_and(|v| v >> 31 != 0) || g(HF_PROCONDF).is_some_and(|v| v >> 31 != 0);
    let boot = (0..4u8)
        .map(|i| (i, r.data[i as usize], r.data[i as usize + 8]))
        .find_map(|(i, o, c)| {
            if r.fully_readable(i) && bmhd_valid(&o) {
                Some(format!("0x{:08X} (BMHD{i})", get32(&o, 4)))
            } else if r.fully_readable(i + 8) && bmhd_valid(&c) {
                Some(format!("0x{:08X} (BMHD{i} COPY)", get32(&c, 4)))
            } else {
                None
            }
        })
        .unwrap_or_else(|| "no valid boot header".into());
    let hsm = match g(SP_PROCONHSMCFG) {
        Some(v) if v & 1 != 0 => "boots",
        Some(_) => "not enabled",
        None => "unknown",
    };
    let otp_sets = (0..8u8).filter(|y| matches!(State::of_field(r.confirm_regs[2] >> (2 * y)), State::Confirmed | State::Errored)).count();
    json!({
        "debug": debug,
        "flash_read_protection": rpro,
        "boot": boot,
        "hsm": hsm,
        "otp": if otp_sets == 0 { "nothing permanent".to_string() } else { format!("{otp_sets} OTP set(s) confirmed or errored") },
    })
}

/// The view of the whole readout (or one UCB).
pub fn view_json(r: &Readout, staged: &Staged, opt: &ViewOptions) -> Value {
    let ucbs: Vec<Value> = (0..UCBS.len()).filter(|&i| opt.only.is_none_or(|o| o == i)).map(|i| ucb_json(i, r, staged, opt)).collect();
    json!({
        "device": {
            "name": r.device.name,
            "chip_id": hex(r.device.chip_id, 32),
            "variant": r.device.variant,
            "uid": r.uid(),
            "pf_sectors": r.device.pf_sectors,
            "cores": r.device.cores,
        },
        "time": r.time,
        "write_enabled": opt.write_enabled,
        "status": status_json(r),
        "ucbs": ucbs,
    })
}

// --- Export ------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
    Orig,
    Copy,
    Both,
}

/// Which UCBs `export` writes.
#[derive(Clone, Debug)]
pub struct Selection {
    /// Definitions by name; empty: all.
    pub names: Vec<usize>,
    pub states: Vec<State>,
    pub classes: Vec<Class>,
    pub changed_only: bool,
    pub which: Which,
    /// Export the staged (not yet written) content instead of the device content.
    pub staged: bool,
}

impl Default for Selection {
    fn default() -> Self {
        Selection { names: Vec::new(), states: Vec::new(), classes: Vec::new(), changed_only: false, which: Which::Both, staged: false }
    }
}

pub struct Export {
    pub image: Image,
    pub exported: Vec<&'static str>,
    pub skipped: Vec<String>,
    pub passwords: bool,
}

/// The selected UCBs as an image: whole UCBs of 512 bytes. UCBs with unreadable words are
/// skipped and reported.
pub fn export(r: &Readout, staged: &Staged, baseline: Option<&Image>, sel: &Selection) -> Result<Export> {
    let mut sections = Vec::new();
    let mut out = Export { image: Image::default(), exported: Vec::new(), skipped: Vec::new(), passwords: false };
    for (i, def) in UCBS.iter().enumerate() {
        if !sel.names.is_empty() && !sel.names.contains(&i) {
            continue;
        }
        if !sel.classes.is_empty() && !sel.classes.contains(&def.class) {
            continue;
        }
        if !sel.states.is_empty() && !sel.states.contains(&r.state(def)) {
            continue;
        }
        if def.class == Class::Spare && sel.names.is_empty() {
            continue;
        }
        if sel.changed_only {
            if def.indices().any(|x| !r.fully_readable(x)) {
                continue;
            }
            let default = delivery_default(def).or_else(|| baseline.and_then(|b| content_from(b, def.orig)));
            let differs = match default {
                Some(d) => def.indices().any(|x| r.data[x as usize] != d) || (sel.staged && staged.contains_key(&i)),
                None => false,
            };
            if !differs {
                continue;
            }
        }
        let indices: Vec<u8> = match (sel.which, def.copy) {
            (Which::Copy, Some(c)) => vec![c],
            (Which::Both, Some(c)) => vec![def.orig, c],
            _ => vec![def.orig],
        };
        if let Some(x) = indices.iter().find(|&&x| !r.fully_readable(x)) {
            out.skipped.push(format!("{}: UCB{x} is not fully readable", def.name));
            continue;
        }
        for x in indices {
            let content = match (sel.staged, staged.get(&i)) {
                (true, Some(s)) => *s,
                _ => r.data[x as usize],
            };
            sections.push(Section { addr: ucb_addr(x), data: content.to_vec() });
        }
        if def.uc.is_some() && (0..8).any(|k| r.word(def.orig, PW + 4 * k) != 0) {
            out.passwords = true;
        }
        out.exported.push(def.name);
    }
    if sections.is_empty() {
        return Err(usage(if out.skipped.is_empty() { "no UCB matches the selection".to_string() } else { format!("nothing to export: {}", out.skipped.join("; ")) }));
    }
    out.image = Image::from_sections(sections);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tc37x() -> Device {
        Device::from_chip_id(0x1020_7083, &[], 3).unwrap()
    }

    #[test]
    fn bmhd_crc_matches_the_reference_header() {
        // iLLD's default BMHD: internal start at 0xA0000000, lockstep CPU0..3, BMHDID 0xB359.
        assert_eq!(bmhd_crc(0xB359_00FE, 0xA000_0000), 0x3179_5570);
        assert_eq!(!bmhd_crc(0xB359_00FE, 0xA000_0000), 0xCE86_AA8F);
    }

    #[test]
    fn table_covers_every_ucb_once() {
        let mut seen = [0u8; COUNT];
        for d in UCBS {
            for i in d.indices() {
                seen[i as usize] += 1;
            }
        }
        assert!(seen.iter().all(|&n| n == 1), "{seen:?}");
        assert_eq!(find("ucb_dbg"), find("DBG"));
        assert_eq!(UCBS[find("26").unwrap()].name, "DBG");
    }

    #[test]
    fn words_follow_the_um_tables() {
        let dev = tc37x();
        let w = |name: &str| words(&UCBS[find(name).unwrap()], &dev);
        let offs = |v: Vec<Word>| v.iter().map(|w| (w.off, w.name.clone())).collect::<Vec<_>>();
        let pf = offs(w("PFLASH"));
        assert!(pf.contains(&(0x000, "PROCONP00".into())) && pf.contains(&(0x034, "PROCONP15".into())) && pf.contains(&(0x0C0, "PROCONPF".into())));
        assert!(pf.contains(&(0x100, "PW0".into())) && pf.contains(&(0x11C, "PW7".into())) && pf.contains(&(0x1F0, "CONFIRMATION".into())));
        let otp = offs(w("OTP3"));
        assert!(otp.contains(&(0x100, "PROCONWOP00".into())) && otp.contains(&(0x1E8, "PROCONTP".into())));
        let redsec = offs(w("REDSEC"));
        assert!(redsec.contains(&(0x180, "DF0_0L".into())) && redsec.contains(&(0x1C4, "DF1_0H".into())) && redsec.contains(&(0x1F8, "CONFIRMATION_COPY".into())));
        let swap = offs(w("SWAP"));
        assert!(swap.contains(&(0x0F8, "CONFIRMATIONL15".into())));
        // TC37x: no CPU3..5DDIS, no LSENA3.
        let tp = w("OTP0").into_iter().find(|w| w.name == "PROCONTP").unwrap();
        assert!(tp.fields.iter().any(|f| f.name == "CPU2DDIS") && !tp.fields.iter().any(|f| f.name == "CPU3DDIS"));
        let bmi = &w("BMHD1")[0];
        assert!(bmi.fields.iter().any(|f| f.name == "LSENA2") && !bmi.fields.iter().any(|f| f.name == "LSENA3"));
        // Unpublished UCBs cover every word.
        assert_eq!(w("TEST").len(), WORDS);
        assert!(reserved_offsets(&w("TEST")).is_empty());
    }

    #[test]
    fn sector_lists() {
        assert_eq!(sectors_text(0x10F, 32, "S", 32), "S32-S35, S40");
        assert_eq!(parse_sectors("S32-S35, S40", 32, "S", 32), Ok(0x10F));
        assert_eq!(parse_sectors("s63", 32, "S", 32), Ok(1 << 31));
        assert!(parse_sectors("S10", 32, "S", 32).is_err());
        assert_eq!(parse_sectors("none", 8, "HSM", 32), Ok(0));
    }

    #[test]
    fn states_of_pairs() {
        assert_eq!(State::pair(State::Errored, Some(State::Confirmed)), State::Confirmed);
        assert_eq!(State::pair(State::Unlocked, Some(State::Errored)), State::Unlocked);
        assert_eq!(State::of_code(0), State::Errored);
    }

    #[test]
    fn stamps() {
        assert_eq!(stamp(0), "19700101-000000");
        assert_eq!(stamp(1_700_000_000), "20231114-221320");
    }
}
