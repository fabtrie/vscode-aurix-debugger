//! DWARF symbol access for TriCore ELF files: functions, globals, line
//! tables, source file lookup, types, local variables and C expressions.
//!
//! Written against GHS C 2021 output (DWARF 2: no .debug_aranges, absolute
//! Windows paths in the line table, CU DIEs without PC ranges, type
//! references across units), which also covers GCC output.
//!
//! C++: functions, variables and types declared in namespaces and classes get their qualified
//! name (`app::Motor::step`); a lookup by name also finds them by any trailing part of it
//! (`Motor::step`, `step`). Definitions outside their class or namespace (DW_AT_specification)
//! and out-of-line instances (DW_AT_abstract_origin) take name, type and linkage from the DIE
//! they complete.

pub mod cdb;
pub mod expr;
pub mod mcs_asm;
pub mod scope;
pub mod types;

use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
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
    /// Address of the function's exit instruction, where the debug information says (SDCC);
    /// a step out runs there.
    pub exit: Option<u32>,
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
    /// The type, for a global without a DIE whose debug information gives one (SDCC); see
    /// [`Symbols::global_type`].
    pub ty: Option<types::TypeId>,
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
    /// Compiled from C++: types are named without `struct`/`enum`, qualified by their scope.
    pub cpp: bool,
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
    /// C++: functions and globals by a trailing part of their qualified name, for names
    /// that no symbol has as its full name.
    functions_by_suffix: HashMap<String, Vec<usize>>,
    globals_by_suffix: HashMap<String, Vec<usize>>,
    /// Named types ("uint8", "struct foo", "enum bar"; C++ also "ns::foo", "foo"), a few definitions each.
    types_by_name: HashMap<String, Vec<DieRef>>,
    /// Enumerator name -> (enumerator, enumeration type).
    enumerators_by_name: HashMap<String, Vec<(DieRef, DieRef)>>,
    /// C++: the scope prefix (`app::Motor::`) of the functions, variables, static members and
    /// types declared in a namespace or class.
    scopes: HashMap<DieRef, Arc<str>>,
    /// All line table rows, sorted by address (end-of-sequence rows first).
    rows: Vec<Row>,
    files: Vec<String>,
    files_norm: Vec<String>,
    rows_by_file: HashMap<u32, Vec<u32>>,
    unit_cache: Vec<OnceLock<Option<gimli::Unit<R>>>>,
    /// .debug_info offset of each unit (ascending).
    unit_offsets: Vec<usize>,
    /// Locals of a symbol source without DWARF (see [`scope::TableLocal`]).
    pub table_locals: Vec<scope::TableLocal>,
    pub load_time: Duration,
    /// The members of the struct globals by expression, built on first use.
    member_paths: OnceLock<Vec<types::MemberPath>>,
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

/// A DIE that completes a declaration or is an instance of an abstract DIE, so takes its name
/// from there.
fn completes_other(e: &Die) -> bool {
    e.attr_value(gimli::DW_AT_specification).is_some() || e.attr_value(gimli::DW_AT_abstract_origin).is_some()
}

/// Byte offsets of the top-level `::` separators of a C++ name (not those inside `<...>` or
/// `(...)`, as in `Ring<app::Item, 4>::push`).
fn scope_separators(name: &str) -> Vec<usize> {
    let b = name.as_bytes();
    let mut out = Vec::new();
    let mut nest = 0i32;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'<' | b'(' => nest += 1,
            b'>' | b')' => nest -= 1,
            b':' if nest == 0 && b.get(i + 1) == Some(&b':') => {
                out.push(i);
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// Whether an expression can name a global by its name: a C identifier or a qualified C++ name
/// of identifiers (`app::Motor::count`), not a template instance or an anonymous namespace.
pub fn is_expression_name(s: &str) -> bool {
    s.split("::").all(|part| {
        let mut b = part.bytes();
        b.next().is_some_and(|c| c.is_ascii_alphabetic() || c == b'_') && b.all(|c| c.is_ascii_alphanumeric() || c == b'_')
    })
}

/// The trailing parts of a qualified C++ name, longest first, without the name itself:
/// `a::B::f` gives `B::f` and `f`.
pub fn name_suffixes(name: &str) -> impl Iterator<Item = &str> {
    scope_separators(name).into_iter().map(move |i| &name[i + 2..])
}

/// Adds `i` under `key` once.
fn index_name(map: &mut HashMap<String, Vec<usize>>, key: &str, i: usize) {
    let v = map.entry(key.to_string()).or_default();
    if !v.contains(&i) {
        v.push(i);
    }
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
            functions_by_suffix: HashMap::new(),
            globals_by_suffix: HashMap::new(),
            types_by_name: HashMap::new(),
            enumerators_by_name: HashMap::new(),
            scopes: HashMap::new(),
            rows: Vec::new(),
            files: Vec::new(),
            files_norm: Vec::new(),
            rows_by_file: HashMap::new(),
            unit_cache: Vec::new(),
            unit_offsets: Vec::new(),
            table_locals: Vec::new(),
            load_time: Duration::ZERO,
            member_paths: OnceLock::new(),
        };
        let mut file_ids: HashMap<String, u32> = HashMap::new();
        // Functions and globals whose DIE has no name of its own (C++ definitions completing a
        // declaration): named once all units are read, as the declaration may come later.
        let mut unnamed_functions: Vec<usize> = Vec::new();
        let mut unnamed_globals: Vec<usize> = Vec::new();

        let mut iter = syms.dwarf.units();
        while let Some(header) = iter.next().map_err(err("unit header"))? {
            let unit = syms.dwarf.unit(header).map_err(err("unit"))?;
            let unit_idx = syms.units.len();
            let mut name = String::new();
            let mut comp_dir = String::new();
            let mut cpp = false;
            let no_prefix: Arc<str> = "".into();
            let mut cur_enum: Option<(isize, DieRef)> = None;
            // The DIEs enclosing the current one: depth, tag, and the C++ scope prefix of
            // their children ("app::Motor::", empty outside namespaces and classes).
            let mut parents: Vec<(isize, gimli::DwTag, Arc<str>)> = Vec::new();

            let mut cursor = unit.entries();
            while let Some(entry) = cursor.next_dfs().map_err(err("DIE"))? {
                let depth = entry.depth();
                let here = DieRef { unit: unit_idx, off: entry.offset().0 };
                while parents.last().is_some_and(|p| p.0 >= depth) {
                    parents.pop();
                }
                let parent_tag = parents.last().map(|p| p.1);
                let prefix: Arc<str> = parents.last().map_or_else(|| Arc::clone(&no_prefix), |p| p.2.clone());
                // Read on demand: most DIEs need no name. Borrows only the DWARF sections.
                let dwarf = &syms.dwarf;
                let own_name = || {
                    let s = dwarf.attr_string(&unit, entry.attr_value(gimli::DW_AT_name)?).ok()?;
                    Some(s.to_string_lossy().into_owned()).filter(|n| !n.is_empty())
                };
                if entry.has_children() {
                    let scope = match entry.tag() {
                        gimli::DW_TAG_namespace => Some(own_name().unwrap_or_else(|| "(anonymous namespace)".into())),
                        gimli::DW_TAG_structure_type
                        | gimli::DW_TAG_class_type
                        | gimli::DW_TAG_union_type
                        | gimli::DW_TAG_enumeration_type
                            if cpp =>
                        {
                            own_name()
                        }
                        _ => None,
                    };
                    let child_prefix = match scope {
                        Some(n) => format!("{prefix}{n}::").into(),
                        None => prefix.clone(),
                    };
                    parents.push((depth, entry.tag(), child_prefix));
                }
                // The scope of what a namespace or class declares, also of declarations that a
                // definition elsewhere completes.
                if !prefix.is_empty() {
                    let named = match entry.tag() {
                        gimli::DW_TAG_subprogram
                        | gimli::DW_TAG_variable
                        | gimli::DW_TAG_base_type
                        | gimli::DW_TAG_typedef
                        | gimli::DW_TAG_structure_type
                        | gimli::DW_TAG_class_type
                        | gimli::DW_TAG_union_type
                        | gimli::DW_TAG_enumeration_type => true,
                        // A static data member (DWARF 2-4).
                        gimli::DW_TAG_member => is_declaration(entry),
                        _ => false,
                    };
                    if named {
                        syms.scopes.insert(here, prefix.clone());
                    }
                }
                match entry.tag() {
                    gimli::DW_TAG_compile_unit | gimli::DW_TAG_partial_unit => {
                        name = syms.attr_str(&unit, entry.attr_value(gimli::DW_AT_name)).unwrap_or_default();
                        comp_dir = syms.attr_str(&unit, entry.attr_value(gimli::DW_AT_comp_dir)).unwrap_or_default();
                        cpp = matches!(
                            entry.attr_value(gimli::DW_AT_language),
                            Some(AttributeValue::Language(
                                gimli::DW_LANG_C_plus_plus
                                    | gimli::DW_LANG_C_plus_plus_03
                                    | gimli::DW_LANG_C_plus_plus_11
                                    | gimli::DW_LANG_C_plus_plus_14
                            ))
                        );
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
                        // Functions removed by the linker keep their DIE with address 0; some assembler
                        // functions have no valid end.
                        if low == 0 || high <= low {
                            continue;
                        }
                        let fname = match own_name() {
                            Some(n) => format!("{prefix}{n}"),
                            None if completes_other(entry) => {
                                unnamed_functions.push(syms.functions.len());
                                String::new()
                            }
                            None => continue,
                        };
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
                            exit: None,
                        });
                    }
                    // File scope or a C++ namespace (not a function's static variable).
                    gimli::DW_TAG_variable
                        if depth == 1
                            || matches!(parent_tag, Some(gimli::DW_TAG_namespace | gimli::DW_TAG_compile_unit | gimli::DW_TAG_partial_unit)) =>
                    {
                        let Some(AttributeValue::Exprloc(expr)) = entry.attr_value(gimli::DW_AT_location) else { continue };
                        let mut ops = expr.operations(unit.encoding());
                        let Ok(Some(gimli::Operation::Address { address })) = ops.next() else { continue };
                        let vname = match own_name() {
                            Some(n) => format!("{prefix}{n}"),
                            None if completes_other(entry) => {
                                unnamed_globals.push(syms.globals.len());
                                String::new()
                            }
                            None => continue,
                        };
                        let external = matches!(entry.attr_value(gimli::DW_AT_external), Some(AttributeValue::Flag(true)));
                        syms.globals.push(Global { name: vname, addr: address as u32, unit: unit_idx, die: entry.offset(), external, ty: None });
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
                        let Some(n) = own_name() else { continue };
                        let mut keys = vec![match entry.tag() {
                            gimli::DW_TAG_structure_type | gimli::DW_TAG_class_type => format!("struct {n}"),
                            gimli::DW_TAG_union_type => format!("union {n}"),
                            gimli::DW_TAG_enumeration_type => format!("enum {n}"),
                            _ => n.clone(),
                        }];
                        // C++ names a type without the keyword, qualified or not.
                        if cpp && !prefix.is_empty() {
                            let q = format!("{prefix}{n}");
                            keys.extend(name_suffixes(&q).map(str::to_string));
                            keys.push(q);
                        } else if cpp {
                            keys.push(n);
                        }
                        for key in keys {
                            let v = syms.types_by_name.entry(key).or_default();
                            if v.len() < NAME_INDEX_CAP && !v.contains(&here) {
                                v.push(here);
                            }
                        }
                    }
                    gimli::DW_TAG_enumerator => {
                        let Some((d, en)) = cur_enum else { continue };
                        if d + 1 != depth {
                            continue;
                        }
                        let Some(n) = syms.attr_str(&unit, entry.attr_value(gimli::DW_AT_name)) else { continue };
                        // C++: also `Mode::Fast` and `app::Mode::Fast` (the prefix includes the
                        // enumeration's name); C names an enumerator by its name only.
                        let mut keys = vec![n.clone()];
                        if !prefix.is_empty() {
                            let q = format!("{prefix}{n}");
                            keys.extend(name_suffixes(&q).filter(|k| *k != n).map(str::to_string));
                            keys.push(q);
                        }
                        for key in keys {
                            let v = syms.enumerators_by_name.entry(key).or_default();
                            if v.len() < NAME_INDEX_CAP && !v.contains(&(here, en)) {
                                v.push((here, en));
                            }
                        }
                    }
                    _ => {}
                }
            }
            drop(cursor);

            if let Some(program) = unit.line_program.clone() {
                let mut rows = program.rows();
                let mut map: HashMap<u64, u32> = HashMap::new();
                // Rows of the current sequence: a sequence starting at address 0 belongs to
                // code removed by the linker and is dropped.
                let mut seq: Vec<Row> = Vec::new();
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
                    seq.push(Row {
                        addr: row.address() as u32,
                        file,
                        line: row.line().map(|l| l.get() as u32).unwrap_or(0),
                        col: match row.column() {
                            gimli::ColumnType::LeftEdge => 0,
                            gimli::ColumnType::Column(c) => c.get() as u32,
                        },
                        end: row.end_sequence(),
                    });
                    if row.end_sequence() {
                        if seq[0].addr != 0 {
                            syms.rows.append(&mut seq);
                        }
                        seq.clear();
                    }
                }
                if seq.first().is_some_and(|r| r.addr != 0) {
                    syms.rows.append(&mut seq);
                }
            }
            syms.unit_offsets.push(header.debug_info_offset().map(|o| o.0).unwrap_or(0));
            syms.units.push(Unit { header, name, comp_dir, cpp });
            let cell = OnceLock::new();
            let _ = cell.set(Some(unit));
            syms.unit_cache.push(cell);
        }

        // Definitions completing a declaration: name and linkage from the declaration.
        for i in unnamed_functions {
            let d = syms.functions[i].die_ref();
            syms.functions[i].name = syms.qualified_name(d).unwrap_or_default();
            syms.functions[i].external |= syms.inherited_flag(d, gimli::DW_AT_external);
            if syms.functions[i].decl_line == 0 {
                syms.functions[i].decl_line =
                    syms.inherited_attr(d, gimli::DW_AT_decl_line).and_then(|(_, v)| v.udata_value()).unwrap_or(0) as u32;
            }
        }
        for i in unnamed_globals {
            let d = syms.globals[i].die_ref();
            syms.globals[i].name = syms.qualified_name(d).unwrap_or_default();
            syms.globals[i].external |= syms.inherited_flag(d, gimli::DW_AT_external);
        }
        syms.functions.retain(|f| !f.name.is_empty());
        syms.globals.retain(|g| !g.name.is_empty());

        syms.functions.sort_by_key(|f| (f.low, f.high));
        syms.index_names();
        syms.rows.sort_by_key(|r| (r.addr, !r.end));
        for (i, r) in syms.rows.iter().enumerate() {
            if !r.end && r.line != 0 {
                syms.rows_by_file.entry(r.file).or_default().push(i as u32);
            }
        }
        syms.load_time = t0.elapsed();
        Ok(syms)
    }

    /// Fills the function and global name indexes.
    fn index_names(&mut self) {
        for (i, f) in self.functions.iter().enumerate() {
            index_name(&mut self.functions_by_name, &f.name, i);
            for s in name_suffixes(&f.name) {
                index_name(&mut self.functions_by_suffix, s, i);
            }
        }
        for (i, g) in self.globals.iter().enumerate() {
            index_name(&mut self.globals_by_name, &g.name, i);
            for s in name_suffixes(&g.name) {
                index_name(&mut self.globals_by_suffix, s, i);
            }
        }
    }

    /// Symbols without debug information (e.g. from an assembler source): functions as
    /// `(name, low, high)`, globals as `(name, address)`, line rows as `(address, file, line)`
    /// with `end` rows as `(address, None, 0)` marking the end of a code range.
    pub fn from_tables(
        path: &Path,
        functions: Vec<(String, u32, u32)>,
        globals: Vec<(String, u32)>,
        files: Vec<String>,
        rows: Vec<(u32, Option<u32>, u32)>,
    ) -> Symbols {
        let no_die = UnitOffset(0);
        let mut syms = Symbols {
            path: path.to_path_buf(),
            dwarf: gimli::Dwarf::default(),
            units: Vec::new(),
            functions: functions
                .into_iter()
                .map(|(name, low, high)| Function { name, low, high, unit: usize::MAX, die: no_die, decl_line: 0, external: true, exit: None })
                .collect(),
            globals: globals
                .into_iter()
                .map(|(name, addr)| Global { name, addr, unit: usize::MAX, die: no_die, external: true, ty: None })
                .collect(),
            functions_by_name: HashMap::new(),
            globals_by_name: HashMap::new(),
            functions_by_suffix: HashMap::new(),
            globals_by_suffix: HashMap::new(),
            types_by_name: HashMap::new(),
            enumerators_by_name: HashMap::new(),
            scopes: HashMap::new(),
            rows: rows
                .into_iter()
                .map(|(addr, file, line)| Row { addr, file: file.unwrap_or(0), line, col: 0, end: file.is_none() })
                .collect(),
            files_norm: files.iter().map(|f| normalize_path(f)).collect(),
            files,
            rows_by_file: HashMap::new(),
            unit_cache: Vec::new(),
            unit_offsets: Vec::new(),
            table_locals: Vec::new(),
            load_time: Duration::ZERO,
            member_paths: OnceLock::new(),
        };
        syms.functions.sort_by_key(|f| (f.low, f.high));
        syms.index_names();
        syms.rows.sort_by_key(|r| (r.addr, !r.end));
        for (i, r) in syms.rows.iter().enumerate() {
            if !r.end && r.line != 0 {
                syms.rows_by_file.entry(r.file).or_default().push(i as u32);
            }
        }
        syms
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
        self.ref_value(from, e.attr_value(at)?)
    }

    /// The DIE that a reference attribute value of DIE `from` refers to.
    fn ref_value(&self, from: DieRef, v: AttributeValue<R>) -> Option<DieRef> {
        match v {
            AttributeValue::UnitRef(o) => Some(DieRef { unit: from.unit, off: o.0 }),
            AttributeValue::DebugInfoRef(g) => self.die_at_global(g.0),
            _ => None,
        }
    }

    /// The DIE that a DIE completes (DW_AT_specification: a C++ definition outside its class or
    /// namespace) or is an instance of (DW_AT_abstract_origin).
    fn origin(&self, d: DieRef, e: &Die) -> Option<DieRef> {
        self.ref_attr(d, e, gimli::DW_AT_specification).or_else(|| self.ref_attr(d, e, gimli::DW_AT_abstract_origin))
    }

    /// An attribute of a DIE, else of its [origin](Self::origin): the DIE that has it, and the value.
    pub(crate) fn inherited_attr(&self, d: DieRef, at: gimli::DwAt) -> Option<(DieRef, AttributeValue<R>)> {
        let mut cur = d;
        for _ in 0..8 {
            let (_, e) = self.entry(cur)?;
            if let Some(v) = e.attr_value(at) {
                return Some((cur, v));
            }
            cur = self.origin(cur, &e)?;
        }
        None
    }

    fn inherited_flag(&self, d: DieRef, at: gimli::DwAt) -> bool {
        matches!(self.inherited_attr(d, at), Some((_, AttributeValue::Flag(true))))
    }

    /// A reference attribute of a DIE or of its origin (see [`Self::inherited_attr`]).
    pub(crate) fn inherited_ref(&self, d: DieRef, at: gimli::DwAt) -> Option<DieRef> {
        let (from, v) = self.inherited_attr(d, at)?;
        self.ref_value(from, v)
    }

    /// The name of a function, variable or type, qualified by its C++ namespaces and classes.
    pub fn qualified_name(&self, d: DieRef) -> Option<String> {
        let mut cur = d;
        for _ in 0..8 {
            let (u, e) = self.entry(cur)?;
            if let Some(n) = self.attr_str(u, e.attr_value(gimli::DW_AT_name)).filter(|n| !n.is_empty()) {
                return Some(match self.scopes.get(&cur) {
                    Some(p) => format!("{p}{n}"),
                    None => n,
                });
            }
            cur = self.origin(cur, &e)?;
        }
        None
    }

    /// Whether a DIE comes from a C++ unit.
    pub(crate) fn is_cpp(&self, d: DieRef) -> bool {
        self.units.get(d.unit).is_some_and(|u| u.cpp)
    }

    /// The qualified name of a type DIE declared in a C++ namespace or class.
    pub(crate) fn scoped_name(&self, d: DieRef) -> Option<String> {
        let p = self.scopes.get(&d)?;
        Some(format!("{p}{}", self.die_name(d)?))
    }

    /// The type of global `i`: its DIE's; without one, the type its table gave (SDCC), else one
    /// 32-bit word (MCS data labels).
    pub fn global_type(&self, i: usize) -> types::TypeId {
        match self.globals.get(i) {
            Some(g) if g.unit != usize::MAX => self.var_type(g.die_ref()),
            Some(g) => g.ty.clone().unwrap_or(types::TypeId::Base(types::TypeClass::Cardinal, 4)),
            None => types::TypeId::Void,
        }
    }

    /// The (unqualified) name of a DIE, or of the DIE it completes or is an instance of.
    pub fn die_name(&self, r: DieRef) -> Option<String> {
        let (d, v) = self.inherited_attr(r, gimli::DW_AT_name)?;
        self.attr_str(self.unit(d.unit)?, Some(v))
    }

    /// The members of the struct, class and union globals, nested ones included, each by the
    /// expression that names it (`g_motor.cfg.speed`, `ns::g_obj.count`); see
    /// [`types::MemberPath`].
    pub fn member_paths(&self) -> &[types::MemberPath] {
        self.member_paths.get_or_init(|| self.build_member_paths())
    }

    pub fn types_named(&self, name: &str) -> &[DieRef] {
        self.types_by_name.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn enumerators_named(&self, name: &str) -> &[(DieRef, DieRef)] {
        self.enumerators_by_name.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Number of source files in the line table.
    pub fn source_files(&self) -> usize {
        self.files.len()
    }

    /// Globals with this name; else (C++) the globals whose qualified name ends in it
    /// (`Motor::count` finds `app::Motor::count`). A leading `::` is ignored.
    pub fn global_indices(&self, name: &str) -> &[usize] {
        let name = name.strip_prefix("::").unwrap_or(name);
        self.globals_by_name.get(name).or_else(|| self.globals_by_suffix.get(name)).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Functions with this name, else C++ functions whose qualified name ends in it (see
    /// [`Self::global_indices`]); all overloads of a C++ function.
    pub fn function_indices(&self, name: &str) -> &[usize] {
        let name = name.strip_prefix("::").unwrap_or(name);
        self.functions_by_name.get(name).or_else(|| self.functions_by_suffix.get(name)).map(Vec::as_slice).unwrap_or(&[])
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

    /// Addresses for a location expression: a number, or every function of that name (the
    /// overloads of a C++ function, its constructor variants, static functions of several
    /// files), external ones first.
    pub fn resolve_locations(&self, loc: &str) -> Vec<u32> {
        let t = loc.trim();
        let mut fns = self.functions_named(t);
        if fns.is_empty() {
            return self.resolve_location(t).into_iter().collect();
        }
        fns.sort_by_key(|f| (!f.external, f.low));
        let mut out: Vec<u32> = Vec::new();
        for f in fns {
            if !out.contains(&f.low) {
                out.push(f.low);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(normalize_path(r"C:\Workspace\app\src\a.c"), "c:/workspace/app/src/a.c");
        assert_eq!(normalize_path("../../src/x/./y.c"), "src/x/y.c");
        assert_eq!(normalize_path("a/b/../c.c"), "a/c.c");
        assert_eq!(join_path(r"C:\build", "x.c"), r"C:\build\x.c");
    }

    pub(crate) fn test_elf() -> Option<PathBuf> {
        let p = PathBuf::from(std::env::var_os("AURIX_TEST_ELF")?);
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
        // Any function with line info: lookups by address, file:line and name agree.
        let f = s
            .functions
            .iter()
            .find(|f| f.high > f.low && s.line_at(f.low).is_some() && s.resolve_location(&f.name) == Some(f.low))
            .expect("a function with line info");
        assert_eq!(s.function_at(f.low).map(|x| x.low), Some(f.low));
        let li = s.line_at(f.low).expect("line info");
        let (line, addrs) = s.resolve_line(&li.file, li.line).unwrap();
        assert_eq!(line, li.line);
        assert!(addrs.contains(&li.start) || addrs.iter().any(|a| f.contains(*a)), "{addrs:08x?}");
        assert!(!s.lines_in_range(li.start, li.start + 1).is_empty());
        let g = s.globals.iter().find(|g| g.addr != 0).expect("a global");
        assert!(s.globals_named(&g.name).iter().any(|x| x.addr == g.addr), "{}", g.name);
    }
}
