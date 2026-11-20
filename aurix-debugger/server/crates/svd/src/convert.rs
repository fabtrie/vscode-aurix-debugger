//! The conversion of SVD elements into the model: the extension's TypeScript `SvdConverter`
//! (itself the peripheral viewer's `SVDParser`), step by step, so the result is the same, quirks
//! included (an empty `<access />` is read-only, a `dimIndex` it cannot read names the copies
//! `undefined`, ...).
//!
//! Peripherals are converted in document order as the stream delivers them. Where that order is
//! not enough - a peripheral derived from another, a register derived from another peripheral's,
//! two peripherals of one name, device defaults after the peripherals - the stream stops with
//! [`Fail::Full`] and the files are read again with every peripheral in memory, converted in the
//! order the TypeScript did.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use crate::xml::{self, El, Item, Tree};
use crate::{Access, Cluster, Device, EnumValue, Enumeration, Error, Field, Input, ModifiedWrite, Peripheral, ReadAction, Register, Str};

/// Why a conversion stopped.
pub(crate) enum Fail {
    /// The stream is not enough; convert with every peripheral in memory.
    Full,
    Err(Error),
}

impl From<Error> for Fail {
    fn from(e: Error) -> Self {
        Fail::Err(e)
    }
}

fn svd_err(msg: impl Into<String>) -> Fail {
    Fail::Err(Error::Svd(msg.into()))
}

/// More copies than this in one `dim` is not a register description.
const MAX_DIM: u64 = 1 << 20;

// --- Values ------------------------------------------------------------------------------------

/// JavaScript `parseInteger` of the peripheral viewer: `0b...`, `0x...`, leading decimal digits,
/// `#...` binary; nothing for anything else (text is not trimmed).
pub(crate) fn parse_integer(value: Option<&str>) -> Option<u64> {
    let v = value.filter(|v| !v.is_empty())?;
    let all = |s: &str, f: fn(&u8) -> bool| !s.is_empty() && s.as_bytes().iter().all(f);
    let prefix = |p: &str| v.get(..2).filter(|h| h.eq_ignore_ascii_case(p)).map(|_| &v[2..]);
    if let Some(b) = prefix("0b").filter(|b| all(b, |c| matches!(c, b'0' | b'1'))) {
        return u64::from_str_radix(b, 2).ok();
    }
    if let Some(h) = prefix("0x").filter(|h| all(h, u8::is_ascii_hexdigit)) {
        return u64::from_str_radix(h, 16).ok();
    }
    let leading = |s: &str, f: fn(&u8) -> bool| s.as_bytes().iter().take_while(|c| f(c)).count();
    let n = leading(v, u8::is_ascii_digit);
    if n > 0 {
        return v[..n].parse().ok();
    }
    if let Some(b) = v.strip_prefix('#') {
        let n = leading(b, |c| matches!(c, b'0' | b'1'));
        if n > 0 {
            return u64::from_str_radix(&b[..n], 2).ok();
        }
    }
    None
}

fn access_from(text: &str) -> Access {
    match text {
        "write-only" | "writeOnce" => Access::WriteOnly,
        "read-write" | "read-writeOnce" => Access::ReadWrite,
        _ => Access::ReadOnly,
    }
}

fn read_action_from(text: &str) -> Option<ReadAction> {
    match text {
        "clear" => Some(ReadAction::Clear),
        "set" => Some(ReadAction::Set),
        "modify" => Some(ReadAction::Modify),
        "modifyExternal" => Some(ReadAction::ModifyExternal),
        _ => None,
    }
}

fn modified_write_from(text: &str) -> Option<ModifiedWrite> {
    Some(match text.trim() {
        "oneToClear" => ModifiedWrite::OneToClear,
        "oneToSet" => ModifiedWrite::OneToSet,
        "oneToToggle" => ModifiedWrite::OneToToggle,
        "zeroToClear" => ModifiedWrite::ZeroToClear,
        "zeroToSet" => ModifiedWrite::ZeroToSet,
        "zeroToToggle" => ModifiedWrite::ZeroToToggle,
        "clear" => ModifiedWrite::Clear,
        "set" => ModifiedWrite::Set,
        "modify" => ModifiedWrite::Modify,
        _ => return None,
    })
}

/// Whitespace of a JavaScript regular expression (`\s`).
fn js_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}

/// No `\r`, and each line break with the indentation after it one space.
fn cleanup_description(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().filter(|&c| c != '\r').peekable();
    while let Some(c) = chars.next() {
        if c == '\n' {
            out.push(' ');
            while chars.peek().is_some_and(|&c| js_space(c)) {
                chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn replace_first(s: &str, index: &str) -> String {
    s.replacen("%s", index, 1)
}

/// The names of the copies of a `dim` element (`dimIndex`, else 0, 1, ...).
fn parse_dim_index(spec: &str, count: u64) -> Result<Vec<String>, Fail> {
    let invalid = || svd_err("dimIndex Element has invalid specification.");
    if spec.contains(',') {
        let parts: Vec<String> = spec.split(',').map(|p| p.trim().to_string()).collect();
        if parts.len() as u64 != count {
            return Err(invalid());
        }
        return Ok(parts);
    }
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if let Some((a, b)) = spec.split_once('-').filter(|(a, b)| digits(a) && digits(b)) {
        let (Some(start), Some(end)) = (parse_integer(Some(a)), parse_integer(Some(b))) else { return Ok(Vec::new()) };
        if end < start {
            return Err(svd_err("dimIndex Element Range Spec invalid."));
        }
        if end - start + 1 < count {
            return Err(invalid());
        }
        return Ok((0..count).map(|i| (start + i).to_string()).collect());
    }
    let b = spec.as_bytes();
    if b.len() == 3 && b[0].is_ascii_alphabetic() && b[1] == b'-' && b[2].is_ascii_alphabetic() {
        if (b[2] as i64) - (b[0] as i64) + 1 < count as i64 {
            return Err(invalid());
        }
        return Ok((0..count).map(|i| char::from_u32(b[0] as u32 + i as u32).map(String::from).unwrap_or_default()).collect());
    }
    Ok(Vec::new())
}

struct Dim {
    count: u64,
    increment: u64,
    index: Vec<String>,
}

impl Dim {
    /// The name of copy `i`; `undefined` where the `dimIndex` named none, as in the extension.
    fn index(&self, i: u64) -> &str {
        self.index.get(i as usize).map_or("undefined", |s| s)
    }
}

fn dim_of(e: &El, kind: &str) -> Result<Dim, Fail> {
    let name = e.text_of("name").unwrap_or("");
    let count = parse_integer(e.text_of("dim")).filter(|&c| c >= 1).ok_or_else(|| svd_err(format!("Unable to parse SVD file: {kind} {name} has dim element, with no/invalid dimensions.")))?;
    if count > MAX_DIM {
        return Err(svd_err(format!("Unable to parse SVD file: {kind} {name} has {count} copies")));
    }
    let increment = match e.text_of("dimIncrement") {
        Some(t) if !t.is_empty() => parse_integer(Some(t)).unwrap_or(0),
        _ => 0,
    };
    if increment == 0 && count > 1 {
        return Err(svd_err(format!("Unable to parse SVD file: {kind} {name} has dim element, with no/invalid dimIncrement element.")));
    }
    let index = match e.text_of("dimIndex") {
        Some(spec) => parse_dim_index(spec, count)?,
        None => (0..count).map(|i| i.to_string()).collect(),
    };
    Ok(Dim { count, increment, index })
}

fn required<'e>(e: &'e El, tag: &str, what: &str) -> Result<&'e str, Fail> {
    e.text_of(tag).ok_or_else(|| svd_err(format!("Unable to parse SVD file: {what} without <{tag}>")))
}

// --- The conversion ----------------------------------------------------------------------------

struct Defaults {
    access: Access,
    size: u32,
    reset_value: u64,
}

struct Converter {
    strings: HashSet<Str, crate::FastHash>,
    /// Enumerations by their values; the strings are interned, so their addresses tell them
    /// apart.
    enumerations: HashMap<Vec<(u64, usize, usize)>, Arc<Enumeration>, crate::FastHash>,
    /// Enumerations by name, also by their paths (`FIELD.name`, `REG.FIELD.name`, ...).
    named: HashMap<String, Arc<Enumeration>>,
    /// The registers as written, by `parent.name` (TypeScript `peripheralRegisterMap`); only
    /// with every peripheral in memory, where a register may derive from another peripheral's.
    registers: Option<HashMap<String, Tree>>,
    /// Stand-ins for enumerations named before they are defined, by address, with the name;
    /// replaced at the end of the conversion.
    pending: HashMap<usize, (Arc<Enumeration>, String), crate::FastHash>,
    defaults: Defaults,
}

impl Converter {
    fn new(full: bool) -> Converter {
        Converter {
            strings: HashSet::default(),
            enumerations: HashMap::default(),
            named: HashMap::new(),
            registers: full.then(HashMap::new),
            pending: HashMap::default(),
            defaults: Defaults { access: Access::ReadWrite, size: 32, reset_value: 0 },
        }
    }

    fn str(&mut self, s: &str) -> Str {
        if let Some(x) = self.strings.get(s) {
            return x.clone();
        }
        let x: Str = s.into();
        self.strings.insert(x.clone());
        x
    }

    fn enumeration(&mut self, values: BTreeMap<u64, (Str, Str)>) -> Arc<Enumeration> {
        let key: Vec<(u64, usize, usize)> = values.iter().map(|(v, (n, d))| (*v, n.as_ptr() as usize, d.as_ptr() as usize)).collect();
        self.enumerations
            .entry(key)
            .or_insert_with(|| Arc::new(Enumeration { values: values.into_iter().map(|(value, (name, description))| EnumValue { value, name, description }).collect() }))
            .clone()
    }

    fn device_item(&mut self, e: El, seen: &mut HashSet<String>, device: &mut Device) {
        // The first of each counts (xml2js `device.size[0]`).
        if !seen.insert(e.tag().to_string()) {
            return;
        }
        match e.tag() {
            "name" => device.name = Some(self.str(e.text())),
            "size" => self.defaults.size = parse_integer(Some(e.text())).unwrap_or(32) as u32,
            "access" => self.defaults.access = access_from(e.text()),
            "resetValue" => self.defaults.reset_value = parse_integer(Some(e.text())).unwrap_or(0),
            _ => {}
        }
    }

    /// `scope`: the register's name and its parents', innermost first.
    fn fields(&mut self, field_els: &[El], scope: &[&str]) -> Result<Vec<Field>, Fail> {
        let mut fields = Vec::new();
        for f in field_els {
            let raw_name = f.text_of("name").ok_or_else(|| svd_err("Unable to parse SVD file: field without <name>"))?;
            let description = self.str(&cleanup_description(f.text_of("description").unwrap_or("")));
            let (offset, width): (i64, i64) = if f.has("bitOffset") && f.has("bitWidth") {
                (parse_integer(f.text_of("bitOffset")).unwrap_or(0) as i64, parse_integer(f.text_of("bitWidth")).unwrap_or(0) as i64)
            } else if let Some(r) = f.text_of("bitRange") {
                let inner = r.get(1..r.len().saturating_sub(1)).unwrap_or("");
                let mut parts = inner.split(':');
                let end = parse_integer(parts.next()).unwrap_or(0) as i64;
                let start = parse_integer(parts.next()).unwrap_or(0) as i64;
                (start, end - start + 1)
            } else if f.has("msb") && f.has("lsb") {
                let msb = parse_integer(f.text_of("msb")).unwrap_or(0) as i64;
                let lsb = parse_integer(f.text_of("lsb")).unwrap_or(0) as i64;
                (lsb, msb - lsb + 1)
            } else {
                return Err(svd_err(format!(
                    "Unable to parse SVD file: field {raw_name} must have either bitOffset and bitWidth elements, bitRange Element, or msb and lsb elements."
                )));
            };
            let bits = |v: i64| u32::try_from(v).map_err(|_| svd_err(format!("Unable to parse SVD file: field {raw_name} has an invalid bit range")));
            let (offset, width) = (bits(offset)?, bits(width)?);

            let mut enumeration = None;
            if let Some(ev) = f.first("enumeratedValues") {
                match ev.attr("derivedFrom").filter(|d| !d.is_empty()) {
                    Some(from) => match self.named.get(from) {
                        Some(found) => enumeration = Some(found.clone()),
                        None => {
                            let stand_in = Arc::new(Enumeration::default());
                            self.pending.insert(Arc::as_ptr(&stand_in) as usize, (stand_in.clone(), from.to_string()));
                            enumeration = Some(stand_in);
                        }
                    },
                    None => {
                        let mut values = BTreeMap::new();
                        for v in ev.all("enumeratedValue").filter(|v| v.has("value")) {
                            let value = parse_integer(v.text_of("value")).unwrap_or(0);
                            let name = v.text_of("name").ok_or_else(|| svd_err(format!("Unable to parse SVD file: an enumerated value of field {raw_name} has no <name>")))?;
                            let (name, description) = (self.str(name), self.str(&cleanup_description(v.text_of("description").unwrap_or(""))));
                            values.insert(value, (name, description));
                        }
                        let map = self.enumeration(values);
                        if let Some(n) = ev.text_of("name").filter(|n| !n.is_empty()) {
                            let mut key = n.to_string();
                            self.named.insert(key.clone(), map.clone());
                            key = format!("{raw_name}.{key}");
                            self.named.insert(key.clone(), map.clone());
                            for s in scope {
                                key = format!("{s}.{key}");
                                self.named.insert(key.clone(), map.clone());
                            }
                        }
                        enumeration = Some(map);
                    }
                }
            }
            let access = f.text_of("access").map(access_from);
            let read_action = f.text_of("readAction").and_then(read_action_from);
            let modified_write = f.text_of("modifiedWriteValues").and_then(modified_write_from);
            let field = |name: Str, offset: u32| Field {
                name,
                description: description.clone(),
                offset,
                width,
                enumeration: enumeration.clone(),
                access,
                read_action,
                modified_write,
            };
            if f.has("dim") {
                let dim = dim_of(f, "field")?;
                for i in 0..dim.count {
                    let name = self.str(&replace_first(raw_name, dim.index(i)));
                    fields.push(field(name, offset.wrapping_add((dim.increment * i) as u32)));
                }
            } else {
                let name = self.str(raw_name);
                fields.push(field(name, offset));
            }
        }
        Ok(fields)
    }

    /// `parent`: the peripheral's or cluster's name; `scope`: it and its parents, innermost
    /// first.
    fn registers(&mut self, els: &[El], parent: &str, scope: &[&str]) -> Result<Vec<Register>, Fail> {
        if let Some(g) = &mut self.registers {
            for r in els {
                g.insert(format!("{parent}.{}", r.text_of("name").unwrap_or("")), Tree::copy_of(*r));
            }
        }
        // Derived registers are merged with their base into trees of their own; the others are
        // read where they are.
        let derived = els.iter().any(|r| r.attr("derivedFrom").is_some_and(|d| !d.is_empty()));
        let mut list: Vec<Option<Tree>> = Vec::new();
        if derived {
            list = els.iter().map(|_| None).collect();
            let mut local: HashMap<&str, usize> = HashMap::new();
            for (i, r) in els.iter().enumerate() {
                local.insert(r.text_of("name").unwrap_or(""), i);
            }
            for (i, r) in els.iter().enumerate() {
                let Some(from) = r.attr("derivedFrom").filter(|d| !d.is_empty()) else { continue };
                let name = r.text_of("name").unwrap_or("");
                let combined = match local.get(from) {
                    Some(&j) => Tree::merged(list[j].as_ref().map_or(els[j], Tree::root), *r),
                    None => match &self.registers {
                        None => return Err(Fail::Full),
                        Some(g) => {
                            let base = g.get(from).ok_or_else(|| svd_err(format!("SVD error: Invalid 'derivedFrom' \"{from}\" for register \"{name}\"")))?;
                            Tree::merged(base.root(), *r)
                        }
                    },
                };
                if let Some(g) = &mut self.registers {
                    g.insert(format!("{parent}.{name}"), Tree::copy_of(combined.root()));
                }
                list[i] = Some(combined);
                local.insert(name, i);
            }
        }
        let regs: Vec<El> = els.iter().enumerate().map(|(i, r)| list.get(i).and_then(|t| t.as_ref()).map_or(*r, Tree::root)).collect();

        let mut registers = Vec::new();
        for r in regs {
            let access = r.text_of("access").map(access_from);
            let size = r.text_of("size").map(|t| parse_integer(Some(t)).unwrap_or(32) as u32);
            let reset_value = r.text_of("resetValue").map(|t| parse_integer(Some(t)).unwrap_or(0));
            let read_action = r.text_of("readAction").and_then(read_action_from);
            let modified_write = r.text_of("modifiedWriteValues").and_then(modified_write_from);
            let raw_name = r.text_of("name").ok_or_else(|| svd_err(format!("Unable to parse SVD file: a register of {parent} has no <name>")))?;
            // Exactly one <fields> (xml2js `r.fields.length === 1`).
            let field_els: Vec<El> = if r.all("fields").count() == 1 { r.first("fields").map(|f| f.all("field").collect()).unwrap_or_default() } else { Vec::new() };
            let register = |this: &mut Converter, name: String, description: String, address_offset: u64| -> Result<Register, Fail> {
                let inner: Vec<&str> = std::iter::once(name.as_str()).chain(scope.iter().copied()).collect();
                let fields = this.fields(&field_els, &inner)?;
                Ok(Register { name: this.str(&name), description: this.str(&description), address_offset, access, size, reset_value, read_action, modified_write, fields })
            };
            let description = cleanup_description(r.text_of("description").unwrap_or(""));
            if r.has("dim") {
                let dim = dim_of(&r, "register")?;
                let offset = required(&r, "addressOffset", "a register")?;
                let base = parse_integer(Some(offset)).ok_or_else(|| svd_err(format!("Unable to parse SVD file: register {raw_name} has invalid addressOffset {offset}")))?;
                for i in 0..dim.count {
                    let idx = dim.index(i);
                    let reg = register(self, replace_first(raw_name, idx), replace_first(&description, idx), base + dim.increment * i)?;
                    registers.push(reg);
                }
            } else {
                let offset = r.text_of("addressOffset").ok_or_else(|| svd_err(format!("Unable to parse SVD file: register {raw_name} has no addressOffset")))?;
                let offset = parse_integer(Some(offset)).ok_or_else(|| svd_err(format!("Unable to parse SVD file: register {raw_name} has invalid addressOffset {offset}")))?;
                let reg = register(self, raw_name.to_string(), description, offset)?;
                registers.push(reg);
            }
        }
        Ok(registers)
    }

    /// `scope`: the parent's name and its parents', innermost first.
    fn clusters(&mut self, els: &[El], scope: &[&str]) -> Result<Vec<Cluster>, Fail> {
        let mut clusters = Vec::new();
        for c in els {
            let access = c.text_of("access").map(access_from);
            let size = c.text_of("size").map(|t| parse_integer(Some(t)).unwrap_or(0) as u32);
            let reset_value = c.text_of("resetValue").map(|t| parse_integer(Some(t)).unwrap_or(0));
            let regs: Vec<El> = c.all("register").collect();
            let subs: Vec<El> = c.all("cluster").collect();
            let raw_name = c.text_of("name").ok_or_else(|| svd_err("Unable to parse SVD file: cluster without <name>"))?;
            let build = |this: &mut Converter, name: String, description: String, address_offset: u64| -> Result<Cluster, Fail> {
                let inner: Vec<&str> = std::iter::once(name.as_str()).chain(scope.iter().copied()).collect();
                let registers = this.registers(&regs, &name, &inner)?;
                let clusters = this.clusters(&subs, &inner)?;
                Ok(Cluster { name: this.str(&name), description: this.str(&description), address_offset, access, size, reset_value, registers, clusters })
            };
            let description = cleanup_description(c.text_of("description").unwrap_or(""));
            if c.has("dim") {
                let dim = dim_of(c, "cluster")?;
                let offset = required(c, "addressOffset", "a cluster")?;
                let base = parse_integer(Some(offset)).ok_or_else(|| svd_err(format!("Unable to parse SVD file: cluster {raw_name} has an no/invalid addressOffset")))?;
                for i in 0..dim.count {
                    let idx = dim.index(i);
                    let cl = build(self, replace_first(raw_name, idx), replace_first(&description, idx), base + dim.increment * i)?;
                    clusters.push(cl);
                }
            } else {
                let offset = parse_integer(c.text_of("addressOffset")).unwrap_or(0);
                let cl = build(self, raw_name.to_string(), description, offset)?;
                clusters.push(cl);
            }
        }
        Ok(clusters)
    }

    fn peripheral(&mut self, p: El) -> Result<Peripheral, Fail> {
        let name = p.text_of("name").ok_or_else(|| svd_err("Unable to parse SVD file: peripheral without <name>"))?.to_string();
        let mut total_length = 0;
        for ab in p.all("addressBlock") {
            if let (Some(o), Some(s)) = (parse_integer(ab.text_of("offset")), parse_integer(ab.text_of("size"))) {
                total_length = total_length.max(o + s);
            }
        }
        let base_text = p.text_of("baseAddress").unwrap_or("0");
        let base_address = parse_integer(Some(base_text)).ok_or_else(|| svd_err(format!("Unable to parse SVD file: peripheral {name} has invalid baseAddress {base_text}")))?;
        let access = p.text_of("access").map_or(self.defaults.access, access_from);
        let size = p.text_of("size").map_or(self.defaults.size, |t| parse_integer(Some(t)).map_or(self.defaults.size, |v| v as u32));
        let reset_value = p.text_of("resetValue").map_or(self.defaults.reset_value, |t| parse_integer(Some(t)).unwrap_or(self.defaults.reset_value));
        let group_name = p.text_of("groupName").map(|g| self.str(g));
        let description = self.str(&cleanup_description(p.text_of("description").unwrap_or("")));
        let regs_el = p.first("registers");
        let regs: Vec<El> = regs_el.map(|r| r.all("register").collect()).unwrap_or_default();
        let subs: Vec<El> = regs_el.map(|r| r.all("cluster").collect()).unwrap_or_default();
        let registers = self.registers(&regs, &name, &[name.as_str()])?;
        let clusters = self.clusters(&subs, &[name.as_str()])?;
        Ok(Peripheral { name: self.str(&name), base_address, total_length, description, group_name, access, size, reset_value, registers, clusters })
    }

    /// Sorts as the view does, then resolves the enumerations named before they were defined.
    fn finish(&mut self, mut device: Device) -> Result<Device, Error> {
        device.peripherals.sort_by(|a, b| {
            let (ga, gb) = (a.group_name.as_deref().unwrap_or(""), b.group_name.as_deref().unwrap_or(""));
            ga.cmp(gb).then_with(|| a.name.cmp(&b.name))
        });
        type Pending = HashMap<usize, (Arc<Enumeration>, String), crate::FastHash>;
        fn resolve(named: &HashMap<String, Arc<Enumeration>>, pending: &Pending, regs: &mut [Register], clusters: &mut [Cluster]) -> Result<(), Error> {
            for r in regs {
                for f in &mut r.fields {
                    let Some((_, from)) = f.enumeration.as_ref().and_then(|e| pending.get(&(Arc::as_ptr(e) as usize))) else { continue };
                    let map = named.get(from).ok_or_else(|| Error::Svd(format!("Invalid derivedFrom={from} for enumeratedValues of field {}", f.name)))?;
                    f.enumeration = Some(map.clone());
                }
            }
            for c in clusters {
                resolve(named, pending, &mut c.registers, &mut c.clusters)?;
            }
            Ok(())
        }
        if !self.pending.is_empty() {
            for p in &mut device.peripherals {
                resolve(&self.named, &self.pending, &mut p.registers, &mut p.clusters)?;
            }
        }
        Ok(device)
    }
}

/// Whether an extra file's peripheral `name` is merged: not if it excludes it. A name already
/// present (the device's, or an earlier extra's) is an error unless the extra replaces it.
fn wanted(input: &Input, name: &str, present: &mut HashSet<String>) -> Result<bool, Error> {
    let name = name.trim();
    if input.exclude.iter().any(|x| x == name) {
        return Ok(false);
    }
    if present.contains(name) && !input.replace.iter().any(|x| x == name) {
        return Err(Error::Svd(format!("SVD merge: peripherals already defined by the device: {name}")));
    }
    present.insert(name.to_string());
    Ok(true)
}

/// Converts the device file and its extras (see [`crate::load`]).
pub(crate) fn convert(inputs: &[Input]) -> Result<Device, Error> {
    match stream(inputs) {
        Ok(d) => Ok(d),
        Err(Fail::Err(e)) => Err(e),
        Err(Fail::Full) => full(inputs),
    }
}

/// The device's peripherals the extras replace.
fn replaced(inputs: &[Input]) -> HashSet<String> {
    inputs[1..].iter().flat_map(|x| x.replace.iter().cloned()).collect()
}

fn stream(inputs: &[Input]) -> Result<Device, Fail> {
    let mut cv = Converter::new(false);
    let mut device = Device::default();
    let replace = replaced(inputs);
    // The peripherals so far (a merged one may not have their name).
    let mut present: HashSet<String> = HashSet::new();
    let mut names: HashSet<String> = HashSet::new();
    for (n, input) in inputs.iter().enumerate() {
        let mut seen_items = HashSet::new();
        let source = (input.open)().map_err(Error::Io)?;
        let had = xml::read(source, |item| -> Result<(), Fail> {
            match item {
                Item::Device(e) if n == 0 => {
                    if !device.peripherals.is_empty() && !seen_items.contains(e.root().tag()) {
                        return Err(Fail::Full);
                    }
                    cv.device_item(e.root(), &mut seen_items, &mut device);
                }
                Item::Device(_) => {}
                Item::Peripheral(t) => {
                    let p = t.root();
                    let name = p.text_of("name").unwrap_or("").to_string();
                    if n == 0 {
                        if replace.contains(name.trim()) {
                            return Ok(());
                        }
                        present.insert(name.trim().to_string());
                    } else if !wanted(input, &name, &mut present)? {
                        return Ok(());
                    }
                    if !names.insert(name) || p.attr("derivedFrom").is_some_and(|d| !d.is_empty()) {
                        return Err(Fail::Full);
                    }
                    let peripheral = cv.peripheral(p)?;
                    device.peripherals.push(peripheral);
                }
            }
            Ok(())
        })?;
        if n == 0 && !had {
            return Err(svd_err("Unable to parse SVD file: no peripherals defined"));
        }
    }
    Ok(cv.finish(device)?)
}

/// The conversion with every peripheral in memory, in the TypeScript's order: by name in first
/// appearance (a later one of the same name wins), derived peripherals merged with their base.
fn full(inputs: &[Input]) -> Result<Device, Error> {
    let mut cv = Converter::new(true);
    let mut device = Device::default();
    let replace = replaced(inputs);
    let mut present: HashSet<String> = HashSet::new();
    let mut order: Vec<(String, Tree)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut device_items: Vec<Tree> = Vec::new();
    for (n, input) in inputs.iter().enumerate() {
        let source = (input.open)()?;
        let had = xml::read::<Error>(source, |item| {
            match item {
                Item::Device(e) if n == 0 => device_items.push(e),
                Item::Device(_) => {}
                Item::Peripheral(p) => {
                    let name = p.root().text_of("name").unwrap_or("").to_string();
                    if n == 0 {
                        if replace.contains(name.trim()) {
                            return Ok(());
                        }
                        present.insert(name.trim().to_string());
                    } else if !wanted(input, &name, &mut present)? {
                        return Ok(());
                    }
                    match index.get(&name) {
                        Some(&i) => order[i].1 = p,
                        None => {
                            index.insert(name.clone(), order.len());
                            order.push((name, p));
                        }
                    }
                }
            }
            Ok(())
        })?;
        if n == 0 && !had {
            return Err(Error::Svd("Unable to parse SVD file: no peripherals defined".into()));
        }
    }
    let mut seen = HashSet::new();
    for e in &device_items {
        cv.device_item(e.root(), &mut seen, &mut device);
    }
    for i in 0..order.len() {
        let Some(from) = order[i].1.root().attr("derivedFrom").filter(|d| !d.is_empty()).map(str::to_string) else { continue };
        if let Some(&j) = index.get(&from) {
            let merged = Tree::merged(order[j].1.root(), order[i].1.root());
            order[i].1 = merged;
        }
    }
    for (_, p) in &order {
        let peripheral = match cv.peripheral(p.root()) {
            Ok(p) => p,
            Err(Fail::Err(e)) => return Err(e),
            Err(Fail::Full) => unreachable!("the full conversion has every register"),
        };
        device.peripherals.push(peripheral);
    }
    cv.finish(device)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_as_the_view_parses_them() {
        let p = |s: &str| parse_integer(Some(s));
        assert_eq!(p("0x1F"), Some(31));
        assert_eq!(p("0X1f"), Some(31));
        assert_eq!(p("0b101"), Some(5));
        assert_eq!(p("#101"), Some(5));
        assert_eq!(p("32"), Some(32));
        assert_eq!(p("12abc"), Some(12));
        assert_eq!(p("0x1000 "), Some(0), "not hex with the space: the leading digit");
        assert_eq!(p(" 32"), None, "not trimmed");
        assert_eq!(p(""), None);
        assert_eq!(parse_integer(None), None);
    }

    #[test]
    fn dim_indices() {
        assert_eq!(parse_dim_index("A,B, C", 3).ok().unwrap(), ["A", "B", "C"]);
        assert_eq!(parse_dim_index("3-6", 4).ok().unwrap(), ["3", "4", "5", "6"]);
        assert_eq!(parse_dim_index("a-c", 2).ok().unwrap(), ["a", "b"]);
        assert!(parse_dim_index("A,B", 3).is_err());
        assert!(parse_dim_index("3-4", 4).is_err());
        assert!(parse_dim_index("x", 2).ok().unwrap().is_empty(), "unreadable: copies named undefined");
    }

    /// Without anything that needs it, converting with every peripheral in memory gives what the
    /// stream gives.
    #[test]
    fn stream_and_full_agree() {
        let xml = include_str!("../tests/fixtures/stream.svd");
        let none: &[String] = &[];
        let inputs = [Input { open: Box::new(|| Ok(Box::new(xml.as_bytes()) as Box<dyn std::io::BufRead>)), replace: none, exclude: none }];
        let streamed = match stream(&inputs) {
            Ok(d) => d,
            Err(_) => panic!("the fixture reads as a stream"),
        };
        let all = full(&inputs).unwrap();
        let json = |d: &Device| serde_json::to_string(&crate::PeripheralOptions(&d.peripherals)).unwrap();
        assert_eq!(json(&streamed), json(&all));
    }

    #[test]
    fn descriptions() {
        assert_eq!(cleanup_description("a\r\n    b\n\n c"), "a b c");
    }
}

