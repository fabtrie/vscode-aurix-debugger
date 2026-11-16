//! Changing UCBs: staged edits, the review (checks, risk, token) and the guarded write.
//!
//! Every write goes through the same steps, whether the new content comes from edits by field
//! (`ucb set`) or from the UCB records of image files (`ucb review -image`):
//! 1. [`plan`]: the new content of every UCB that changes, and how it is written - by programming
//!    erased pages only (plus the over-programming the UM allows: UNLOCKED -> CONFIRMED, a SWAP
//!    entry invalidated with all ones), or by erasing and reprogramming.
//! 2. [`review`]: the general and per-UCB checks against the device state with the changes
//!    applied (and the images flashed in the same run), the risk, and a token over all of it.
//! 3. [`write`]: only with the session flag (`--ucb-write`), a token from the last review, from a
//!    person (not the REST/MCP API), with the device name typed for permanent changes. It halts
//!    the cores, reads again (refusing if the device changed since the review), repeats the checks
//!    that need the flash, saves a backup, writes COPY before ORIG (each read back), stops at the
//!    first DMU error, resumes the protection, logs, resets and reads again.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::adapter::Io;
use crate::ctx::Cx;
use crate::error::{other, usage, Error, Result};
use crate::flash::{self, DMU_HF_ERRSR, DMU_HF_STATUS, ERR_MASK, ERR_PROER, ERR_SQER, KEY, KEY_CLEAR_STATUS, KEY_RESET_TO_READ, LOAD_PAGE, SEQ_ADDR, SEQ_CMD, SEQ_COUNT};
use crate::image::Image;
use crate::target::TargetState;
use crate::ucb::{self, get32, put32, words, Access, Class, Content, Kind, Readout, Staged, State, UcbDef, CONF, CONFIRMED, PW, SIZE, UCBS, UNLOCKED};
use crate::{log_info, log_warn, ops};

/// Who runs a Tcl command: the write commands refuse the REST/MCP API, so a write always comes
/// from a person (VS Code, the Tcl console).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Origin {
    #[default]
    Person,
    Api,
}

/// The session's UCB state.
#[derive(Default)]
pub struct UcbState {
    /// `--ucb-write`: the launch configuration (`ucbWrite`) or a hub profile allows writing.
    pub write_enabled: bool,
    /// Baselines, backups and the write log; default [`ucb::default_dir`].
    pub dir: Option<PathBuf>,
    pub readout: Option<Readout>,
    pub staged: Staged,
    /// Passwords for Disable Protection, by definition name. Never logged.
    pub passwords: HashMap<&'static str, [u32; 8]>,
    pub review: Option<Review>,
    /// UCBs written UNLOCKED since the last reset: they are confirmed only after a reset showed
    /// that the setting works.
    pub written_since_reset: BTreeSet<usize>,
    /// The image files `ucb compare` compares with (`ucb apply` stages from them).
    pub compare: Option<CompareState>,
}

/// The image files of `ucb compare`.
#[derive(Clone, Debug)]
pub struct CompareState {
    pub files: Vec<crate::image::ImageFile>,
    pub image: Image,
}

impl UcbState {
    pub fn dir(&self) -> PathBuf {
        self.dir.clone().unwrap_or_else(ucb::default_dir)
    }
    /// The password that disables the protection of `def` (BMHD1..3 use BMHD0's).
    pub fn password(&self, def: &UcbDef, debug_password: Option<[u32; 8]>) -> Option<[u32; 8]> {
        let key = if def.name.starts_with("BMHD") { "BMHD0" } else { def.name };
        self.passwords.get(key).copied().or(if def.name == "DBG" { debug_password } else { None })
    }
}

// --- Device and readout ----------------------------------------------------------------------

/// The device behind the session, from `SCU_CHIPID` and the cfg.
pub fn device(cx: &Cx) -> Result<ucb::Device> {
    let (banks, cores) = cx.with(|c| {
        let banks: Vec<(u32, u32)> = c.flash_banks.iter().map(|b| (b.base, b.size)).collect();
        let cores = c.targets.iter().filter(|t| t.core.arch().name == "tricore").count() as u32;
        (banks, cores)
    });
    let chip_id = cx.with(|c| c.io()?.read32(ucb::SCU_CHIPID))?;
    ucb::Device::from_chip_id(chip_id, &banks, cores)
}

/// Reads all UCBs and keeps the readout; saves the chip's baseline on its first readout.
pub fn refresh(cx: &Cx) -> Result<Readout> {
    let dev = device(cx)?;
    let r = cx.with(|c| ucb::read(&mut c.io()?, dev))?;
    let dir = cx.with(|c| c.ucb.dir());
    let _ = ucb::baseline(&dir, &r);
    cx.with(|c| c.ucb.readout = Some(r.clone()));
    Ok(r)
}

/// The last readout, reading the device if there is none.
pub fn readout(cx: &Cx) -> Result<Readout> {
    match cx.with(|c| c.ucb.readout.clone()) {
        Some(r) => Ok(r),
        None => refresh(cx),
    }
}

// --- Staging ---------------------------------------------------------------------------------

/// A field address: `<UCB>.<word>.<field>`, `<UCB>.<word>` (the whole word) or `<UCB>.PW` (the
/// password, eight words).
pub struct Spec {
    pub def: usize,
    pub word: Option<usize>,
    pub field: Option<usize>,
    pub password: bool,
}

pub fn parse_spec(dev: &ucb::Device, s: &str) -> Result<Spec> {
    let mut parts = s.split('.');
    let name = parts.next().unwrap_or_default();
    let def = ucb::find(name).ok_or_else(|| usage(format!("unknown UCB '{name}' (see `ucb show`)")))?;
    let d = &UCBS[def];
    let Some(wname) = parts.next() else { return Err(usage(format!("'{s}': give <UCB>.<word>[.<field>]"))) };
    if wname.eq_ignore_ascii_case("PW") || wname.eq_ignore_ascii_case("password") {
        if d.uc.is_none() {
            return Err(usage(format!("UCB_{} has no password", d.name)));
        }
        return Ok(Spec { def, word: None, field: None, password: true });
    }
    let ws = words(d, dev);
    let wi = ws
        .iter()
        .position(|w| w.name.eq_ignore_ascii_case(wname))
        .or_else(|| crate::util::parse_u32(wname).ok().and_then(|o| ws.iter().position(|w| w.off as u32 == o)))
        .ok_or_else(|| usage(format!("UCB_{} has no word '{wname}' (words: {})", d.name, ws.iter().map(|w| w.name.as_str()).collect::<Vec<_>>().join(", "))))?;
    let field = match parts.next() {
        None => None,
        Some(fname) => Some(
            ws[wi]
                .fields
                .iter()
                .position(|f| f.name.eq_ignore_ascii_case(fname))
                .ok_or_else(|| usage(format!("{}.{} has no field '{fname}' (fields: {})", d.name, ws[wi].name, ws[wi].fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(", "))))?,
        ),
    };
    Ok(Spec { def, word: Some(wi), field, password: false })
}

/// Parses eight 32-bit hex words (or one token of 64 hex digits), PW0 first.
pub fn parse_password(words: &[&str]) -> std::result::Result<[u32; 8], String> {
    let joined;
    let ws: Vec<&str> = if words.len() == 1 && words[0].trim_start_matches("0x").len() == 64 {
        joined = words[0].trim_start_matches("0x").to_string();
        (0..8).map(|i| &joined[i * 8..(i + 1) * 8]).collect()
    } else {
        words.to_vec()
    };
    if ws.len() != 8 {
        return Err(format!("a password is 8 words PW0..PW7 ({} given)", ws.len()));
    }
    let mut pw = [0u32; 8];
    for (i, w) in ws.iter().enumerate() {
        pw[i] = u32::from_str_radix(w.trim_start_matches("0x").trim_start_matches("0X"), 16).map_err(|_| format!("PW{i} is not a 32-bit hex word"))?;
    }
    Ok(pw)
}

/// Makes the derived words of a boot mode header consistent: BMHDID and both checksums.
pub fn fix_bmhd(c: &mut Content) {
    let (mut bmi, stad) = (get32(c, 0), get32(c, 4));
    if bmi == 0 && stad == 0 {
        put32(c, 8, 0);
        put32(c, 12, 0);
        return;
    }
    bmi = (bmi & 0xFFFF) | (0xB359 << 16);
    put32(c, 0, bmi);
    let crc = ucb::bmhd_crc(bmi, stad);
    put32(c, 8, crc);
    put32(c, 12, !crc);
}

/// Stages a change: the new content starts from the staged one, else from the device's ORIG.
pub fn stage(cx: &Cx, spec: &str, value: &[&str]) -> Result<String> {
    let r = readout(cx)?;
    let s = parse_spec(&r.device, spec)?;
    let d = &UCBS[s.def];
    if let Access::ReadOnly(why) = d.access {
        return Err(usage(format!("UCB_{}: {why}", d.name)));
    }
    if !r.fully_readable(d.orig) {
        return Err(usage(format!("UCB_{} is not readable: set its password (`ucb password {}`) and run `ucb unlock {}` first", d.name, d.name, d.name)));
    }
    let mut content = cx.with(|c| c.ucb.staged.get(&s.def).copied()).unwrap_or(r.data[d.orig as usize]);
    let what = if s.password {
        let pw = parse_password(value).map_err(usage)?;
        for (i, w) in pw.iter().enumerate() {
            put32(&mut content, PW + 4 * i as u16, *w);
        }
        format!("UCB_{}.PW", d.name)
    } else {
        let ws = words(d, &r.device);
        let w = &ws[s.word.unwrap()];
        let text = value.join(" ");
        let old = get32(&content, w.off);
        let new = match s.field {
            Some(fi) => {
                let f = &w.fields[fi];
                if let Kind::Auto(why) = &f.kind {
                    return Err(usage(format!("{}.{}.{} is {why}", d.name, w.name, f.name)));
                }
                f.put(old, f.parse(&text).map_err(usage)?)
            }
            None => {
                if w.fields.iter().any(|f| matches!(f.kind, Kind::Auto(_))) {
                    return Err(usage(format!("{}.{} is computed when the UCB is written", d.name, w.name)));
                }
                crate::util::parse_u32(&text).map_err(usage)?
            }
        };
        put32(&mut content, w.off, new);
        format!("UCB_{}.{}{}", d.name, w.name, s.field.map(|fi| format!(".{}", w.fields[fi].name)).unwrap_or_default())
    };
    if d.class == Class::Boot {
        fix_bmhd(&mut content);
    }
    cx.with(|c| {
        if content == r.data[d.orig as usize] && d.copy.is_none_or(|x| r.data[x as usize] == content) {
            c.ucb.staged.remove(&s.def);
        } else {
            c.ucb.staged.insert(s.def, content);
        }
        c.ucb.review = None;
    });
    Ok(format!("{what} staged"))
}

/// Stages the confirmation UNLOCKED -> CONFIRMED of a UCB.
pub fn stage_confirm(cx: &Cx, name: &str) -> Result<String> {
    let i = ucb::find(name).ok_or_else(|| usage(format!("unknown UCB '{name}'")))?;
    let d = &UCBS[i];
    let mut msgs = Vec::new();
    for &off in d.confirm {
        let r = readout(cx)?;
        let word = words(d, &r.device).into_iter().find(|w| w.off == off).map(|w| w.name).unwrap_or_default();
        msgs.push(stage(cx, &format!("{}.{}.CODE", d.name, word), &["CONFIRMED"])?);
    }
    Ok(msgs.join("\n"))
}

// --- Plan --------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Method {
    /// Only erased pages are programmed (or over-programmed as the UM allows): no erase.
    Program,
    /// Erase, then program.
    Rewrite,
}

#[derive(Clone, Debug)]
pub struct Change {
    pub def: usize,
    pub new: Content,
    /// (UCB index, current content, method) for ORIG and COPY.
    pub targets: Vec<(u8, Content, Method)>,
}

impl Change {
    /// The new content of UCB `index` (COPY of UCB_SWAP: its addresses point into COPY).
    pub fn content_for(&self, index: u8) -> Content {
        ucb::content_for(&UCBS[self.def], index, &self.new)
    }
}

/// Whether one 8-byte page can go from `old` to `new` without an erase.
fn page_programmable(def: &UcbDef, page: u16, old: &[u8], new: &[u8]) -> bool {
    if old == new || old.iter().all(|&b| b == 0) {
        return true;
    }
    let w = |s: &[u8], k: usize| u32::from_le_bytes([s[4 * k], s[4 * k + 1], s[4 * k + 2], s[4 * k + 3]]);
    let (o0, o1, n0, n1) = (w(old, 0), w(old, 1), w(new, 0), w(new, 1));
    // UNLOCKED -> CONFIRMED with the following word 0 (UM 6.5.4.3.1).
    if def.confirm.contains(&page) && o0 == UNLOCKED && n0 == CONFIRMED && o1 == 0 && n1 == 0 {
        return true;
    }
    // A SWAP entry's confirmation invalidated with all ones (UM 1.5.2.2): CONFIRMATIONLx/Hx.
    if def.name == "SWAP" && page < 0x100 && page % 0x10 == 8 && n0 == 0xFFFF_FFFF && o1 == n1 {
        return true;
    }
    false
}

fn method(def: &UcbDef, old: &Content, new: &Content) -> Method {
    for p in (0..SIZE).step_by(8) {
        if !page_programmable(def, p as u16, &old[p..p + 8], &new[p..p + 8]) {
            return Method::Rewrite;
        }
    }
    Method::Program
}

/// What image files hold for one UCB definition.
#[derive(Clone, Debug)]
pub struct FileUcb {
    /// The content, normalized to ORIG (UCB_SWAP's COPY addresses moved), the device content where
    /// the images have no bytes.
    pub content: Content,
    /// Which UCBs of the definition the images touch: "orig", "copy" or "both".
    pub in_file: &'static str,
    /// Bytes the images cover, of the less covered UCB.
    pub covered: usize,
    /// ORIG and COPY records disagree (ORIG is used).
    pub conflict: bool,
}

impl FileUcb {
    pub fn notes(&self, d: &UcbDef) -> Vec<String> {
        let mut v = Vec::new();
        if self.covered < SIZE {
            v.push(format!("UCB_{}: the images cover {} of 512 bytes; the rest is kept from the device", d.name, self.covered));
        }
        if self.in_file != "both" && d.copy.is_some() {
            v.push(format!("UCB_{}: only {} is in the images; it is written to ORIG and COPY", d.name, self.in_file.to_uppercase()));
        }
        if self.conflict {
            v.push(format!("UCB_{}: the images hold different contents for ORIG and COPY; ORIG is used", d.name));
        }
        v
    }
}

/// What the images hold for UCB definition `d`; `None` if they have no bytes there.
pub fn image_ucb(r: &Readout, d: &UcbDef, img: &Image) -> Option<FileUcb> {
    let mut contents: Vec<(u8, Content, usize)> = Vec::new();
    for x in d.indices() {
        let base = ucb::ucb_addr(x);
        if !img.touches(base, base as u64 + SIZE as u64) {
            continue;
        }
        let mut c = r.data[x as usize];
        let mut n = 0;
        for (k, byte) in c.iter_mut().enumerate() {
            if let Some(b) = img.byte_at(base + k as u32) {
                *byte = b;
                n += 1;
            }
        }
        // Normalized to ORIG (UCB_SWAP's COPY holds COPY addresses).
        contents.push((x, ucb::relocate_swap(d, &c, x, d.orig), n));
    }
    let first = contents.first()?;
    Some(FileUcb {
        content: first.1,
        in_file: match (contents.len(), first.0 == d.orig) {
            (2, _) => "both",
            (_, true) => "orig",
            _ => "copy",
        },
        covered: contents.iter().map(|c| c.2).min().unwrap_or(0),
        conflict: contents.len() == 2 && contents[0].1 != contents[1].1,
    })
}

/// The new content of every UCB that changes. From the staged edits, or from the UCB records of
/// `image` (ORIG records also go to COPY; ORIG and COPY records must agree; a partial record is
/// merged into the device content).
pub fn plan(r: &Readout, staged: &Staged, image: Option<&Image>) -> Result<(Vec<Change>, Vec<String>)> {
    let mut notes = Vec::new();
    let mut new: BTreeMap<usize, Content> = BTreeMap::new();
    match image {
        Some(img) => {
            if !staged.is_empty() {
                return Err(usage("there are staged UCB edits: discard them (`ucb discard`) before reviewing image files"));
            }
            for (i, d) in UCBS.iter().enumerate() {
                let Some(fu) = image_ucb(r, d, img) else { continue };
                if fu.conflict {
                    return Err(other(format!("the images hold different contents for UCB_{} ORIG and COPY", d.name)));
                }
                notes.extend(fu.notes(d));
                new.insert(i, fu.content);
            }
        }
        None => new.extend(staged.iter().map(|(k, v)| (*k, *v))),
    }
    let mut changes = Vec::new();
    for (i, content) in new {
        let d = &UCBS[i];
        let targets: Vec<(u8, Content, Method)> = d
            .indices()
            .map(|x| (x, r.data[x as usize], ucb::content_for(d, x, &content)))
            .filter(|(_, old, want)| old != want)
            .map(|(x, old, want)| (x, old, method(d, &old, &want)))
            .collect();
        if !targets.is_empty() {
            changes.push(Change { def: i, new: content, targets });
        }
    }
    // Locks last: protection after everything that needs the debugger to write it.
    let order = |name: &str| match name {
        "ECPRIO" | "SWAP" => 0,
        "HSM" | "HSMCOTP0" | "HSMCOTP1" | "HSMCFG" => 1,
        n if n.starts_with("OTP") => 2,
        n if n.starts_with("BMHD") => 3,
        "DFLASH" => 4,
        "PFLASH" => 5,
        "DBG" => 6,
        _ => 7,
    };
    changes.sort_by_key(|c| (order(UCBS[c.def].name), UCBS[c.def].name));
    Ok((changes, notes))
}

// --- Checks ------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Info,
    Warn,
    Block,
}

#[derive(Clone, Debug)]
pub struct Problem {
    pub level: Level,
    pub ucb: &'static str,
    pub title: String,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Risk {
    Reversible,
    Password,
    Permanent,
}

impl Risk {
    pub fn key(self) -> &'static str {
        match self {
            Risk::Reversible => "reversible",
            Risk::Password => "password",
            Risk::Permanent => "permanent",
        }
    }
    pub fn text(self) -> &'static str {
        match self {
            Risk::Reversible => "Can be undone",
            Risk::Password => "Undo needs the password",
            Risk::Permanent => "Permanent",
        }
    }
}

/// Facts about the PFLASH the checks need: whether a sector is erased (DMU "verify erased", no
/// read), known only while the cores are halted.
pub type Erased = BTreeMap<u32, bool>;

pub struct Env<'a> {
    pub image: Option<&'a Image>,
    /// Erase state of the sectors [`flash_addresses`] lists; `None` while the cores run.
    pub erased: Option<&'a Erased>,
    pub password_known: &'a dyn Fn(&UcbDef) -> bool,
    pub written_since_reset: &'a BTreeSet<usize>,
}

fn image_has(img: Option<&Image>, addr: u32) -> bool {
    img.is_some_and(|i| [0x8000_0000, 0xA000_0000].iter().any(|seg| i.byte_at((addr & 0x0FFF_FFFF) | seg).is_some()))
}

fn sector_of(addr: u32) -> u32 {
    (addr & 0x0FFF_FFFF & !(flash::SECTOR_SIZE - 1)) | 0xA000_0000
}

/// HSM code sector `n` (HSMnX) is PF0 logical sector Sn.
fn hsm_sector(n: u32) -> u32 {
    0xA000_0000 + n * flash::SECTOR_SIZE
}

/// The PFLASH sectors whose erase state the checks of these changes need.
pub fn flash_addresses(changes: &[Change]) -> Vec<u32> {
    let mut v = BTreeSet::new();
    for ch in changes {
        let d = &UCBS[ch.def];
        if d.class == Class::Boot && matches!((get32(&ch.new, 0) >> 1) & 7, 7 | 6) {
            v.insert(sector_of(get32(&ch.new, 4)));
        }
        if d.name.starts_with("HSMCOTP") && get32(&ch.new, 0x18) & 1 != 0 {
            let cbs = get32(&ch.new, 4);
            for k in 0..4 {
                v.insert(hsm_sector((cbs >> (8 * k)) & 0x3F));
            }
        }
        if d.class == Class::Otp {
            for (p, s) in otp_sectors(&ch.new) {
                if p == 0 {
                    v.insert(0xA000_0000 + s * flash::SECTOR_SIZE);
                }
            }
        }
    }
    v.into_iter().collect()
}

/// (bank, sector) of the OTP and WOP bits of an OTP set.
fn otp_sectors(c: &Content) -> Vec<(u32, u32)> {
    let mut v = Vec::new();
    for base in [0x000u16, 0x100] {
        for p in 0..6u16 {
            for s in 0..6u16 {
                let w = get32(c, base + p * 0x20 + s * 4);
                for bit in 0..32 {
                    if w >> bit & 1 != 0 {
                        v.push((p as u32, s as u32 * 32 + bit));
                    }
                }
            }
        }
    }
    v.sort();
    v.dedup();
    v
}

/// The content of every UCB after the changes.
fn after(r: &Readout, changes: &[Change]) -> Vec<Content> {
    let mut v = r.data.clone();
    for ch in changes {
        for x in UCBS[ch.def].indices() {
            v[x as usize] = ch.content_for(x);
        }
    }
    v
}

pub fn change_risk(r: &Readout, ch: &Change) -> Risk {
    let d = &UCBS[ch.def];
    if d.class == Class::Otp || d.class == Class::Infineon || d.name.starts_with("HSMCOTP") || d.name == "HSMCFG" {
        return Risk::Permanent;
    }
    let new_state = State::of_code(get32(&ch.new, d.confirm.first().copied().unwrap_or(CONF)));
    if new_state == State::Confirmed && r.state(d) != State::Confirmed {
        return if d.has_password() || d.class == Class::Boot { Risk::Password } else { Risk::Permanent };
    }
    let locks = match d.name {
        "DBG" => get32(&ch.new, 0) & 3 != 0,
        "PFLASH" => get32(&ch.new, 0xC0) >> 31 != 0,
        "DFLASH" => get32(&ch.new, 4) >> 31 != 0,
        _ => false,
    };
    if locks {
        Risk::Password
    } else {
        Risk::Reversible
    }
}

struct Ck<'a> {
    out: Vec<Problem>,
    passed: Vec<String>,
    r: &'a Readout,
}

impl Ck<'_> {
    fn p(&mut self, level: Level, ucb: &'static str, title: impl Into<String>, detail: impl Into<String>) {
        self.out.push(Problem { level, ucb, title: title.into(), detail: detail.into() });
    }
    fn ok(&mut self, s: impl Into<String>) {
        self.passed.push(s.into());
    }
}

/// All checks of a set of changes.
pub fn check(r: &Readout, changes: &[Change], env: &Env) -> (Vec<Problem>, Vec<String>) {
    let mut k = Ck { out: Vec::new(), passed: Vec::new(), r };
    let new_all = after(r, changes);
    let otp_locked = (0..8).any(|y| matches!(State::of_field(r.confirm_regs[2] >> (2 * y)), State::Confirmed | State::Errored));
    let hsmcotp_state = |n: usize| {
        let d = &UCBS[ucb::find(if n == 0 { "HSMCOTP0" } else { "HSMCOTP1" }).unwrap()];
        r.state(d)
    };
    for ch in changes {
        let d = &UCBS[ch.def];
        let n = d.name;
        if let Access::ReadOnly(why) = d.access {
            k.p(Level::Block, n, "Read-only UCB", why);
            continue;
        }
        if d.indices().any(|x| !r.fully_readable(x)) {
            k.p(Level::Block, n, "Not readable", "Its current content is not known: set the password (`ucb password`) and run `ucb unlock` first.");
            continue;
        }
        let old_state = r.state(d);
        let new_state = State::of_code(get32(&ch.new, d.confirm[0]));
        let rewrite = ch.targets.iter().any(|t| t.2 == Method::Rewrite);
        // Confirmation codes.
        let mut conf_ok = true;
        for &off in d.confirm {
            let v = get32(&ch.new, off);
            if v != UNLOCKED && v != CONFIRMED {
                conf_ok = false;
                k.p(Level::Block, n, "Invalid confirmation code", format!("0x{v:08X} at +0x{off:03X} would make the UCB ERRORED; only UNLOCKED (0x{UNLOCKED:08X}) or CONFIRMED (0x{CONFIRMED:08X})."));
            }
            if off as usize + 8 <= SIZE && get32(&ch.new, off + 4) != 0 {
                conf_ok = false;
                k.p(Level::Block, n, "Word after the confirmation code is not 0", format!("+0x{:03X} must stay 0 so that a later UNLOCKED -> CONFIRMED over-program is ECC-clean.", off + 4));
            }
        }
        if conf_ok {
            k.ok(format!("UCB_{n}: confirmation codes valid"));
        }
        // Reserved bits and words.
        let ws = words(d, &r.device);
        let old = r.data[d.orig as usize];
        let mut rsv_ok = true;
        for w in ws.iter().filter(|w| !w.raw) {
            let mask = !w.defined();
            let (nv, ov) = (get32(&ch.new, w.off) & mask, get32(&old, w.off) & mask);
            if nv != 0 && nv != ov {
                rsv_ok = false;
                k.p(Level::Block, n, format!("Reserved bits in {}", w.name), format!("0x{nv:08X} sets bits that are reserved on the {} (missing sectors or CPUs included).", r.device.name));
            }
            for f in &w.fields {
                if let Kind::Enum(e, true) = &f.kind {
                    let v = f.get(get32(&ch.new, w.off));
                    let header = !(d.class == Class::Boot && get32(&ch.new, 0) == 0 && get32(&ch.new, 4) == 0);
                    if header && !e.iter().any(|(x, _)| *x == v) && get32(&ch.new, w.off) != get32(&old, w.off) {
                        k.p(Level::Block, n, format!("Invalid {} ({})", f.label, f.name), format!("{v} is not one of: {}", e.iter().map(|(x, t)| format!("{x} = {t}")).collect::<Vec<_>>().join(", ")));
                    }
                }
            }
        }
        for off in ucb::reserved_offsets(&ws) {
            if get32(&ch.new, off) != get32(&old, off) && get32(&ch.new, off) != 0 {
                rsv_ok = false;
                k.p(Level::Block, n, format!("Reserved word +0x{off:03X}"), "Words the UM does not define must stay 0.");
            }
        }
        if rsv_ok {
            k.ok(format!("UCB_{n}: reserved bits stay 0"));
        }
        // What the DMU allows (UM 6.5.4.3).
        if old_state == State::Errored {
            k.p(Level::Block, n, "UCB is ERRORED", "The DMU refuses to program or erase an ERRORED UCB.");
        }
        if old_state == State::Confirmed && !r.prodis(d) {
            if d.has_password() || d.class == Class::Boot {
                if !(env.password_known)(d) {
                    k.p(Level::Block, n, "Password needed", format!("UCB_{n} is CONFIRMED: set its current password with `ucb password {}` (BMHD1..3 use BMHD0's).", if d.class == Class::Boot { "BMHD0" } else { n }));
                } else {
                    k.ok(format!("UCB_{n}: the password to disable its protection is known"));
                }
            } else if d.class != Class::Otp && !n.starts_with("HSMCOTP") {
                k.p(Level::Block, n, "CONFIRMED without a password", format!("UCB_{n} is CONFIRMED and has no password: it can no longer be changed."));
            }
        }
        if d.class == Class::Otp && otp_locked && rewrite {
            k.p(Level::Block, n, "OTP UCBs can no longer be erased", "An OTP set is CONFIRMED or ERRORED (PROINOTP): the DMU refuses every erase of the OTP UCBs.");
        }
        if d.class == Class::Otp && old_state == State::Confirmed {
            k.p(Level::Block, n, "OTP set is CONFIRMED", "A CONFIRMED OTP set can never be changed.");
        }
        if n.starts_with("HSMCOTP") {
            let (s0, s1) = (hsmcotp_state(0), hsmcotp_state(1));
            let any = |s: State| matches!(s, State::Confirmed | State::Errored);
            if rewrite && (any(s0) || any(s1)) {
                k.p(Level::Block, n, "HSMCOTP can no longer be erased", "An HSMCOTP set is CONFIRMED or ERRORED: the DMU refuses every erase of the HSMCOTP UCBs.");
            }
            if n == "HSMCOTP0" && (any(s0) || s1 == State::Errored) {
                k.p(Level::Block, n, "HSMCOTP0 is write protected", "HSMCOTP0 is CONFIRMED/ERRORED or HSMCOTP1 is ERRORED.");
            }
            if n == "HSMCOTP1" && (s0 != State::Unlocked || any(s1)) {
                k.p(Level::Block, n, "Only the HSM may program HSMCOTP1", "HSMCOTP0 is CONFIRMED (or a set is ERRORED).");
            }
        }
        if n == "HSMCFG" && old_state != State::Unlocked {
            k.p(Level::Block, n, "HSMCFG is write protected", "UCB_HSMCFG can only be changed while UNLOCKED.");
        }
        if n == "DBG" {
            let edm = r.installed.get(&ucb::HF_PROCONDBG).is_some_and(|v| (v >> 2) & 3 == 3);
            if edm {
                k.p(Level::Block, n, "Destructive debug entered", "While PROCONDBG.EDM says 'debug entered' the DMU blocks UCB_DBG.");
            }
        }
        // UNLOCKED -> CONFIRMED.
        if new_state == State::Confirmed && old_state != State::Confirmed {
            if env.written_since_reset.contains(&ch.def) {
                k.p(Level::Block, n, "Reset first", "This UCB was written UNLOCKED in this session: reset and check that the setting works before confirming it.");
            }
            if d.has_password() && (0..8).all(|i| get32(&ch.new, PW + 4 * i) == 0) {
                k.p(Level::Warn, n, "Password is all zero", "CONFIRMED with an all-zero password: anybody can disable the protection with eight zero words.");
            }
            k.p(Level::Warn, n, "CONFIRMED", match change_risk(r, ch) {
                Risk::Permanent => "Once CONFIRMED this UCB can never be changed again.",
                _ => "Once CONFIRMED the UCB can only be changed with its password. Keep it.",
            });
        }
        // Passwords.
        if d.has_password() {
            let pw = |c: &Content| (0..8).map(|i| get32(c, PW + 4 * i)).collect::<Vec<_>>();
            if pw(&ch.new) != pw(&old) {
                k.p(Level::Warn, n, "New password", "It becomes this session's password for the UCB (and for DBG the debug password). Write it down: a lost password of a CONFIRMED UCB cannot be recovered.");
            }
        }
        semantics(&mut k, d, ch, &old, &new_all, env);
    }
    // At least one valid boot mode header remains.
    if changes.iter().any(|c| UCBS[c.def].class == Class::Boot) {
        let valid = (0..4).any(|i| ucb::bmhd_valid(&new_all[i]) || ucb::bmhd_valid(&new_all[i + 8]));
        if valid {
            k.ok("A valid boot mode header remains");
        } else {
            k.p(Level::Block, "BMHD0", "No valid boot mode header", "After the write no BMHD would be valid: the device would not start the application.");
        }
    }
    k.out.sort_by_key(|p| std::cmp::Reverse(p.level));
    (k.out, k.passed)
}

fn erased_text(env: &Env, addr: u32) -> Option<bool> {
    env.erased.and_then(|e| e.get(&sector_of(addr)).copied())
}

/// Per-UCB checks.
fn semantics(k: &mut Ck, d: &UcbDef, ch: &Change, old: &Content, new_all: &[Content], env: &Env) {
    let n = d.name;
    let new = &ch.new;
    if d.class == Class::Boot {
        {
            let (bmi, stad) = (get32(new, 0), get32(new, 4));
            if bmi == 0 && stad == 0 {
                return;
            }
            if bmi >> 16 != 0xB359 {
                k.p(Level::Block, n, "Header ID is not 0xB359", "The SSW ignores this header.");
            }
            let crc = ucb::bmhd_crc(bmi, stad);
            if get32(new, 8) != crc || get32(new, 12) != !crc {
                k.p(Level::Block, n, "Checksum wrong", format!("CRCBMHD must be 0x{crc:08X} and CRCBMHD_N 0x{:08X} (CRC-32 over BMI_BMHDID and STAD as the SSW computes it).", !crc));
            } else {
                k.ok(format!("UCB_{n}: header ID and checksum valid"));
            }
            let hw = (bmi >> 1) & 7;
            if matches!(hw, 7 | 6) {
                let in_pf = flash::pflash_at(k.r.device.variant, stad).is_some();
                if !in_pf || stad % 4 != 0 {
                    k.p(Level::Block, n, "Start address not in PFLASH", format!("STAD 0x{stad:08X} must be a word-aligned PFLASH address."));
                } else if image_has(env.image, stad) {
                    k.ok(format!("UCB_{n}: the image flashed in this run holds code at 0x{stad:08X}"));
                } else {
                    match erased_text(env, stad) {
                        Some(true) => k.p(Level::Block, n, "No code at the start address", format!("The PFLASH sector of 0x{stad:08X} is erased: flash the application first, or add it to the images.")),
                        Some(false) => k.ok(format!("UCB_{n}: code programmed at 0x{stad:08X}")),
                        None => k.p(Level::Info, n, "Start address checked when writing", format!("Whether code is programmed at 0x{stad:08X} is checked with the cores halted.")),
                    }
                }
                if hw == 6 && (get32(old, 0) != bmi || get32(old, 4) != stad) {
                    k.p(Level::Warn, n, "Alternate boot mode", "The alternate boot mode header at STAD is not checked: make sure it is valid.");
                }
            }
            if bmi & 1 != 0 && get32(old, 0) & 1 == 0 {
                k.p(Level::Warn, n, "Boot mode pins disabled", "PINDIS=1: the HWCFG pins can no longer select a bootstrap loader for recovery.");
            }
            if bmi & (1 << 8) != 0 && old[1] & 1 == 0 {
                k.p(Level::Info, n, "LBIST at cold start", "LBIST runs at every cold power-on and lengthens the start-up.");
            }
        }
    }
    match n {
        "DBG" => {
            let v = get32(new, 0);
            let o = get32(old, 0);
            if v & 1 != 0 && v & 2 == 0 {
                k.p(Level::Block, n, "OCDS disabled without a debug interface lock", "The SSW never unlocks OCDSDIS=1 with DBGIFLCK=0, even with the password: the debugger would be locked out.");
            }
            if (v >> 2) & 3 != (o >> 2) & 3 {
                k.p(Level::Block, n, "EDM is set by the SSW", "PROCONDBG.EDM records a destructive debug entry; writing it by hand blocks CAN/FlexRay.");
            }
            if v & 2 != 0 && o & 2 == 0 {
                k.p(Level::Warn, n, "Debug interface locked", "The debugger connects only with the password from now on (`adapter debug_password`, set for this session automatically). TAS and BlueBox send it at reset.");
            }
            if (v >> 8) & 0xF != (o >> 8) & 0xF {
                k.p(Level::Warn, n, "DAP over CAN changed", "TIC selects whether DXCPL stays enabled on the CAN pins.");
            }
        }
        "PFLASH" => {
            if get32(new, 0xC0) >> 31 != 0 && get32(old, 0xC0) >> 31 == 0 {
                k.p(Level::Warn, n, "PFLASH read protection", "The SSW keeps the debug interface locked after reset; the flash can then be read only with the password.");
            }
            let protected: Vec<String> = (0..k.r.device.pf_sectors.len() as u16)
                .flat_map(|p| (0..6u16).map(move |s| (p, s)))
                .filter_map(|(p, s)| {
                    let w = get32(new, p * 0x20 + s * 4);
                    (w != 0).then(|| format!("PF{p} {}", ucb::sectors_text(w, 32, "S", s as u32 * 32)))
                })
                .collect();
            if !protected.is_empty() && (0..0xC0u16).step_by(4).any(|off| get32(new, off) != get32(old, off)) {
                k.p(Level::Warn, n, "Write-protected sectors", format!("{}: flashing them later needs Disable Protection with the password.", protected.join("; ")));
            }
        }
        "DFLASH" => {
            let v = get32(new, 4);
            if v >> 31 != 0 && get32(old, 4) >> 31 == 0 {
                k.p(Level::Warn, n, "DFLASH read protection", "The SSW keeps the debug interface locked after reset.");
            }
            if v & 1 != 0 && get32(old, 4) & 1 == 0 {
                k.p(Level::Warn, n, "DF0 EEPROM write locked", "DF0 can then be written only after Disable Protection.");
            }
            if v & (1 << 8) != 0 && get32(old, 4) & (1 << 8) == 0 {
                k.p(Level::Warn, n, "Oscillator set up by the SSW", "The SSW configures the oscillator from MODE, CAP*EN, AMPCTL and HYSCTL: values that do not match the crystal stop the clock.");
            }
        }
        "SWAP" => {
            let otp_swapen = (0..8).any(|y| (get32(&new_all[32 + y], 0x1E8) >> 16) & 3 == 3) || k.r.installed.get(&ucb::HF_PROCONTP).is_some_and(|v| (v >> 16) & 3 == 3);
            if !otp_swapen {
                k.p(Level::Warn, n, "SOTA not enabled", "SWAP entries are used only when SWAPEN is enabled in UCB_OTP.");
            }
            let mut end = false;
            for x in 0..16u16 {
                let o = x * 0x10;
                let (ml, mh, cl, chh) = (get32(new, o), get32(new, o + 4), get32(new, o + 8), get32(new, o + 12));
                let used = ml != 0 || mh != 0 || cl != 0 || chh != 0;
                if !used {
                    end = true;
                    continue;
                }
                if end {
                    k.p(Level::Block, n, format!("Entry {x} after an empty entry"), "SWAP entries are appended without gaps.");
                }
                let orig = ucb::ucb_addr(d.orig) + o as u32;
                if !matches!(ml, 0x55 | 0xAA) || mh != orig || chh != orig + 8 || !matches!(cl, CONFIRMED | 0xFFFF_FFFF) {
                    k.p(Level::Block, n, format!("Entry {x} malformed"), format!("Expected MARKERL 0x55 or 0xAA, MARKERH 0x{orig:08X}, CONFIRMATIONL 0x{CONFIRMED:08X} (or all ones once replaced), CONFIRMATIONH 0x{:08X}.", orig + 8));
                }
                let (oml, omh, ocl, och) = (get32(old, o), get32(old, o + 4), get32(old, o + 8), get32(old, o + 12));
                if (oml | omh | ocl | och) != 0 && (oml != ml || omh != mh || och != chh || (ocl != cl && cl != 0xFFFF_FFFF)) {
                    k.p(Level::Block, n, format!("Entry {x} rewritten"), "Existing SWAP entries are only invalidated (CONFIRMATIONL all ones), never changed.");
                }
            }
        }
        "HSM" => {
            let v = get32(new, 0);
            if v & 1 != 0 && get32(old, 0) & 1 == 0 {
                k.p(Level::Warn, n, "HSM debug disabled", "Only the HSM software can enable HSM debugging then.");
            }
            if v & 2 != 0 && get32(old, 0) & 2 == 0 {
                k.p(Level::Warn, n, "HSM keeps the debug interface locked", "The HSM firmware does not release the debug interface: only HSM software can unlock it.");
            }
        }
        x if x.starts_with("HSMCOTP") => {
            let cfg = get32(new, 0x18);
            if cfg & 1 != 0 {
                let cbs = get32(new, 4);
                let sectors: BTreeSet<u32> = (0..4).map(|i| (cbs >> (8 * i)) & 0x3F).collect();
                let mut found = false;
                let mut unknown = false;
                for s in &sectors {
                    let a = hsm_sector(*s);
                    if image_has(env.image, a) {
                        found = true;
                    } else {
                        match erased_text(env, a) {
                            Some(false) => found = true,
                            Some(true) => {}
                            None => unknown = true,
                        }
                    }
                }
                let list = sectors.iter().map(|s| format!("HSM{s}X (PF0 S{s})")).collect::<Vec<_>>().join(", ");
                // The DMU tells programmed from erased, not HSM code from application code.
                let app_starts: BTreeSet<u32> =
                    (0..4).flat_map(|i| [i, i + 8]).filter(|&i| ucb::bmhd_valid(&new_all[i])).map(|i| sector_of(get32(&new_all[i], 4))).collect();
                let shared: Vec<u32> = sectors.iter().copied().filter(|s| app_starts.contains(&hsm_sector(*s))).collect();
                if !shared.is_empty() && (get32(new, 4) != get32(old, 4) || get32(old, 0x18) & 1 == 0 || new_all.iter().zip(&k.r.data).take(12).any(|(a, b)| a != b)) {
                    k.p(Level::Warn, n, "HSM boot sector holds the application start", format!("{} is where a boot mode header starts the application: make sure it holds HSM code, not application code.", shared.iter().map(|s| format!("PF0 S{s}")).collect::<Vec<_>>().join(", ")));
                }
                if found {
                    k.ok(format!("UCB_{n}: HSM code present in {list}"));
                } else if unknown {
                    k.p(Level::Info, n, "HSM code checked when writing", format!("Whether {list} holds HSM code is checked with the cores halted."));
                } else {
                    k.p(Level::Block, n, "HSM boot enabled, but there is no HSM code", format!("{list} erased. Flash the HSM image first (or add it to the images: they are programmed before the UCBs)."));
                }
            }
            if cfg & 2 != 0 && get32(old, 0x18) & 2 == 0 {
                k.p(Level::Warn, n, "CPU0 waits for the HSM", "If the HSM code never acknowledges, the application never starts.");
            }
            if (cfg >> 11) & 3 == 3 && (get32(old, 0x18) >> 11) & 3 != 3 {
                k.p(Level::Warn, n, "Destructive debug entry", "Connecting a debugger with the password then switches CAN and FlexRay off until UCB_DBG is rewritten.");
            }
            let excl = get32(new, 8);
            if excl != 0 {
                if let Some(img) = env.image {
                    let hits: Vec<u32> = (0..32).filter(|b| excl >> b & 1 != 0 && img.touches(hsm_sector(*b), hsm_sector(*b) as u64 + flash::SECTOR_SIZE as u64)).collect();
                    if !hits.is_empty() {
                        k.p(Level::Warn, n, "HSM-exclusive sectors in the image", format!("{} become HSM-exclusive: the debugger cannot flash them afterwards.", hits.iter().map(|s| format!("PF0 S{s}")).collect::<Vec<_>>().join(", ")));
                    }
                }
                if excl != get32(old, 8) {
                    k.p(Level::Warn, n, "HSM-exclusive sectors", format!("{}: only the HSM can access them afterwards.", ucb::sectors_text(excl, 32, "HSM", 0)));
                }
            }
            if (get32(new, 0x10) != 0 || get32(new, 0x14) != 0) && (get32(new, 0x10) != get32(old, 0x10) || get32(new, 0x14) != get32(old, 0x14)) {
                k.p(Level::Warn, n, "HSM OTP sectors", "These sectors can never be reprogrammed once the set is CONFIRMED.");
            }
        }
        x if x.starts_with("OTP") => {
            let sectors = otp_sectors(new);
            if !sectors.is_empty() && sectors != otp_sectors(old) {
                let pf0_erased: Vec<u32> = sectors.iter().filter(|(p, s)| *p == 0 && erased_text(env, 0xA000_0000 + s * flash::SECTOR_SIZE) == Some(true)).map(|(_, s)| *s).collect();
                if !pf0_erased.is_empty() {
                    k.p(Level::Block, n, "OTP/WOP on erased sectors", format!("{} erased: protecting them makes them permanently empty.", pf0_erased.iter().map(|s| format!("PF0 S{s}")).collect::<Vec<_>>().join(", ")));
                }
                k.p(Level::Warn, n, "OTP/WOP sectors", format!("{} sector(s) become one-time programmable: they can never be reprogrammed once the set is CONFIRMED.", sectors.len()));
            }
            let tp = get32(new, 0x1E8);
            if (tp >> 8) & 3 != 0 && (get32(old, 0x1E8) >> 8) & 3 == 0 {
                k.p(Level::Warn, n, "Boot mode lock", "BML restricts the boot flow: no HWCFG pins, no bootstrap loader for recovery.");
            }
            if (tp >> 16) & 3 == 3 && (get32(old, 0x1E8) >> 16) & 3 != 3 {
                let swap = &new_all[23];
                let has_entry = get32(swap, 0) != 0 || get32(&new_all[31], 0) != 0;
                if !has_entry {
                    k.p(Level::Block, n, "SOTA without a SWAP entry", "SWAPEN needs a confirmed entry in UCB_SWAP selecting the address map.");
                } else {
                    k.p(Level::Warn, n, "SOTA enabled", "Both bank groups must hold valid images and boot headers.");
                }
            }
            if tp & 1 != 0 {
                k.p(Level::Info, n, "Tuning protection", "TP=1 protects the calibration (tuning) data.");
            }
        }
        _ => {}
    }
}

// --- Review ------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Review {
    pub token: String,
    pub changes: Vec<Change>,
    pub problems: Vec<Problem>,
    pub passed: Vec<String>,
    pub notes: Vec<String>,
    pub risk: Risk,
    /// The image files the changes come from, if any.
    pub images: Vec<crate::image::ImageFile>,
    /// The readout the review is based on.
    pub device_name: String,
    pub checks_deferred: bool,
}

impl Review {
    pub fn blocked(&self) -> bool {
        self.problems.iter().any(|p| p.level == Level::Block)
    }
}

fn fnv(data: impl IntoIterator<Item = u8>, mut h: u64) -> u64 {
    for b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01B3);
    }
    h
}

/// Whether the TriCore cores are known to be halted: at least one examined, all examined halted.
/// Without examined cores (e.g. a passive attach) nothing is known, and the checks that need DMU
/// commands wait for the write.
fn all_halted(cx: &Cx) -> bool {
    let n = cx.with(|c| c.targets.len());
    let cores: Vec<usize> = (0..n).filter(|&t| cx.with(|c| c.targets[t].core.examined() && c.targets[t].core.arch().name == "tricore")).collect();
    !cores.is_empty() && cores.iter().all(|&t| ops::state(cx, t).0 == TargetState::Halted)
}

/// Erase-checks the sectors with the DMU (no read: reading erased PFLASH raises ECC errors).
fn erase_states(cx: &Cx, variant: u32, addrs: &[u32]) -> Result<Erased> {
    let mut out = Erased::new();
    for &a in addrs {
        let Some(bank) = flash::pflash_at(variant, a) else { continue };
        let sector = sector_of(a);
        let erased = cx.with(|c| -> Result<bool> {
            let mut io = c.io()?;
            flash::Driver::new(&mut io, bank).is_erased(sector, 1)
        })?;
        out.insert(sector, erased);
    }
    Ok(out)
}

fn images_load(files: &[crate::image::ImageFile]) -> Result<Option<Image>> {
    if files.is_empty() {
        return Ok(None);
    }
    let mut img = Image::default();
    for f in files {
        img = img.overlay(&f.load().map_err(other)?).0;
    }
    Ok(Some(img))
}

/// Reviews the staged edits (or the UCB records of `images`) against a fresh readout.
pub fn review(cx: &Cx, images: &[crate::image::ImageFile]) -> Result<Review> {
    let r = refresh(cx)?;
    let img = images_load(images)?;
    let staged = cx.with(|c| c.ucb.staged.clone());
    let (changes, notes) = plan(&r, &staged, img.as_ref())?;
    let halted = all_halted(cx);
    let addrs = flash_addresses(&changes);
    let erased = if halted && !addrs.is_empty() { Some(erase_states(cx, r.device.variant, &addrs)?) } else { None };
    let (problems, passed) = run_checks(cx, &r, &changes, img.as_ref(), erased.as_ref());
    let risk = changes.iter().map(|c| change_risk(&r, c)).max().unwrap_or(Risk::Reversible);
    let mut h = 0xCBF2_9CE4_8422_2325u64;
    for ch in &changes {
        h = fnv(ch.def.to_le_bytes(), h);
        h = fnv(ch.new, h);
        for (x, old, _) in &ch.targets {
            h = fnv([*x], h);
            h = fnv(*old, h);
        }
    }
    h = fnv(images.iter().flat_map(|f| f.path.bytes()), h);
    let rv = Review {
        token: format!("{h:016x}"),
        changes,
        problems,
        passed,
        notes,
        risk,
        images: images.to_vec(),
        device_name: r.device.name.clone(),
        checks_deferred: erased.is_none() && !addrs.is_empty(),
    };
    cx.with(|c| c.ucb.review = Some(rv.clone()));
    Ok(rv)
}

fn run_checks(cx: &Cx, r: &Readout, changes: &[Change], img: Option<&Image>, erased: Option<&Erased>) -> (Vec<Problem>, Vec<String>) {
    let (pwmap, dbgpw, since): (Vec<&'static str>, Option<[u32; 8]>, BTreeSet<usize>) =
        cx.with(|c| (c.ucb.passwords.keys().copied().collect(), c.adapter_settings.debug_password, c.ucb.written_since_reset.clone()));
    let known = move |d: &UcbDef| {
        let key = if d.name.starts_with("BMHD") { "BMHD0" } else { d.name };
        pwmap.contains(&key) || (d.name == "DBG" && dbgpw.is_some())
    };
    check(r, changes, &Env { image: img, erased, password_known: &known, written_since_reset: &since })
}

/// The review as JSON: changes by field, problems first, the confirmation needed.
pub fn review_json(r: &Readout, rv: &Review) -> Value {
    let changes: Vec<Value> = rv
        .changes
        .iter()
        .map(|ch| {
            let d = &UCBS[ch.def];
            let ws = words(d, &r.device);
            let old = r.data[d.orig as usize];
            let mut fields = Vec::new();
            for w in &ws {
                let (o, n) = (get32(&old, w.off), get32(&ch.new, w.off));
                if o == n {
                    continue;
                }
                if w.secret {
                    fields.push(json!({ "word": w.name, "label": if w.name.starts_with("PW") { "Password" } else { "secret word" }, "from": "(hidden)", "to": "(hidden)" }));
                    continue;
                }
                for f in &w.fields {
                    if f.get(o) != f.get(n) {
                        fields.push(json!({ "word": w.name, "field": f.name, "label": f.label, "from": f.text(f.get(o)), "to": f.text(f.get(n)) }));
                    }
                }
            }
            fields.dedup_by(|a, b| a["label"] == "Password" && b["label"] == "Password");
            json!({
                "ucb": d.name,
                "risk": change_risk(r, ch).key(),
                "risk_text": change_risk(r, ch).text(),
                "targets": ch.targets.iter().map(|(x, _, m)| json!({ "index": x, "addr": format!("0x{:08X}", ucb::ucb_addr(*x)), "method": if *m == Method::Rewrite { "erase and program" } else { "program" } })).collect::<Vec<_>>(),
                "fields": fields,
            })
        })
        .collect();
    let level = |l: Level| match l {
        Level::Block => "block",
        Level::Warn => "warn",
        Level::Info => "info",
    };
    json!({
        "token": rv.token,
        "device": rv.device_name,
        "changes": changes,
        "problems": rv.problems.iter().map(|p| json!({ "level": level(p.level), "ucb": p.ucb, "title": p.title, "detail": p.detail })).collect::<Vec<_>>(),
        "passed": rv.passed,
        "notes": rv.notes,
        "blocked": rv.blocked(),
        "risk": rv.risk.key(),
        "confirm": match rv.risk {
            Risk::Reversible => json!({ "level": "ack" }),
            Risk::Password => json!({ "level": "password" }),
            Risk::Permanent => json!({ "level": "type", "text": rv.device_name }),
        },
        "checks_deferred": rv.checks_deferred,
        "images": rv.images.iter().map(|f| f.path.clone()).collect::<Vec<_>>(),
    })
}

// --- Compare with image files ------------------------------------------------------------------

/// `ucb compare`: the device UCBs against the UCB records of image files, field by field. With
/// `files`, loads them and reads the device; without, compares again with the files loaded last
/// (after `ucb apply`, to show what is taken).
pub fn compare(cx: &Cx, files: Option<&[crate::image::ImageFile]>) -> Result<Value> {
    let (r, cmp) = match files {
        Some(f) if !f.is_empty() => {
            let img = images_load(f)?.unwrap_or_default();
            let r = refresh(cx)?;
            let cmp = CompareState { files: f.to_vec(), image: img };
            cx.with(|c| c.ucb.compare = Some(cmp.clone()));
            (r, cmp)
        }
        _ => {
            let cmp = cx.with(|c| c.ucb.compare.clone()).ok_or_else(|| usage("no image files to compare with: `ucb compare <file> ...`"))?;
            (readout(cx)?, cmp)
        }
    };
    let staged = cx.with(|c| c.ucb.staged.clone());
    Ok(compare_json(&r, &staged, &cmp))
}

fn secret_text(readable: bool, v: u32) -> &'static str {
    if !readable {
        "unreadable"
    } else if v == 0 {
        "zero"
    } else {
        "set"
    }
}

/// The comparison as JSON: per UCB whether the files hold it, and per field device and file value.
pub fn compare_json(r: &Readout, staged: &Staged, cmp: &CompareState) -> Value {
    let mut ucbs = Vec::new();
    let (mut in_file, mut differ, mut applicable) = (0, 0, 0);
    for (i, d) in UCBS.iter().enumerate() {
        let read_only = match d.access {
            Access::ReadOnly(why) => Some(why),
            Access::Writable => None,
        };
        let Some(fu) = image_ucb(r, d, &cmp.image) else {
            ucbs.push(json!({ "name": d.name, "group": d.class.group(), "class": d.class.key(), "in_file": "none", "differs": 0, "taken": 0, "read_only_reason": read_only }));
            continue;
        };
        in_file += 1;
        let st = staged.get(&i);
        let dev_readable = r.fully_readable(d.orig);
        let mut wj = Vec::new();
        let (mut n_diff, mut n_taken) = (0, 0);
        for w in words(d, &r.device) {
            let readable = r.readable(d.orig, w.off);
            let (dv, fv) = (r.word(d.orig, w.off), get32(&fu.content, w.off));
            let sv = st.map(|s| get32(s, w.off));
            let fields: Vec<Value> = w
                .fields
                .iter()
                .map(|f| {
                    let (a, b) = (f.get(dv), f.get(fv));
                    let auto = matches!(f.kind, Kind::Auto(_));
                    let differs = !readable || a != b;
                    let taken = differs && sv.is_some_and(|s| f.get(s) == b);
                    if differs && !auto {
                        n_diff += 1;
                        if taken {
                            n_taken += 1;
                        }
                    }
                    let mut j = f.to_json();
                    let m = j.as_object_mut().unwrap();
                    if w.secret {
                        m.insert("dev_text".into(), json!(secret_text(readable, a)));
                        m.insert("file_text".into(), json!(secret_text(true, b)));
                    } else {
                        m.insert("dev".into(), json!(a));
                        m.insert("file".into(), json!(b));
                        m.insert("dev_text".into(), json!(if readable { f.text(a) } else { "unreadable".into() }));
                        m.insert("file_text".into(), json!(f.text(b)));
                    }
                    m.insert("differs".into(), json!(differs));
                    m.insert("taken".into(), json!(taken));
                    j
                })
                .collect();
            wj.push(json!({ "name": w.name, "off": format!("0x{:03X}", w.off), "secret": w.secret, "raw": w.raw, "fields": fields }));
        }
        let mut notes: Vec<Value> = fu.notes(d).into_iter().map(|t| json!({ "level": "info", "text": t })).collect();
        if let Some(why) = read_only {
            notes.push(json!({ "level": "info", "text": format!("{why}. Infineon data in an image file usually comes from another chip or a template: shown for information, it cannot be applied.") }));
        }
        if !dev_readable {
            notes.push(json!({ "level": "warn", "text": "The device content is not fully readable: unlock the UCB first to compare it." }));
        }
        if d.class == Class::Boot && (get32(&fu.content, 0) != 0 || get32(&fu.content, 4) != 0) {
            let ok = ucb::bmhd_valid(&fu.content);
            notes.push(json!({ "level": if ok { "info" } else { "warn" }, "text": if ok { "The file's header ID and checksum are what the SSW checks." } else { "The file's boot header is not valid as it stands (header ID, mode or checksum); applied, the checksum is computed." } }));
        }
        let rsv: Vec<String> = ucb::reserved_offsets(&words(d, &r.device)).into_iter().filter(|&o| get32(&fu.content, o) != 0).map(|o| format!("+0x{o:03X}")).collect();
        if !rsv.is_empty() {
            notes.push(json!({ "level": "warn", "text": format!("The file has data in reserved words ({}): the review blocks it.", rsv.join(", ")) }));
        }
        if n_diff > 0 {
            differ += 1;
            if read_only.is_none() {
                applicable += 1;
            }
        }
        ucbs.push(json!({
            "name": d.name, "group": d.class.group(), "class": d.class.key(), "in_file": fu.in_file,
            "differs": n_diff, "taken": n_taken, "read_only_reason": read_only, "conflict": fu.conflict,
            "covered": fu.covered, "notes": notes, "words": wj,
        }));
    }
    json!({
        "files": cmp.files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(),
        "device": r.device.name,
        "time": r.time,
        "in_file": in_file,
        "differ": differ,
        "applicable": applicable,
        "ucbs": ucbs,
    })
}

/// `ucb apply all | <UCB> | <UCB>.<word>[.<field>] | <UCB>.PW`: stages values from the image files
/// of `ucb compare`. A UCB takes the file's whole content; a field only that field. Boot mode
/// header IDs and checksums are computed. Nothing is written.
pub fn apply(cx: &Cx, target: &str) -> Result<String> {
    take(cx, target, false)
}

/// `ucb revert <UCB> | <UCB>.<word>[.<field>] | <UCB>.PW | all`: the counterpart of [`apply`] -
/// puts staged values back to the device's (e.g. a password, whose value the views never show).
pub fn revert(cx: &Cx, target: &str) -> Result<String> {
    take(cx, target, true)
}

fn take(cx: &Cx, target: &str, from_device: bool) -> Result<String> {
    let cmp = cx.with(|c| c.ucb.compare.clone()).ok_or_else(|| usage("no image files to apply from: `ucb compare <file> ...` first"))?;
    let r = readout(cx)?;
    let mut done = Vec::new();
    let mut skipped = Vec::new();
    let targets: Vec<(usize, Option<String>)> = if target.eq_ignore_ascii_case("all") {
        (0..UCBS.len()).map(|i| (i, None)).collect()
    } else if target.contains('.') {
        let spec = parse_spec(&r.device, target)?;
        vec![(spec.def, Some(target.to_string()))]
    } else {
        vec![(ucb::find(target).ok_or_else(|| usage(format!("unknown UCB '{target}'")))?, None)]
    };
    let all = target.eq_ignore_ascii_case("all");
    for (i, field) in targets {
        let d = &UCBS[i];
        let Some(mut fu) = image_ucb(&r, d, &cmp.image) else {
            if !all {
                return Err(usage(format!("the image files hold nothing for UCB_{}", d.name)));
            }
            continue;
        };
        if from_device {
            fu.content = r.data[d.orig as usize];
        }
        if !from_device && fu.content == r.data[d.orig as usize] && field.is_none() && d.copy.is_none_or(|x| r.data[x as usize] == ucb::content_for(d, x, &fu.content)) {
            continue;
        }
        if let Access::ReadOnly(why) = d.access {
            if all {
                skipped.push(format!("UCB_{}: {why}", d.name));
                continue;
            }
            return Err(usage(format!("UCB_{}: {why}", d.name)));
        }
        if !r.fully_readable(d.orig) {
            let m = format!("UCB_{} is not readable: unlock it first", d.name);
            if all {
                skipped.push(m);
                continue;
            }
            return Err(usage(m));
        }
        let mut content = match &field {
            None => fu.content,
            Some(spec) => {
                let mut c = cx.with(|c| c.ucb.staged.get(&i).copied()).unwrap_or(r.data[d.orig as usize]);
                let s = parse_spec(&r.device, spec)?;
                if s.password {
                    for k in 0..8 {
                        put32(&mut c, PW + 4 * k, get32(&fu.content, PW + 4 * k));
                    }
                } else {
                    let ws = words(d, &r.device);
                    let w = &ws[s.word.unwrap()];
                    match s.field {
                        Some(fi) => {
                            let f = &w.fields[fi];
                            if matches!(f.kind, Kind::Auto(_)) {
                                return Err(usage(format!("{spec} is computed when the UCB is written")));
                            }
                            let v = f.put(get32(&c, w.off), f.get(get32(&fu.content, w.off)));
                            put32(&mut c, w.off, v);
                        }
                        None => {
                            // The whole word: its fields, the computed ones follow below.
                            let mut v = get32(&c, w.off);
                            for f in w.fields.iter().filter(|f| !matches!(f.kind, Kind::Auto(_))) {
                                v = f.put(v, f.get(get32(&fu.content, w.off)));
                            }
                            put32(&mut c, w.off, v);
                        }
                    }
                }
                c
            }
        };
        if d.class == Class::Boot {
            fix_bmhd(&mut content);
        }
        cx.with(|c| {
            if content == r.data[d.orig as usize] && d.copy.is_none_or(|x| r.data[x as usize] == ucb::content_for(d, x, &content)) {
                c.ucb.staged.remove(&i);
            } else {
                c.ucb.staged.insert(i, content);
            }
            c.ucb.review = None;
        });
        done.push(field.unwrap_or_else(|| format!("UCB_{}", d.name)));
    }
    let mut out = if done.is_empty() {
        "nothing to apply".to_string()
    } else if from_device {
        format!("back to the device values: {}", done.join(", "))
    } else {
        format!("staged from the file: {}", done.join(", "))
    };
    if !skipped.is_empty() {
        out += &format!("\nnot applied: {}", skipped.join("; "));
    }
    Ok(out)
}

// --- DMU ---------------------------------------------------------------------------------------

const UCB_BUSY_BIT: u32 = 0; // UCBs are part of DFLASH0 (D0BUSY).
const KEY_PAGE_MODE_DFLASH: u32 = 0x5D;
const KEY_RESUME_PROTECTION: u32 = 0xF5;
const DISABLE_PROTECTION: u32 = flash::CMD + 0x553C;
const UCB_TIMEOUT: Duration = Duration::from_secs(5);

/// The DMU command sequences on the UCBs (UM 6.5.2.2.3).
pub struct Dmu<'a, 'b> {
    pub io: &'a mut Io<'b>,
}

impl Dmu<'_, '_> {
    fn status(&mut self) -> Result<(u32, u32)> {
        let v = self.io.read32_many(&[DMU_HF_STATUS, DMU_HF_ERRSR])?;
        Ok((v[0], v[1] & ERR_MASK))
    }
    fn begin(&mut self) -> Result<()> {
        let (st, _) = self.status()?;
        if st & (1 << UCB_BUSY_BIT) != 0 {
            return Err(other("DFLASH0 is busy with another operation"));
        }
        self.io.write32(KEY, KEY_CLEAR_STATUS)
    }
    fn wait(&mut self) -> Result<u32> {
        let t0 = Instant::now();
        loop {
            let (st, err) = self.status()?;
            if st & (1 << UCB_BUSY_BIT) == 0 {
                return Ok(err);
            }
            if t0.elapsed() > UCB_TIMEOUT {
                let _ = self.io.write32(KEY, KEY_RESET_TO_READ);
                return Err(Error::Timeout("UCB operation timed out".into()));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn fail(&mut self, what: String, err: u32) -> Error {
        let _ = self.io.write32(KEY, if err & (ERR_SQER | ERR_PROER) != 0 { KEY_RESET_TO_READ } else { KEY_CLEAR_STATUS });
        other(format!("{what} failed: {}", flash::err_text(err)))
    }
    /// Erases one UCB (one logical sector; the UM allows only one per command).
    pub fn erase(&mut self, index: u8) -> Result<()> {
        let a = ucb::ucb_addr(index);
        self.begin()?;
        self.io.write32_seq(&[(SEQ_ADDR, a), (SEQ_COUNT, 1), (SEQ_CMD, 0x80), (SEQ_CMD, 0x50)])?;
        let err = self.wait()?;
        if err != 0 {
            return Err(self.fail(format!("erasing UCB{index}"), err));
        }
        Ok(())
    }
    /// Programs one 8-byte page with Write Page in DFLASH page mode.
    pub fn program_page(&mut self, addr: u32, data: &[u8]) -> Result<()> {
        self.begin()?;
        let mut b = tas::Batch::new();
        b.write32(KEY as u64, KEY_PAGE_MODE_DFLASH);
        b.write(LOAD_PAGE as u64, data);
        b.write32(SEQ_ADDR as u64, addr);
        b.write32(SEQ_COUNT as u64, 0);
        b.write32(SEQ_CMD as u64, 0xA0);
        b.write32(SEQ_CMD as u64, 0xAA);
        let res = self.io.batch(&b)?;
        for r in &res {
            crate::adapter::check(r, addr as u64, true)?;
        }
        let err = self.wait()?;
        if err != 0 {
            return Err(self.fail(format!("programming 0x{addr:08X}"), err));
        }
        Ok(())
    }
    /// Programs the pages of `new` that differ from `old` (erased or allowed over-programs).
    pub fn program(&mut self, index: u8, old: &Content, new: &Content) -> Result<usize> {
        let base = ucb::ucb_addr(index);
        let mut n = 0;
        for p in (0..SIZE).step_by(8) {
            if old[p..p + 8] != new[p..p + 8] && new[p..p + 8].iter().any(|&b| b != 0) {
                self.program_page(base + p as u32, &new[p..p + 8])?;
                n += 1;
            }
        }
        Ok(n)
    }
    /// Disable Protection with the password of a UCB (UC code); a wrong password sets PROER and
    /// blocks further attempts until the next reset.
    pub fn disable_protection(&mut self, uc: u8, pw: &[u32; 8]) -> Result<()> {
        self.begin()?;
        let mut seq = vec![(DISABLE_PROTECTION, uc as u32)];
        seq.extend(pw.iter().map(|&w| (DISABLE_PROTECTION, w)));
        self.io.write32_seq(&seq)?;
        let err = self.wait()?;
        if err != 0 {
            let e = self.fail("Disable Protection".into(), err);
            return Err(if err & ERR_PROER != 0 { other("wrong password: the DMU refuses further attempts until the next reset") } else { e });
        }
        Ok(())
    }
    pub fn resume_protection(&mut self) -> Result<()> {
        self.io.write32(KEY, KEY_RESUME_PROTECTION)
    }
}

/// `ucb unlock <UCB>`: Disable Protection with the session password, so a CONFIRMED UCB can be
/// read (until Resume Protection or a reset).
pub fn unlock(cx: &Cx, name: &str) -> Result<String> {
    let i = ucb::find(name).ok_or_else(|| usage(format!("unknown UCB '{name}'")))?;
    let d = &UCBS[i];
    let uc = match (d.uc, d.class) {
        (Some(uc), _) => uc,
        (None, Class::Boot) => 0,
        _ => return Err(usage(format!("UCB_{} has no password", d.name))),
    };
    let dbg = cx.with(|c| c.adapter_settings.debug_password);
    let pw = cx.with(|c| c.ucb.password(d, dbg)).ok_or_else(|| usage(format!("no password for UCB_{}: `ucb password {} <PW0> .. <PW7>`", d.name, d.name)))?;
    cx.with(|c| Dmu { io: &mut c.io()? }.disable_protection(uc, &pw))?;
    refresh(cx)?;
    Ok(format!("UCB_{}: protection disabled until `ucb lock` or the next reset", d.name))
}

/// `ucb lock`: Resume Protection.
pub fn lock(cx: &Cx) -> Result<String> {
    cx.with(|c| Dmu { io: &mut c.io()? }.resume_protection())?;
    refresh(cx)?;
    Ok("protection resumed".into())
}

// --- Write -------------------------------------------------------------------------------------

pub struct WriteOptions<'a> {
    pub token: &'a str,
    /// The device name, typed by the user for permanent changes.
    pub confirm: Option<&'a str>,
    pub reset: bool,
}

/// Writes the reviewed changes. See the module documentation for the steps.
pub fn write(cx: &Cx, opt: &WriteOptions) -> Result<Value> {
    let (enabled, origin) = cx.with(|c| (c.ucb.write_enabled, c.tcl_origin));
    if origin == Origin::Api {
        return Err(usage("UCBs are written only on a person's confirmation: from VS Code or the Tcl console, not through the REST/MCP API"));
    }
    if !enabled {
        return Err(usage("UCB writing is off: enable it with \"ucbWrite\": true in the launch configuration (or `ucb_write` in a hub profile)"));
    }
    if cx.with(|c| c.adapter.as_deref_mut().is_some_and(|a| a.flash_download().is_some())) {
        return Err(usage("this adapter programs flash from its own download list; UCBs can only be written through TAS or the BlueBox"));
    }
    let rv = cx.with(|c| c.ucb.review.clone()).ok_or_else(|| usage("no review: run `ucb review` first"))?;
    if rv.token != opt.token {
        return Err(usage("the token does not match the last review (something changed): review again"));
    }
    if rv.changes.is_empty() {
        return Ok(json!({ "written": [], "message": "nothing to write" }));
    }
    if rv.blocked() {
        return Err(usage("the review found blocking problems"));
    }
    if rv.risk == Risk::Permanent && opt.confirm != Some(rv.device_name.as_str()) {
        return Err(usage(format!("permanent changes: confirm with -confirm {}", rv.device_name)));
    }
    flash::halt_all(cx)?;
    let r = refresh(cx)?;
    for ch in &rv.changes {
        for (x, old, _) in &ch.targets {
            if r.data[*x as usize] != *old {
                return Err(other(format!("UCB{x} changed since the review: review again")));
            }
        }
    }
    // The checks that need the flash, now that the cores are halted.
    let img = images_load(&rv.images)?;
    let addrs = flash_addresses(&rv.changes);
    let erased = erase_states(cx, r.device.variant, &addrs)?;
    let (problems, _) = run_checks(cx, &r, &rv.changes, img.as_ref(), Some(&erased));
    if let Some(p) = problems.iter().find(|p| p.level == Level::Block) {
        return Err(other(format!("UCB_{}: {} - {}", p.ucb, p.title, p.detail)));
    }
    let dir = cx.with(|c| c.ucb.dir());
    let backup = ucb::backup(&dir, &r)?;
    log_info!("ucb: backup of all UCBs saved to {}", backup.display());
    let dbg_pw = cx.with(|c| c.adapter_settings.debug_password);
    let mut done: Vec<Value> = Vec::new();
    let mut disabled = false;
    let t0 = Instant::now();
    let result = (|| -> Result<()> {
        for ch in &rv.changes {
            let d = &UCBS[ch.def];
            if r.state(d) == State::Confirmed && !r.prodis(d) && (d.has_password() || d.class == Class::Boot) {
                let pw = cx.with(|c| c.ucb.password(d, dbg_pw)).ok_or_else(|| other(format!("no password for UCB_{}", d.name)))?;
                cx.with(|c| Dmu { io: &mut c.io()? }.disable_protection(d.uc.unwrap_or(0), &pw))?;
                disabled = true;
            }
            // COPY first: ORIG keeps the old content until COPY holds the new one.
            let mut order: Vec<&(u8, Content, Method)> = ch.targets.iter().collect();
            order.sort_by_key(|(x, _, _)| if Some(*x) == d.copy { 0 } else { 1 });
            for (x, old, m) in order {
                let pages = cx.with(|c| -> Result<usize> {
                    let mut io = c.io()?;
                    let mut dmu = Dmu { io: &mut io };
                    let want = ch.content_for(*x);
                    if *m == Method::Rewrite {
                        dmu.erase(*x)?;
                        dmu.program(*x, &[0; SIZE], &want)
                    } else {
                        dmu.program(*x, old, &want)
                    }
                })?;
                let back = cx.with(|c| c.io()?.read(ucb::ucb_addr(*x), SIZE))?;
                if back != ch.content_for(*x) {
                    return Err(other(format!("UCB{x} (UCB_{}) reads back different from what was written", d.name)));
                }
                log_info!("ucb: UCB_{} UCB{x}: {} ({pages} pages), verified", d.name, if *m == Method::Rewrite { "erased and programmed" } else { "programmed" });
                done.push(json!({ "ucb": d.name, "index": x, "method": if *m == Method::Rewrite { "erase and program" } else { "program" }, "pages": pages }));
            }
            // Passwords that changed become the session's.
            if d.has_password() {
                let pw: Vec<u32> = (0..8).map(|i| get32(&ch.new, PW + 4 * i)).collect();
                if pw != (0..8).map(|i| get32(&r.data[d.orig as usize], PW + 4 * i)).collect::<Vec<_>>() {
                    let mut a = [0u32; 8];
                    a.copy_from_slice(&pw);
                    cx.with(|c| {
                        c.ucb.passwords.insert(d.name, a);
                        if d.name == "DBG" {
                            c.adapter_settings.debug_password = Some(a);
                        }
                    });
                }
            }
            if State::of_code(get32(&ch.new, d.confirm[0])) == State::Unlocked {
                cx.with(|c| c.ucb.written_since_reset.insert(ch.def));
            }
        }
        Ok(())
    })();
    if disabled {
        let _ = cx.with(|c| Dmu { io: &mut c.io()? }.resume_protection());
    }
    log_write(&dir, &r, &rv, &backup, &result);
    cx.with(|c| {
        c.ucb.review = None;
        c.ucb.staged.clear();
    });
    if let Err(e) = result {
        let _ = refresh(cx);
        return Err(other(format!("{e}\nWritten before the failure: {}. Backup: {}", if done.is_empty() { "nothing".into() } else { done.iter().map(|d| format!("{}/UCB{}", d["ucb"].as_str().unwrap_or(""), d["index"])).collect::<Vec<_>>().join(", ") }, backup.display())));
    }
    let elapsed = t0.elapsed().as_secs_f64();
    if opt.reset {
        ops::reset(cx, "halt")?;
    }
    let after = refresh(cx)?;
    let states: Vec<Value> = rv
        .changes
        .iter()
        .map(|ch| {
            let d = &UCBS[ch.def];
            let (o, c) = after.installed_states(d);
            json!({ "ucb": d.name, "state": after.state(d).name(), "installed_state": State::pair(o, c).name(), "matches": d.indices().all(|x| !after.fully_readable(x) || after.data[x as usize] == ch.content_for(x)) })
        })
        .collect();
    Ok(json!({ "written": done, "backup": backup.to_string_lossy(), "seconds": elapsed, "reset": opt.reset, "after": states }))
}

fn log_write(dir: &std::path::Path, r: &Readout, rv: &Review, backup: &std::path::Path, result: &Result<()>) {
    let mut line = format!(
        "{} {} uid={} backup={} result={}",
        ucb::stamp(ucb::Readout::clone(r).time),
        r.device.name,
        r.uid().unwrap_or_default(),
        backup.display(),
        match result {
            Ok(()) => "ok".to_string(),
            Err(e) => format!("failed: {e}"),
        }
    );
    for ch in &rv.changes {
        let d = &UCBS[ch.def];
        line += &format!(" | UCB_{} old={:016x} new={:016x}", d.name, fnv(r.data[d.orig as usize], 0xCBF2_9CE4_8422_2325), fnv(ch.new, 0xCBF2_9CE4_8422_2325));
    }
    let _ = std::fs::create_dir_all(dir);
    let path = dir.join("ucb-log.txt");
    use std::io::Write as _;
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            let _ = writeln!(f, "{line}");
        }
        Err(e) => log_warn!("ucb: cannot write the log {}: {e}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn readout() -> Readout {
        let device = ucb::Device::from_chip_id(0x1020_7083, &[], 3).unwrap();
        let mut data = vec![[0u8; SIZE]; ucb::COUNT];
        for d in UCBS {
            if let Some(c) = ucb::delivery_default(d) {
                for x in d.indices() {
                    data[x as usize] = c;
                }
            }
        }
        // A valid BMHD0 (as the iLLD default).
        let mut b = data[0];
        put32(&mut b, 0, 0xB359_00FE);
        // (LSENA3 set: fine here, the device content is not checked, only changes.)
        put32(&mut b, 4, 0xA000_0000);
        fix_bmhd(&mut b);
        data[0] = b;
        data[8] = b;
        // Confirm registers: everything UNLOCKED.
        Readout { device, data, unreadable: vec![[false; ucb::WORDS]; ucb::COUNT], confirm_regs: [0x5555_5555; 3], protect: 0, installed: BTreeMap::new(), time: 0 }
    }

    fn checks(r: &Readout, staged: &Staged, erased: Option<&Erased>) -> (Vec<Change>, Vec<Problem>) {
        let (ch, _) = plan(r, staged, None).unwrap();
        let since = BTreeSet::new();
        let (p, _) = check(r, &ch, &Env { image: None, erased, password_known: &|_| false, written_since_reset: &since });
        (ch, p)
    }

    fn blocks(p: &[Problem]) -> Vec<String> {
        p.iter().filter(|p| p.level == Level::Block).map(|p| p.title.clone()).collect()
    }

    #[test]
    fn bmhd_fix_matches_the_reference() {
        let r = readout();
        assert_eq!(get32(&r.data[0], 8), 0x3179_5570);
        assert!(ucb::bmhd_valid(&r.data[0]));
    }

    #[test]
    fn writing_an_empty_ucb_needs_no_erase() {
        let r = readout();
        let i = ucb::find("BMHD1").unwrap();
        let mut c = r.data[1];
        // Lockstep CPU0..2 only: LSENA3 is reserved on the 3-core TC37x.
        put32(&mut c, 0, 0x007E);
        put32(&mut c, 4, 0xA000_0000);
        fix_bmhd(&mut c);
        let staged: Staged = [(i, c)].into();
        let (ch, p) = checks(&r, &staged, Some(&[(0xA000_0000, false)].into()));
        assert_eq!(ch[0].targets.iter().map(|t| t.2).collect::<Vec<_>>(), vec![Method::Program, Method::Program]);
        assert!(blocks(&p).is_empty(), "{p:?}");
        // Confirming it later is an over-program, not an erase.
        let mut r2 = readout();
        r2.data[1] = c;
        r2.data[9] = c;
        let mut cc = c;
        put32(&mut cc, CONF, CONFIRMED);
        let (ch, _) = plan(&r2, &[(i, cc)].into(), None).unwrap();
        assert!(ch[0].targets.iter().all(|t| t.2 == Method::Program));
    }

    #[test]
    fn hsm_boot_needs_hsm_code() {
        let r = readout();
        let i = ucb::find("HSMCOTP0").unwrap();
        let mut c = r.data[20];
        put32(&mut c, 0x18, 1);
        let (_, p) = checks(&r, &[(i, c)].into(), Some(&[(0xA000_0000, true)].into()));
        assert!(blocks(&p).iter().any(|t| t.contains("no HSM code")), "{p:?}");
        let (_, p) = checks(&r, &[(i, c)].into(), Some(&[(0xA000_0000, false)].into()));
        assert!(blocks(&p).is_empty(), "{p:?}");
        // Not halted: deferred to the write.
        let (_, p) = checks(&r, &[(i, c)].into(), None);
        assert!(p.iter().any(|p| p.level == Level::Info && p.title.contains("checked when writing")));
    }

    #[test]
    fn debug_lockout_and_confirmation_rules() {
        let r = readout();
        let i = ucb::find("DBG").unwrap();
        let mut c = r.data[18];
        put32(&mut c, 0, 1); // OCDSDIS without DBGIFLCK
        let (_, p) = checks(&r, &[(i, c)].into(), None);
        assert!(blocks(&p).iter().any(|t| t.contains("OCDS disabled")));
        let mut c = r.data[18];
        put32(&mut c, CONF, 0x1234_5678);
        let (_, p) = checks(&r, &[(i, c)].into(), None);
        assert!(blocks(&p).iter().any(|t| t.contains("Invalid confirmation")));
        // Removing every valid boot header is refused.
        let b = ucb::find("BMHD0").unwrap();
        let (_, p) = checks(&r, &[(b, ucb::delivery_default(&UCBS[b]).unwrap())].into(), None);
        assert!(blocks(&p).iter().any(|t| t.contains("No valid boot mode header")));
        // Infineon data is read only.
        let s = ucb::find("SSW").unwrap();
        let mut c = r.data[4];
        c[0] = 1;
        let (_, p) = checks(&r, &[(s, c)].into(), None);
        assert!(blocks(&p).iter().any(|t| t.contains("Read-only")));
    }

    #[test]
    fn swap_copy_holds_its_own_addresses() {
        let r = readout();
        let i = ucb::find("SWAP").unwrap();
        let mut c = r.data[23];
        put32(&mut c, 0, 0x55);
        put32(&mut c, 4, 0xAF40_2E00);
        put32(&mut c, 8, CONFIRMED);
        put32(&mut c, 12, 0xAF40_2E08);
        let (ch, p) = checks(&r, &[(i, c)].into(), None);
        let copy = ch[0].content_for(31);
        assert_eq!((get32(&copy, 4), get32(&copy, 12)), (0xAF40_3E00, 0xAF40_3E08));
        assert!(ch[0].targets.iter().all(|t| t.2 == Method::Program), "appending an entry needs no erase");
        assert!(!blocks(&p).iter().any(|t| t.contains("malformed")), "{p:?}");
    }

    #[test]
    fn warnings_only_for_what_changes() {
        let mut r = readout();
        put32(&mut r.data[20], 0x18, 3); // HSM boots, CPU0 waits: already on the device
        r.data[28] = r.data[20];
        let i = ucb::find("HSMCOTP0").unwrap();
        let mut c = r.data[20];
        put32(&mut c, 4, 0); // BOOTSEL unchanged (0), only HSMDX set
        put32(&mut c, 0x18, 3 | 4);
        let (_, p) = checks(&r, &[(i, c)].into(), Some(&[(0xA000_0000, false)].into()));
        assert!(!p.iter().any(|p| p.title.contains("CPU0 waits")), "{p:?}");
    }

    #[test]
    fn reserved_bits_of_missing_sectors() {
        let mut r = readout();
        r.device.pf_sectors = vec![128, 192];
        let i = ucb::find("PFLASH").unwrap();
        let mut c = r.data[16];
        put32(&mut c, 0x010, 1); // PF0 S128: not on a 2 MB bank
        let (_, p) = checks(&r, &[(i, c)].into(), None);
        assert!(blocks(&p).iter().any(|t| t.contains("Reserved")), "{p:?}");
    }

    #[test]
    fn otp_over_confirmed_sets() {
        let mut r = readout();
        r.confirm_regs[2] = 0x5555_5556; // OTP0 ORIG CONFIRMED
        let i = ucb::find("OTP1").unwrap();
        let mut c = r.data[33];
        put32(&mut c, 0, 0xFFFF_FFFF);
        put32(&mut r.data[33], 0, 1); // programmed already: needs an erase
        let (_, p) = checks(&r, &[(i, c)].into(), None);
        assert!(blocks(&p).iter().any(|t| t.contains("no longer be erased")), "{p:?}");
    }
}
