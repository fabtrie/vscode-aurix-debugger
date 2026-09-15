//! DWARF symbol access for TriCore ELF files: functions, globals, line
//! tables, source file lookup, types, local variables and C expressions.
//!
//! Written against GHS C 2021 output (DWARF 2: no .debug_aranges, absolute
//! Windows paths in the line table, CU DIEs without PC ranges, type
//! references across units), which also covers GCC output.

pub mod expr;
pub mod scope;
pub mod types;

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use gimli::{AttributeValue, EndianSlice, LittleEndian, UnitOffset};
use memmap2::Mmap;
use object::{Object, ObjectSection};

pub type R = EndianSlice<'static, LittleEndian>;
pub type Die = gimli::DebuggingInformationEntry<R>;

/// Pseudo DWARF register number for the PC in expressions (`$pc`).
pub const PC_REGISTER: u16 = 32;

/// A DIE: unit index and offset within the unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DieRef {
    pub unit: usize,
    pub off: usize,
}

#[derive(Clone, Debug)]
pub struct Function {
    pub name: String,
    pub low: u32,
    pub high: u32,
    pub unit: usize,
    pub die: UnitOffset<usize>,
    pub decl_line: u32,
    pub external: bool,
}

impl Function {
    pub fn contains(&self, addr: u32) -> bool {
        addr >= self.low && addr < self.high
    }

    pub fn die_ref(&self) -> DieRef {
        DieRef { unit: self.unit, off: self.die.0 }
    }
}

#[derive(Clone, Debug)]
pub struct Global {
    pub name: String,
    pub addr: u32,
    pub unit: usize,
    pub die: UnitOffset<usize>,
    pub external: bool,
}

impl Global {
    pub fn die_ref(&self) -> DieRef {
        DieRef { unit: self.unit, off: self.die.0 }
    }
}

#[derive(Clone, Copy, Debug)]
struct Row {
    addr: u32,
    file: u32,
    line: u32,
    col: u32,
    end: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineInfo {
    pub file: String,
    pub line: u32,
    pub column: u32,
    pub start: u32,
    pub end: u32,
}

pub struct Unit {
    pub header: gimli::UnitHeader<R>,
    pub name: String,
    pub comp_dir: String,
}

pub struct Symbols {
    pub path: PathBuf,
    pub dwarf: gimli::Dwarf<R>,
    pub units: Vec<Unit>,
    /// Sorted by start address.
    pub functions: Vec<Function>,
    pub globals: Vec<Global>,
    functions_by_name: HashMap<String, Vec<usize>>,
    globals_by_name: HashMap<String, Vec<usize>>,
    /// Named types ("uint8", "struct foo", "enum bar"), a few definitions each.
    types_by_name: HashMap<String, Vec<DieRef>>,
    /// Enumerator name -> (enumerator, enumeration type).
    enumerators_by_name: HashMap<String, Vec<(DieRef, DieRef)>>,
    /// All line table rows, sorted by address (end-of-sequence rows first).
    rows: Vec<Row>,
    files: Vec<String>,
    files_norm: Vec<String>,
    rows_by_file: HashMap<u32, Vec<u32>>,
    unit_cache: Vec<OnceLock<Option<gimli::Unit<R>>>>,
    /// .debug_info offset of each unit (ascending).
    unit_offsets: Vec<usize>,
    pub load_time: Duration,
}

/// Index entries kept per name: enough to pick a definition, bounded for
/// typedefs repeated in every unit.
const NAME_INDEX_CAP: usize = 4;

fn err<E: std::fmt::Display>(what: &str) -> impl Fn(E) -> String + '_ {
    move |e| format!("{what}: {e}")
}

/// Lower-case, forward slashes, no `./` or leading `../` components.
pub fn normalize_path(p: &str) -> String {
    let s = p.replace('\\', "/").to_ascii_lowercase();
    let mut parts: Vec<&str> = Vec::new();
    for c in s.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|l| *l != "..") {
                    parts.pop();
                } else {
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let lead = parts.iter().take_while(|c| **c == "..").count();
    parts[lead..].join("/")
}

fn is_absolute(p: &str) -> bool {
    p.starts_with('/') || p.starts_with('\\') || (p.len() > 2 && p.as_bytes()[1] == b':')
}

fn join_path(dir: &str, file: &str) -> String {
    if dir.is_empty() || is_absolute(file) {
        return file.to_string();
    }
    let sep = if dir.contains('\\') { '\\' } else { '/' };
    let mut d = dir.to_string();
    if !d.ends_with('/') && !d.ends_with('\\') {
        d.push(sep);
    }
    d + file
}

fn is_declaration(e: &Die) -> bool {
    matches!(e.attr_value(gimli::DW_AT_declaration), Some(AttributeValue::Flag(true)))
}

impl Symbols {
    pub fn load(path: &Path) -> Result<Symbols, String> {
        let t0 = Instant::now();
        let file = File::open(path).map_err(err("cannot open ELF"))?;
        let mmap = unsafe { Mmap::map(&file) }.map_err(err("cannot map ELF"))?;
        // The symbol data lives for the rest of the process.
        let data: &'static [u8] = Box::leak(Box::new(mmap));
        let obj = object::File::parse(data).map_err(err("invalid ELF"))?;
        if !obj.is_little_endian() {
            return Err("big-endian ELF files are not supported".into());
        }
        let load = |id: gimli::SectionId| -> Result<R, gimli::Error> {
            let bytes: &'static [u8] = obj
                .section_by_name(id.name())
                .and_then(|s| s.data().ok())
                .unwrap_or(&[]);
            Ok(EndianSlice::new(bytes, LittleEndian))
        };
        let dwarf = gimli::Dwarf::load(load).map_err(err("DWARF"))?;

        let mut syms = Symbols {
            path: path.to_path_buf(),
            dwarf,
            units: Vec::new(),
            functions: Vec::new(),
            globals: Vec::new(),
            functions_by_name: HashMap::new(),
            globals_by_name: HashMap::new(),
            types_by_name: HashMap::new(),
            enumerators_by_name: HashMap::new(),
            rows: Vec::new(),
            files: Vec::new(),
            files_norm: Vec::new(),
            rows_by_file: HashMap::new(),
            unit_cache: Vec::new(),
            unit_offsets: Vec::new(),
            load_time: Duration::ZERO,
        };
        let mut file_ids: HashMap<String, u32> = HashMap::new();

        let mut iter = syms.dwarf.units();
        while let Some(header) = iter.next().map_err(err("unit header"))? {
            let unit = syms.dwarf.unit(header).map_err(err("unit"))?;
            let unit_idx = syms.units.len();
            let mut name = String::new();
            let mut comp_dir = String::new();
            let mut cur_enum: Option<(isize, DieRef)> = None;

            let mut cursor = unit.entries();
            while let Some(entry) = cursor.next_dfs().map_err(err("DIE"))? {
                let depth = entry.depth();
                let here = DieRef { unit: unit_idx, off: entry.offset().0 };
                match entry.tag() {
                    gimli::DW_TAG_compile_unit | gimli::DW_TAG_partial_unit => {
                        name = syms.attr_str(&unit, entry.attr_value(gimli::DW_AT_name)).unwrap_or_default();
                        comp_dir = syms.attr_str(&unit, entry.attr_value(gimli::DW_AT_comp_dir)).unwrap_or_default();
                    }
                    gimli::DW_TAG_subprogram => {
                        if is_declaration(entry) {
                            continue;
                        }
                        let Some(AttributeValue::Addr(low)) = entry.attr_value(gimli::DW_AT_low_pc) else { continue };
                        let high = match entry.attr_value(gimli::DW_AT_high_pc) {
                            Some(AttributeValue::Addr(h)) => h,
                            Some(v) => match v.udata_value() {
                                Some(len) => low + len,
                                None => continue,
                            },
                            None => continue,
                        };
                        let Some(fname) = syms.attr_str(&unit, entry.attr_value(gimli::DW_AT_name)) else { continue };
                        let decl_line = entry.attr_value(gimli::DW_AT_decl_line).and_then(|v| v.udata_value()).unwrap_or(0) as u32;
                        let external = matches!(entry.attr_value(gimli::DW_AT_external), Some(AttributeValue::Flag(true)));
                        syms.functions.push(Function {
                            name: fname,
                            low: low as u32,
                            high: high as u32,
                            unit: unit_idx,
                            die: entry.offset(),
                            decl_line,
                            external,
                        });
                    }
                    gimli::DW_TAG_variable if depth == 1 => {
                        let Some(AttributeValue::Exprloc(expr)) = entry.attr_value(gimli::DW_AT_location) else { continue };
                        let mut ops = expr.operations(unit.encoding());
                        let Ok(Some(gimli::Operation::Address { address })) = ops.next() else { continue };
                        let Some(vname) = syms.attr_str(&unit, entry.attr_value(gimli::DW_AT_name)) else { continue };
                        let external = matches!(entry.attr_value(gimli::DW_AT_external), Some(AttributeValue::Flag(true)));
                        syms.globals.push(Global { name: vname, addr: address as u32, unit: unit_idx, die: entry.offset(), external });
                    }
                    gimli::DW_TAG_base_type
                    | gimli::DW_TAG_typedef
                    | gimli::DW_TAG_structure_type
                    | gimli::DW_TAG_class_type
                    | gimli::DW_TAG_union_type
                    | gimli::DW_TAG_enumeration_type => {
                        if entry.tag() == gimli::DW_TAG_enumeration_type {
                            cur_enum = Some((depth, here));
                        }
                        if is_declaration(entry) {
                            continue;
                        }
                        let Some(n) = syms.attr_str(&unit, entry.attr_value(gimli::DW_AT_name)).filter(|n| !n.is_empty()) else {
                            continue;
                        };
                        let key = match entry.tag() {
                            gimli::DW_TAG_structure_type | gimli::DW_TAG_class_type => format!("struct {n}"),
                            gimli::DW_TAG_union_type => format!("union {n}"),
                            gimli::DW_TAG_enumeration_type => format!("enum {n}"),
                            _ => n,
                        };
                        let v = syms.types_by_name.entry(key).or_default();
                        if v.len() < NAME_INDEX_CAP {
                            v.push(here);
                        }
                    }
                    gimli::DW_TAG_enumerator => {
                        let Some((d, en)) = cur_enum else { continue };
                        if d + 1 != depth {
                            continue;
                        }
                        let Some(n) = syms.attr_str(&unit, entry.attr_value(gimli::DW_AT_name)) else { continue };
                        let v = syms.enumerators_by_name.entry(n).or_default();
                        if v.len() < NAME_INDEX_CAP {
                            v.push((here, en));
                        }
                    }
                    _ => {}
                }
            }
            drop(cursor);

            if let Some(program) = unit.line_program.clone() {
                let mut rows = program.rows();
                let mut map: HashMap<u64, u32> = HashMap::new();
                while let Some((lheader, row)) = rows.next_row().map_err(err("line program"))? {
                    let fidx = row.file_index();
                    let file = match map.get(&fidx) {
                        Some(f) => *f,
                        None => {
                            let path = syms.line_file_path(&unit, lheader, fidx, &comp_dir);
                            let norm = normalize_path(&path);
                            let id = *file_ids.entry(norm.clone()).or_insert_with(|| {
                                syms.files.push(path.clone());
                                syms.files_norm.push(norm.clone());
                                (syms.files.len() - 1) as u32
                            });
                            map.insert(fidx, id);
                            id
                        }
                    };
                    syms.rows.push(Row {
                        addr: row.address() as u32,
                        file,
                        line: row.line().map(|l| l.get() as u32).unwrap_or(0),
                        col: match row.column() {
                            gimli::ColumnType::LeftEdge => 0,
                            gimli::ColumnType::Column(c) => c.get() as u32,
                        },
                        end: row.end_sequence(),
                    });
                }
            }
            syms.unit_offsets.push(header.debug_info_offset().map(|o| o.0).unwrap_or(0));
            syms.units.push(Unit { header, name, comp_dir });
            let cell = OnceLock::new();
            let _ = cell.set(Some(unit));
            syms.unit_cache.push(cell);
        }

        syms.functions.sort_by_key(|f| (f.low, f.high));
        for (i, f) in syms.functions.iter().enumerate() {
            syms.functions_by_name.entry(f.name.clone()).or_default().push(i);
        }
        for (i, g) in syms.globals.iter().enumerate() {
            syms.globals_by_name.entry(g.name.clone()).or_default().push(i);
        }
        syms.rows.sort_by_key(|r| (r.addr, !r.end));
        for (i, r) in syms.rows.iter().enumerate() {
            if !r.end && r.line != 0 {
                syms.rows_by_file.entry(r.file).or_default().push(i as u32);
            }
        }
        syms.load_time = t0.elapsed();
        Ok(syms)
    }

    pub(crate) fn attr_str(&self, unit: &gimli::Unit<R>, v: Option<AttributeValue<R>>) -> Option<String> {
        let v = v?;
        let s = self.dwarf.attr_string(unit, v).ok()?;
        Some(s.to_string_lossy().into_owned())
    }

    fn line_file_path(
        &self,
        unit: &gimli::Unit<R>,
        header: &gimli::LineProgramHeader<R>,
        fidx: u64,
        comp_dir: &str,
    ) -> String {
        let Some(fe) = header.file(fidx) else { return format!("<file {fidx}>") };
        let name = self.attr_str(unit, Some(fe.path_name())).unwrap_or_default();
        if is_absolute(&name) {
            return name;
        }
        let dir = fe
            .directory(header)
            .and_then(|d| self.attr_str(unit, Some(d)))
            .unwrap_or_default();
        let dir = if dir.is_empty() || is_absolute(&dir) { dir } else { join_path(comp_dir, &dir) };
        let dir = if dir.is_empty() { comp_dir.to_string() } else { dir };
        join_path(&dir, &name)
    }

    // --- DIE access --------------------------------------------------------------------

    pub fn unit(&self, idx: usize) -> Option<&gimli::Unit<R>> {
        self.unit_cache.get(idx)?.get_or_init(|| self.dwarf.unit(self.units[idx].header).ok()).as_ref()
    }

    /// The DIE at a .debug_info section offset.
    pub fn die_at_global(&self, goff: usize) -> Option<DieRef> {
        let i = self.unit_offsets.partition_point(|&o| o <= goff);
        let i = i.checked_sub(1)?;
        Some(DieRef { unit: i, off: goff - self.unit_offsets[i] })
    }

    pub(crate) fn entry(&self, r: DieRef) -> Option<(&gimli::Unit<R>, Die)> {
        let u = self.unit(r.unit)?;
        let e = u.header.entry(&u.abbreviations, UnitOffset(r.off)).ok()?;
        Some((u, e))
    }

    /// Direct children of a DIE.
    pub(crate) fn children(&self, r: DieRef) -> Vec<(DieRef, Die)> {
        let Some(u) = self.unit(r.unit) else { return Vec::new() };
        let Ok(mut cur) = u.entries_at_offset(UnitOffset(r.off)) else { return Vec::new() };
        let mut out = Vec::new();
        let mut base = None;
        while let Ok(Some(e)) = cur.next_dfs() {
            let d = e.depth();
            let b = *base.get_or_insert(d);
            if d <= b && e.offset().0 != r.off {
                break;
            }
            if d == b + 1 {
                out.push((DieRef { unit: r.unit, off: e.offset().0 }, e.clone()));
            }
        }
        out
    }

    /// A reference attribute (e.g. DW_AT_type) of a DIE.
    pub(crate) fn ref_attr(&self, from: DieRef, e: &Die, at: gimli::DwAt) -> Option<DieRef> {
        match e.attr_value(at)? {
            AttributeValue::UnitRef(o) => Some(DieRef { unit: from.unit, off: o.0 }),
            AttributeValue::DebugInfoRef(g) => self.die_at_global(g.0),
            _ => None,
        }
    }

    pub fn die_name(&self, r: DieRef) -> Option<String> {
        let (u, e) = self.entry(r)?;
        self.attr_str(u, e.attr_value(gimli::DW_AT_name))
    }

    pub fn types_named(&self, name: &str) -> &[DieRef] {
        self.types_by_name.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn enumerators_named(&self, name: &str) -> &[(DieRef, DieRef)] {
        self.enumerators_by_name.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn global_indices(&self, name: &str) -> &[usize] {
        self.globals_by_name.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn function_indices(&self, name: &str) -> &[usize] {
        self.functions_by_name.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    // --- Addresses and lines -------------------------------------------------------------

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Innermost function containing `addr`.
    pub fn function_at(&self, addr: u32) -> Option<&Function> {
        let i = self.functions.partition_point(|f| f.low <= addr);
        self.functions[..i]
            .iter()
            .rev()
            .take(16)
            .filter(|f| f.contains(addr))
            .min_by_key(|f| f.high - f.low)
    }

    pub fn function_index_at(&self, addr: u32) -> Option<usize> {
        let f = self.function_at(addr)?;
        self.functions.iter().position(|x| std::ptr::eq(x, f))
    }

    pub fn functions_named(&self, name: &str) -> Vec<&Function> {
        self.function_indices(name).iter().map(|&i| &self.functions[i]).collect()
    }

    pub fn globals_named(&self, name: &str) -> Vec<&Global> {
        self.global_indices(name).iter().map(|&i| &self.globals[i]).collect()
    }

    fn row_end(&self, i: usize) -> u32 {
        let a = self.rows[i].addr;
        self.rows[i + 1..].iter().find(|r| r.addr > a).map(|r| r.addr).unwrap_or(a.saturating_add(2))
    }

    fn info(&self, i: usize) -> LineInfo {
        let r = self.rows[i];
        LineInfo { file: self.files[r.file as usize].clone(), line: r.line, column: r.col, start: r.addr, end: self.row_end(i) }
    }

    /// Line table entry covering `addr`.
    pub fn line_at(&self, addr: u32) -> Option<LineInfo> {
        let i = self.rows.partition_point(|r| r.addr <= addr);
        if i == 0 {
            return None;
        }
        let r = self.rows[i - 1];
        if r.end || r.line == 0 {
            return None;
        }
        Some(self.info(i - 1))
    }

    /// Line entries overlapping `[start, end)`.
    pub fn lines_in_range(&self, start: u32, end: u32) -> Vec<LineInfo> {
        let mut out = Vec::new();
        let mut i = self.rows.partition_point(|r| r.addr <= start).saturating_sub(1);
        while i < self.rows.len() && self.rows[i].addr < end.max(start + 1) {
            let r = self.rows[i];
            // Several rows at one address: the last one describes the code.
            let last_at_addr = self.rows.get(i + 1).is_none_or(|n| n.addr != r.addr);
            if !r.end && r.line != 0 && last_at_addr {
                let info = self.info(i);
                if info.end > start {
                    out.push(info);
                }
            }
            i += 1;
        }
        out
    }

    /// Source files matching a (possibly relative or partial) path by suffix.
    pub fn find_files(&self, query: &str) -> Vec<u32> {
        let q = normalize_path(query);
        if q.is_empty() {
            return Vec::new();
        }
        let tail = format!("/{q}");
        (0..self.files_norm.len() as u32)
            .filter(|&i| {
                let f = &self.files_norm[i as usize];
                *f == q || f.ends_with(&tail)
            })
            .collect()
    }

    pub fn file_name(&self, id: u32) -> &str {
        &self.files[id as usize]
    }

    /// Addresses for a source line: the nearest line at or after `line` that
    /// has code, lowest address per function. Returns (line, addresses).
    pub fn resolve_line(&self, file: &str, line: u32) -> Result<(u32, Vec<u32>), String> {
        let ids = self.find_files(file);
        if ids.is_empty() {
            return Err(format!("No line information for file '{file}'"));
        }
        let mut best: Option<u32> = None;
        for id in &ids {
            for &ri in self.rows_by_file.get(id).map(Vec::as_slice).unwrap_or(&[]) {
                let l = self.rows[ri as usize].line;
                if l >= line && best.is_none_or(|b| l < b) {
                    best = Some(l);
                }
            }
        }
        let best = best.ok_or_else(|| format!("No code at or after line {line} in '{file}'"))?;
        let mut per_fn: HashMap<u32, u32> = HashMap::new();
        let mut loose: Vec<u32> = Vec::new();
        for id in &ids {
            for &ri in self.rows_by_file.get(id).map(Vec::as_slice).unwrap_or(&[]) {
                let r = self.rows[ri as usize];
                if r.line != best {
                    continue;
                }
                match self.function_at(r.addr) {
                    Some(f) => {
                        let e = per_fn.entry(f.low).or_insert(r.addr);
                        *e = (*e).min(r.addr);
                    }
                    None => loose.push(r.addr),
                }
            }
        }
        let mut addrs: Vec<u32> = per_fn.into_values().collect();
        if addrs.is_empty() {
            addrs = loose;
        }
        addrs.sort_unstable();
        addrs.dedup();
        Ok((best, addrs))
    }

    /// Address for a location expression: a number or a function name.
    pub fn resolve_location(&self, loc: &str) -> Option<u32> {
        let t = loc.trim();
        if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
            return u32::from_str_radix(h, 16).ok();
        }
        if let Ok(v) = t.parse::<u32>() {
            return Some(v);
        }
        let fns = self.functions_named(t);
        fns.iter().find(|f| f.external).or(fns.first()).map(|f| f.low)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(normalize_path(r"C:\Workspace\app\src\a.c"), "c:/workspace/app/src/a.c");
        assert_eq!(normalize_path("../../bsw/x/./y.c"), "bsw/x/y.c");
        assert_eq!(normalize_path("a/b/../c.c"), "a/c.c");
        assert_eq!(join_path(r"C:\build", "x.c"), r"C:\build\x.c");
    }

    pub(crate) fn test_elf() -> Option<PathBuf> {
        let p = std::env::var("AURIX_TEST_ELF")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("C:/workspace/app/build/bin/app.elf"));
        p.exists().then_some(p)
    }

    #[test]
    fn real_ghs_elf() {
        let Some(path) = test_elf() else {
            eprintln!("test ELF not found, skipping");
            return;
        };
        let s = Symbols::load(&path).unwrap();
        eprintln!(
            "loaded in {:?}: {} units, {} functions, {} globals, {} files, {} rows",
            s.load_time,
            s.units.len(),
            s.functions.len(),
            s.globals.len(),
            s.file_count(),
            s.row_count()
        );
        let f = s.function_at(0x8014_29a0).expect("function at 0x801429a0");
        assert_eq!(f.name, "Module_Init");
        assert_eq!((f.low, f.high), (0x8014_29a0, 0x8014_29be));
        let li = s.line_at(0x8014_29a0).expect("line info");
        assert!(normalize_path(&li.file).ends_with("module/src/module.c"), "{li:?}");
        let (line, addrs) = s.resolve_line("../../bsw/AutosarAbstractionLayers/SL/Module/src/Module.c", li.line).unwrap();
        assert_eq!(line, li.line);
        assert!(addrs.contains(&li.start) || addrs.iter().any(|a| f.contains(*a)), "{addrs:08x?}");
        assert!(!s.lines_in_range(li.start, li.start + 1).is_empty());
        assert_eq!(s.globals_named("module_sm").first().map(|g| g.addr), Some(0x7001_3954));
        assert_eq!(s.resolve_location("Module_Init"), Some(0x8014_29a0));
    }
}
