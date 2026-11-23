//! Variables found by name: the children of an evaluated expression, and the search a search box
//! runs over the globals and the members of the struct globals.
//!
//! Both front doors use this: `list_globals` (REST, MCP) and the `searchSymbols` command of the
//! VS Code variable picker, so a search finds the same variables everywhere, named by expressions
//! that [`evaluate_global`] resolves.
//!
//! A search runs over an index built once per symbol set: every global, and every member path
//! ([`Symbols::member_paths`]), with its name lower-cased. A keystroke is one pass over a few
//! megabytes of text, ranked, of which only the best page is described. What the index does not
//! hold (array elements, members deeper than it goes, a core suffix) is found by evaluating the
//! parent, as `symbol_children` does.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex, Weak};

use memchr::memmem;
use symbols::expr::{Eval, Place, SymbolRef, Target};
use symbols::types::{MemberInfo, TypeClass, TypeId, MEMBER_PATHS_MAX, MEMBER_PATH_DEPTH};
use symbols::Symbols;

use crate::{elf_id, evaluate_global, split_core, GlobalValue, Shared, SymbolSet};

// --- Names -----------------------------------------------------------------------------------

/// Case-insensitive glob match: `*` any run of characters, `?` exactly one.
pub fn glob(pattern: &[u8], name: &[u8]) -> bool {
    let (mut p, mut n) = (0, 0);
    // Where the last `*` was, and the name position it currently swallows up to.
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p].eq_ignore_ascii_case(&name[n])) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some((p, n));
            p += 1;
        } else if let Some((sp, sn)) = star {
            p = sp + 1;
            n = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&c| c == b'*')
}

/// The expression that names global `i` of the ELF at position `p`: its plain name if a lookup
/// of that name finds it, else a symbol reference.
///
/// A lookup tries the ELF files in order and takes the first that knows the name (as a global,
/// a function or an enumerator); within one file it prefers an external definition to a static.
pub fn global_expression(set: &SymbolSet, p: usize, i: usize) -> String {
    let (n, syms) = &set.elfs[p];
    let name = &syms.globals[i].name;
    let earlier = set.elfs[..p].iter().any(|(_, s)| {
        !s.global_indices(name).is_empty() || !s.function_indices(name).is_empty() || !s.enumerators_named(name).is_empty()
    });
    let gi = syms.global_indices(name);
    let found = gi.iter().copied().find(|&j| syms.globals[j].external).or(gi.first().copied());
    if symbols::is_expression_name(name) && !earlier && found == Some(i) {
        name.clone()
    } else {
        format!("${{{}}}", elf_id(*n, &format!("GV:{i}")))
    }
}

/// Evaluation needs a target, but the type of a global reads nothing.
pub struct NoMemory;

impl Target for NoMemory {
    fn pc(&self) -> Option<u32> {
        None
    }
    fn reg(&self, _: u16) -> Option<u32> {
        None
    }
    fn read(&mut self, _: u32, _: usize) -> Result<Vec<u8>, String> {
        Err("no memory access while listing".into())
    }
    fn symbol(&mut self, _: &str) -> Option<SymbolRef> {
        None
    }
}

// --- Children --------------------------------------------------------------------------------

/// `expr` ready to take `.member` or `[i]`: postfix operators bind tighter than everything but a
/// primary expression, so anything else needs parentheses (`(*p).x`, `(a + 1)[2]`).
fn postfix_base(syms: &Symbols, expr: &str) -> String {
    use symbols::expr::Expr;
    let parsed = symbols::expr::parse(expr, &|n| Eval::<NoMemory>::resolve_type(syms, n));
    match parsed {
        Ok(Expr::Ident(_) | Expr::SymRef(_) | Expr::Member(..) | Expr::Arrow(..) | Expr::Index(..)) => expr.to_string(),
        _ => format!("({expr})"),
    }
}

/// Members of a struct or union, with the members of anonymous ones lifted to where C finds them.
fn named_members(syms: &Symbols, ty: &TypeId, base: u32, out: &mut Vec<MemberInfo>) {
    for m in syms.members(ty) {
        if m.name.is_some() {
            out.push(MemberInfo { offset: base + m.offset, ..m });
        } else {
            named_members(syms, &m.ty, base + m.offset, out);
        }
    }
}

/// A member or element of an evaluated expression.
#[derive(Clone, Debug)]
pub struct Child {
    /// As in a tree: `speed`, `[3]`.
    pub name: String,
    pub expression: String,
    pub ty: TypeId,
    pub size: u32,
    /// The system-bus address, if the parent is stored at a fixed place of a known core.
    pub address: Option<u32>,
    /// (bit offset, bit size) of a bit field.
    pub bits: Option<(u32, u32)>,
}

enum Kids {
    Members(Vec<MemberInfo>),
    Elements { len: usize, elem: TypeId, size: u32 },
    None,
}

/// The members of a struct or union value, or the elements of an array, each named by an
/// expression that keeps the parent's core suffix (`g_s@2` -> `g_s.a@2`). Pointers are not
/// followed. A value in core-local RAM that no core is determined for is a `per_core` group,
/// whose children are the copies; this lists its members anyway, without addresses.
pub struct Children<'a> {
    syms: &'a Symbols,
    parent: String,
    suffix: String,
    base: Option<u32>,
    kids: Kids,
    /// The parent is a `per_core` group (see [`GlobalValue::per_core`]).
    pub per_core: bool,
}

impl<'a> Children<'a> {
    pub fn of(sh: &Shared, gv: &'a GlobalValue) -> Self {
        let syms: &Symbols = &gv.elf.1;
        let v = &gv.value;
        let info = syms.type_info(&v.ty);
        // Children of something stored at a fixed place have an address; of a computed value, or
        // of core-local RAM no core is determined for, none.
        let base = match (&v.place, &v.bytes) {
            (Place::Memory(a), None) => gv.global_address(sh, *a).ok(),
            _ => None,
        };
        let kids = match info.class {
            TypeClass::Composite => {
                let mut members = Vec::new();
                named_members(syms, &v.ty, 0, &mut members);
                Kids::Members(members)
            }
            TypeClass::Array => {
                let elem = info.base.clone().unwrap_or(TypeId::Void);
                Kids::Elements { len: info.length.unwrap_or(0) as usize, size: syms.type_info(&elem).size, elem }
            }
            _ => Kids::None,
        };
        let parent = postfix_base(syms, gv.expression());
        let suffix = gv.suffix().map(|s| s.to_string()).unwrap_or_default();
        Children { syms, parent, suffix, base, kids, per_core: gv.per_core(sh).is_some() }
    }

    pub fn len(&self) -> usize {
        match &self.kids {
            Kids::Members(m) => m.len(),
            Kids::Elements { len, .. } => *len,
            Kids::None => 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The children are array elements, not members.
    pub fn elements(&self) -> bool {
        matches!(self.kids, Kids::Elements { .. })
    }

    /// What a search matches of child `i`: a member's name, an element's index in decimal.
    pub fn label(&self, i: usize) -> Cow<'_, str> {
        match &self.kids {
            Kids::Members(m) => Cow::Borrowed(m[i].name.as_deref().unwrap_or_default()),
            _ => Cow::Owned(i.to_string()),
        }
    }

    /// Child `i` (`< len()`).
    pub fn child(&self, i: usize) -> Child {
        let (parent, suffix) = (&self.parent, &self.suffix);
        match &self.kids {
            Kids::Members(members) => {
                let m = &members[i];
                let name = m.name.clone().unwrap_or_default();
                let expression = format!("{parent}.{name}{suffix}");
                Child { name, expression, ty: m.ty.clone(), size: m.size, address: self.base.map(|a| a.wrapping_add(m.offset)), bits: m.bits }
            }
            Kids::Elements { elem, size, .. } => {
                let address = self.base.map(|a| a.wrapping_add(size.wrapping_mul(i as u32)));
                Child { name: format!("[{i}]"), expression: format!("{parent}[{i}]{suffix}"), ty: elem.clone(), size: *size, address, bits: None }
            }
            Kids::None => unreachable!("no children"),
        }
    }

    /// The symbols the children's types belong to.
    pub fn symbols(&self) -> &'a Symbols {
        self.syms
    }
}

// --- Paths -----------------------------------------------------------------------------------

/// A query for the members or elements of an expression: `g_motor.spe*`, `g_buf[1*`, or as a
/// search box wraps it, `*g_motor.spe*`.
#[derive(Debug, PartialEq)]
pub struct PathQuery<'q> {
    /// The parent: a C expression, with the query's core suffix (`g_s@2` of `g_s.a*@2`).
    pub parent: String,
    /// What the last segment matches: a member's name, or an element's index in decimal.
    pub pattern: Cow<'q, str>,
    pub element: bool,
}

/// Splits a query at its last `.` or `[` outside a `${...}` symbol reference into the parent
/// expression and a glob for its children; `None` for a plain name pattern, or when nothing is
/// left of the parent.
///
/// Leading `*` belong to the last segment rather than the parent (where `*` would be a
/// dereference): `*g_s.a*` matches every member of `g_s` whose name contains `a`, `*g_s.a` the
/// ones ending in it. An element segment is the index alone: `g_buf[1*` matches indices starting
/// with 1, and a closing `]` ends it, so `*g_buf[3]*` is element 3 only. An empty segment
/// (`g_s.`, `g_buf[`) matches every child.
pub fn path_query(query: &str) -> Option<PathQuery<'_>> {
    let (body, sel) = split_core(query);
    let mut depth = 0usize;
    let mut split = None;
    for (i, c) in body.char_indices() {
        match c {
            '{' if body[..i].ends_with('$') => depth += 1,
            '}' if depth > 0 => depth -= 1,
            '.' | '[' if depth == 0 => split = Some(i),
            _ => {}
        }
    }
    let at = split?;
    let (head, tail) = (&body[..at], &body[at + 1..]);
    let parent = head.trim_start_matches('*').trim();
    if parent.is_empty() {
        return None;
    }
    let element = body[at..].starts_with('[');
    let pattern: Cow<str> = if element {
        let open = tail.trim_end_matches('*').trim_end();
        Cow::Borrowed(open.strip_suffix(']').unwrap_or(tail).trim())
    } else if head.trim_start().starts_with('*') {
        Cow::Owned(format!("*{}", tail.trim()))
    } else {
        Cow::Borrowed(tail.trim())
    };
    let pattern = if pattern.is_empty() { Cow::Borrowed("*") } else { pattern };
    let parent = match sel {
        Some(s) => format!("{parent}{s}"),
        None => parent.to_string(),
    };
    Some(PathQuery { parent, pattern, element })
}

impl PathQuery<'_> {
    /// The children of `kids` whose label the last segment matches, in order: none when it asks
    /// for elements of a struct (`g_s[`) or members of an array (`g_buf.`).
    pub fn matches(&self, kids: &Children) -> Vec<usize> {
        if self.element != kids.elements() {
            return Vec::new();
        }
        (0..kids.len()).filter(|&i| glob(self.pattern.as_bytes(), kids.label(i).as_bytes())).collect()
    }
}

// --- Search ----------------------------------------------------------------------------------

/// A variable a search found.
#[derive(Clone)]
pub struct Hit {
    /// As a flat list shows it: a global's name, a member's whole path.
    pub name: String,
    /// The expression that names it for `evaluate`, DAQ and a live watch.
    pub expression: String,
    /// The `expression` of the variable this one is a member or element of; `None` for a
    /// global.
    pub parent: Option<String>,
    /// Not a match itself: an ancestor of a match, there so that a client can show the matches
    /// as a tree (see [`search`]).
    pub context: bool,
    pub kind: HitKind,
}

#[derive(Clone)]
pub enum HitKind {
    /// Global `index` of the ELF at position `elf` of the set.
    Global { elf: usize, index: usize },
    /// Member path `index` ([`Symbols::member_paths`]) of the ELF at position `elf`.
    Member { elf: usize, index: usize },
    /// A child of an evaluated parent the index does not hold the members of, or such a parent
    /// (or one of its ancestors) as context.
    Child { syms: Arc<Symbols>, child: Child, per_core: bool },
}

/// One page of a search.
pub struct Found {
    /// Matches in all, not counting context.
    pub total: usize,
    pub hits: Vec<Hit>,
}

/// What a variable picker shows of a hit without evaluating it: the address as the symbols give
/// it (core-local RAM untranslated), the size, and the source file that defines it.
pub fn hit_details(set: &SymbolSet, hit: &Hit) -> (Option<u32>, u32, Option<String>) {
    let file = |syms: &Symbols, unit: usize| syms.units.get(unit).map(|u| u.name.clone()).filter(|n| !n.is_empty());
    match &hit.kind {
        HitKind::Global { elf, index } => {
            let syms = &set.elfs[*elf].1;
            let g = &syms.globals[*index];
            let ty = Eval::new(syms, &mut NoMemory).value_of(&SymbolRef::Global(*index)).map(|v| v.ty).unwrap_or(TypeId::Void);
            (Some(g.addr), syms.type_info(&ty).size, file(syms, g.unit))
        }
        HitKind::Member { elf, index } => {
            let syms = &set.elfs[*elf].1;
            let m = &syms.member_paths()[*index];
            (Some(m.address), m.size, file(syms, syms.globals[m.global].unit))
        }
        HitKind::Child { child, .. } => (child.address, child.size, None),
    }
}

/// Searches the globals and member paths of `set` for `text`, as a search box means it, and
/// returns the page `offset..offset + limit` of the matches, best first.
///
/// A name matches when it contains the text (case-insensitive), when `*` (any run) and `?` (one
/// character) make it match, when the text is a run of the beginnings of its words (`dcs` for
/// `DriveCounterStamp`), or when the source file that defines it contains the text, or its
/// address starts with a `0x...` text. Better first: the whole name, a prefix, the start of a
/// word, anywhere, word beginnings, file or address; then shorter names, then by name. An empty
/// text lists the globals by name, then the members.
///
/// A text with a `.` or `[` whose parent the index holds no members of (array elements, deeper
/// members, a `${...}` reference, a core suffix) also gets the matching children of the parent
/// evaluated in context `ctx`, after the index's matches.
///
/// With `ancestors`, the page also holds, for each member or element on it, those of its
/// ancestors that are not matches on the page (outermost first, each once, before the first
/// match below it), marked as [`Hit::context`]. They do not count toward `limit` or `total`.
/// Every member and element names its parent's `expression` in [`Hit::parent`], so a client can
/// build a tree.
pub fn search(sh: &Shared, ctx: &str, set: &SymbolSet, text: &str, offset: usize, limit: usize, ancestors: bool) -> Found {
    let text = text.trim();
    let index = index(set);
    let want = offset.saturating_add(limit);
    let (indexed, ranked) = index.rank(set, text, want);
    let mut hits: Vec<Hit> = ranked.iter().skip(offset).map(|&pos| index.hit(set, pos as usize)).collect();
    let mut total = indexed;
    for child in index.unindexed_children(sh, ctx, text) {
        if total >= offset && hits.len() < limit {
            hits.push(child);
        }
        total += 1;
    }
    if ancestors {
        hits = with_ancestors(sh, ctx, set, hits);
    }
    Found { total, hits }
}

/// `hits` with the ancestors of each that are not hits themselves before it, once each.
fn with_ancestors(sh: &Shared, ctx: &str, set: &SymbolSet, hits: Vec<Hit>) -> Vec<Hit> {
    let matched: HashSet<String> = hits.iter().map(|h| h.expression.clone()).collect();
    let mut shown: HashSet<String> = HashSet::new();
    // The ancestors of an evaluated parent, by its expression: its children share them.
    let mut evaluated: HashMap<String, Vec<Hit>> = HashMap::new();
    let mut out = Vec::with_capacity(hits.len());
    for h in hits {
        let chain = match &h.kind {
            HitKind::Global { .. } => Vec::new(),
            HitKind::Member { elf, index } => member_ancestors(set, *elf, *index),
            HitKind::Child { .. } => match &h.parent {
                Some(p) => evaluated.entry(p.clone()).or_insert_with(|| evaluated_ancestors(sh, ctx, p)).clone(),
                None => Vec::new(),
            },
        };
        for a in chain {
            if !matched.contains(&a.expression) && shown.insert(a.expression.clone()) {
                out.push(a);
            }
        }
        out.push(h);
    }
    out
}

/// The global and the enclosing members of member path `index` of the ELF at `elf`, outermost
/// first, as context.
fn member_ancestors(set: &SymbolSet, elf: usize, index: usize) -> Vec<Hit> {
    let syms = &set.elfs[elf].1;
    let paths = syms.member_paths();
    let mut chain = Vec::new();
    let mut up = paths[index].parent;
    while let Some(k) = up {
        chain.push(Hit { context: true, ..member_hit(set, elf, k) });
        up = paths[k].parent;
    }
    let g = paths[index].global;
    chain.push(Hit { context: true, ..global_hit(set, elf, g) });
    chain.reverse();
    chain
}

/// The variable `text` evaluates to and its ancestors, outermost first, as context: each the
/// parent `path_query` splits off (`g_s.arr[2]@1` -> `g_s.arr@1` -> `g_s@1`). An ancestor that
/// does not evaluate ends the chain.
fn evaluated_ancestors(sh: &Shared, ctx: &str, text: &str) -> Vec<Hit> {
    let mut chain = Vec::new();
    let mut cur = text.to_string();
    // A guard against a parent that splits off nothing.
    for _ in 0..32 {
        let Ok(gv) = evaluate_global(sh, ctx, &cur) else { break };
        let parent = path_query(&cur).map(|q| q.parent).filter(|p| *p != cur);
        let child = value_child(sh, &gv, &cur);
        let per_core = gv.per_core(sh).is_some();
        chain.push(Hit {
            name: cur.clone(),
            expression: cur.clone(),
            parent: parent.clone(),
            context: true,
            kind: HitKind::Child { syms: gv.elf.1.clone(), child, per_core },
        });
        match parent {
            Some(p) => cur = p,
            None => break,
        }
    }
    chain.reverse();
    chain
}

/// An evaluated variable described like a child, named `text`.
fn value_child(sh: &Shared, gv: &GlobalValue, text: &str) -> Child {
    let syms: &Symbols = &gv.elf.1;
    let v = &gv.value;
    let (address, size, bits) = match (&v.place, &v.bytes) {
        (Place::Bits { addr, storage, shift, bits }, None) => (gv.global_address(sh, *addr).ok(), *storage, Some((*shift, *bits))),
        _ => (gv.stored_at().and_then(|a| gv.global_address(sh, a).ok()), syms.type_info(&v.ty).size, None),
    };
    Child { name: text.to_string(), expression: text.to_string(), ty: v.ty.clone(), size, address, bits }
}

fn global_hit(set: &SymbolSet, elf: usize, index: usize) -> Hit {
    let name = set.elfs[elf].1.globals[index].name.clone();
    Hit { name, expression: global_expression(set, elf, index), parent: None, context: false, kind: HitKind::Global { elf, index } }
}

fn member_hit(set: &SymbolSet, elf: usize, index: usize) -> Hit {
    let syms = &set.elfs[elf].1;
    let m = &syms.member_paths()[index];
    let parent = match m.parent {
        Some(k) => syms.member_paths()[k].expression.clone(),
        None => global_expression(set, elf, m.global),
    };
    Hit { name: m.expression.clone(), expression: m.expression.clone(), parent: Some(parent), context: false, kind: HitKind::Member { elf, index } }
}

/// Symbol sets whose index is kept: usually every core has the same files.
const SEARCH_SETS: usize = 2;

static INDEXES: Mutex<Vec<Arc<SearchIndex>>> = Mutex::new(Vec::new());

/// The index of `set`, built on first use.
fn index(set: &SymbolSet) -> Arc<SearchIndex> {
    let mut cache = INDEXES.lock().unwrap();
    // An index of files no longer loaded only holds memory.
    cache.retain(|x| x.elfs.iter().all(|(_, w)| w.strong_count() > 0));
    if let Some(i) = cache.iter().position(|x| x.indexes(set)) {
        let hit = cache.remove(i);
        cache.push(hit.clone());
        return hit;
    }
    let built = Arc::new(SearchIndex::build(set));
    if cache.len() >= SEARCH_SETS {
        cache.remove(0);
    }
    cache.push(built.clone());
    built
}

struct Entry {
    /// Position of the ELF in the set.
    elf: u16,
    member: bool,
    /// Global index, or member path index.
    index: u32,
    /// The lower-cased name in `keys`.
    key: u32,
    len: u32,
    /// Compilation unit that defines it (of the global a member belongs to).
    unit: u32,
    addr: u32,
}

/// The searchable names of a symbol set, sorted case-insensitively.
struct SearchIndex {
    /// The ELF numbers and contents indexed; the index keeps neither alive.
    elfs: Vec<(usize, Weak<Symbols>)>,
    entries: Vec<Entry>,
    keys: Vec<u8>,
    /// Per ELF, per compilation unit: the source file's name, lower-cased.
    files: Vec<Vec<String>>,
    /// Hashes of the expressions whose members the index holds: the struct globals a plain name
    /// finds, and the member paths.
    walked: HashSet<u64>,
    /// Some ELF has more member paths than [`MEMBER_PATHS_MAX`]: the index holds only the first.
    truncated: bool,
}

fn hash(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Match quality, best first.
#[derive(Clone, Copy)]
enum Tier {
    Equal,
    Prefix,
    WordStart,
    Substring,
    Initials,
    File,
}

impl SearchIndex {
    fn indexes(&self, set: &SymbolSet) -> bool {
        self.elfs.len() == set.elfs.len() && self.elfs.iter().zip(&set.elfs).all(|((n, w), (m, s))| n == m && w.as_ptr() == Arc::as_ptr(s))
    }

    fn build(set: &SymbolSet) -> Self {
        let mut entries = Vec::new();
        let mut keys = Vec::new();
        let mut walked = HashSet::new();
        let mut truncated = false;
        let mut push = |elf: usize, member: bool, index: usize, name: &str, unit: usize, addr: u32| {
            let key = keys.len() as u32;
            keys.extend(name.bytes().map(|b| b.to_ascii_lowercase()));
            let unit = u32::try_from(unit).unwrap_or(u32::MAX);
            entries.push(Entry { elf: elf as u16, member, index: index as u32, key, len: name.len() as u32, unit, addr });
        };
        for (p, (_, syms)) in set.elfs.iter().enumerate() {
            // As `list_globals` lists them: a DWARF global at address 0 is a definition the
            // linker dropped.
            for (i, g) in syms.globals.iter().enumerate() {
                if g.addr != 0 || g.unit == usize::MAX {
                    push(p, false, i, &g.name, g.unit, g.addr);
                }
            }
            // As the member paths name them: a global whose name an earlier file has too is
            // named by a symbol reference, which no member path starts with.
            let paths = syms.member_paths();
            truncated |= paths.len() >= MEMBER_PATHS_MAX;
            let earlier = &set.elfs[..p];
            for (k, m) in paths.iter().enumerate() {
                let root = &syms.globals[m.global];
                if earlier.iter().any(|(_, e)| !e.global_indices(&root.name).is_empty()) {
                    continue;
                }
                push(p, true, k, &m.expression, root.unit, m.address);
                walked.insert(hash(&root.name));
                walked.insert(hash(&m.expression));
            }
        }
        let key = |e: &Entry| &keys[e.key as usize..(e.key + e.len) as usize];
        entries.sort_unstable_by(|a, b| key(a).cmp(key(b)).then(a.member.cmp(&b.member)).then(a.elf.cmp(&b.elf)).then(a.index.cmp(&b.index)));
        let files = set
            .elfs
            .iter()
            .map(|(_, s)| s.units.iter().map(|u| u.name.rsplit(['/', '\\']).next().unwrap_or_default().to_ascii_lowercase()).collect())
            .collect();
        let elfs = set.elfs.iter().map(|(n, s)| (*n, Arc::downgrade(s))).collect();
        SearchIndex { elfs, entries, keys, files, walked, truncated }
    }

    fn key(&self, e: &Entry) -> &[u8] {
        &self.keys[e.key as usize..(e.key + e.len) as usize]
    }

    /// The name as written, for the case of its letters.
    fn name<'s>(&self, set: &'s SymbolSet, e: &Entry) -> &'s str {
        let syms = &set.elfs[e.elf as usize].1;
        if e.member {
            &syms.member_paths()[e.index as usize].expression
        } else {
            &syms.globals[e.index as usize].name
        }
    }

    fn hit(&self, set: &SymbolSet, pos: usize) -> Hit {
        let e = &self.entries[pos];
        if e.member {
            member_hit(set, e.elf as usize, e.index as usize)
        } else {
            global_hit(set, e.elf as usize, e.index as usize)
        }
    }

    /// The number of entries that match `text`, and the positions of the best `want` of them,
    /// best first.
    fn rank(&self, set: &SymbolSet, text: &str, want: usize) -> (usize, Vec<u32>) {
        if text.is_empty() {
            let globals = self.entries.iter().enumerate().filter(|(_, e)| !e.member);
            let members = self.entries.iter().enumerate().filter(|(_, e)| e.member);
            return (self.entries.len(), globals.chain(members).take(want).map(|(p, _)| p as u32).collect());
        }
        let q = text.to_ascii_lowercase();
        let q = q.as_bytes();
        let wild = q.contains(&b'*') || q.contains(&b'?');
        let (prefix, anywhere) = ([q, b"*"].concat(), [b"*", q, b"*"].concat());
        let finder = memmem::Finder::new(q);
        let address = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")).filter(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_hexdigit()));
        let address = address.map(|h| format!("0x{}", h.to_ascii_lowercase()));
        // (tier, length, position): sorting these sorts the matches best first.
        let mut found: Vec<u64> = Vec::new();
        for (pos, e) in self.entries.iter().enumerate() {
            let key = self.key(e);
            let tier = if wild {
                if glob(&prefix, key) {
                    Some(if glob(q, key) { Tier::Equal } else { Tier::Prefix })
                } else if glob(&anywhere, key) {
                    Some(Tier::Substring)
                } else {
                    None
                }
            } else {
                match finder.find(key) {
                    Some(0) => Some(if key.len() == q.len() { Tier::Equal } else { Tier::Prefix }),
                    Some(at) => {
                        let name = self.name(set, e).as_bytes();
                        let mut i = at;
                        let mut tier = Tier::Substring;
                        loop {
                            if word_start(name, i) {
                                tier = Tier::WordStart;
                                break;
                            }
                            match finder.find(&key[i + 1..]) {
                                Some(n) => i += 1 + n,
                                None => break,
                            }
                        }
                        Some(tier)
                    }
                    None if subsequence(q, key) && initials(q, key, self.name(set, e).as_bytes()) => Some(Tier::Initials),
                    None => None,
                }
            };
            let tier = tier.or_else(|| {
                let file = self.files.get(e.elf as usize).and_then(|f| f.get(e.unit as usize)).map(String::as_bytes).unwrap_or_default();
                let in_file = if wild { glob(&anywhere, file) } else { finder.find(file).is_some() };
                let at_address = address.as_ref().is_some_and(|a| format!("0x{:x}", e.addr).starts_with(a.as_str()));
                (in_file || at_address).then_some(Tier::File)
            });
            if let Some(t) = tier {
                found.push(((t as u64) << 56) | ((e.len.min(0xFF_FFFF) as u64) << 32) | pos as u64);
            }
        }
        let total = found.len();
        if found.len() > want {
            if want == 0 {
                found.clear();
            } else {
                found.select_nth_unstable(want - 1);
                found.truncate(want);
            }
        }
        found.sort_unstable();
        (total, found.into_iter().map(|k| k as u32).collect())
    }

    /// The children of the parent `text` names that the index does not hold, matched by its last
    /// segment: elements, members deeper than the index goes or beyond its limit, and children
    /// named with a `${...}` reference or a core suffix. Empty without a `.` or `[`, or when the
    /// parent does not evaluate.
    fn unindexed_children(&self, sh: &Shared, ctx: &str, text: &str) -> Vec<Hit> {
        // The last segment is matched anywhere in a child's name, as the whole text is in a name.
        let (body, sel) = split_core(text);
        let wrapped = match sel {
            Some(s) => format!("*{body}*{s}"),
            None => format!("*{body}*"),
        };
        let Some(q) = path_query(&wrapped) else { return Vec::new() };
        let depth = q.parent.matches('.').count() as u32 + 1;
        let held = !q.element && !self.truncated && depth < MEMBER_PATH_DEPTH && self.walked.contains(&hash(&q.parent));
        if held {
            return Vec::new();
        }
        let Ok(gv) = evaluate_global(sh, ctx, &q.parent) else { return Vec::new() };
        let kids = Children::of(sh, &gv);
        q.matches(&kids)
            .into_iter()
            .map(|i| kids.child(i))
            // A member the index holds after all (it reached its limit, or the depth) is found
            // by the index if it matches.
            .filter(|c| !self.walked.contains(&hash(&c.expression)))
            .map(|child| Hit {
                name: child.expression.clone(),
                expression: child.expression.clone(),
                parent: Some(q.parent.clone()),
                context: false,
                kind: HitKind::Child { syms: gv.elf.1.clone(), child, per_core: kids.per_core },
            })
            .collect()
    }
}

/// A word of a name starts at `i`: at its beginning, after a character that is not a letter or
/// digit, at an upper-case letter after a lower-case one, and where digits start or end.
fn word_start(name: &[u8], i: usize) -> bool {
    let Some(&c) = name.get(i) else { return false };
    if i == 0 {
        return true;
    }
    let prev = name[i - 1];
    !prev.is_ascii_alphanumeric() || (c.is_ascii_uppercase() && prev.is_ascii_lowercase()) || (c.is_ascii_digit() != prev.is_ascii_digit())
}

/// The characters of `q` occur in `key` in order: needed for [`initials`], and cheap.
fn subsequence(q: &[u8], key: &[u8]) -> bool {
    let mut it = key.iter();
    q.iter().all(|c| it.any(|k| k == c))
}

/// `q` is a run of beginnings of the words of `name` (`key` lower-cased), in order: `dcs` and
/// `drcost` match `Drive_CounterStamp`.
fn initials(q: &[u8], key: &[u8], name: &[u8]) -> bool {
    let starts: Vec<usize> = (0..name.len()).filter(|&i| name[i].is_ascii_alphanumeric() && word_start(name, i)).collect();
    if starts.len() < 2 || q.len() > 64 {
        return false;
    }
    // memo[qi * (words + 1) + w]: whether q[qi..] matches from word w on; 0 unknown, 1 no, 2 yes.
    let words = starts.len();
    let mut memo = vec![0u8; (q.len() + 1) * (words + 1)];
    fn rest(q: &[u8], qi: usize, w: usize, key: &[u8], starts: &[usize], memo: &mut [u8]) -> bool {
        if qi == q.len() {
            return true;
        }
        let slot = qi * (starts.len() + 1) + w;
        if memo[slot] != 0 {
            return memo[slot] == 2;
        }
        let mut ok = false;
        'words: for s in w..starts.len() {
            let end = starts.get(s + 1).copied().unwrap_or(key.len());
            let mut n = 0;
            while qi + n < q.len() && starts[s] + n < end && key[starts[s] + n] == q[qi + n] {
                n += 1;
            }
            for take in (1..=n).rev() {
                if rest(q, qi + take, s + 1, key, starts, memo) {
                    ok = true;
                    break 'words;
                }
            }
        }
        memo[slot] = if ok { 2 } else { 1 };
        ok
    }
    rest(q, 0, 0, key, &starts, &mut memo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_patterns() {
        let m = |p: &str, n: &str| glob(p.as_bytes(), n.as_bytes());
        assert!(m("*speed*", "g_Motor_SPEED_rpm"));
        assert!(m("g_?otor", "g_motor"));
        assert!(m("*", ""));
        assert!(m("a*b*c", "aXXbYYc"));
        assert!(m("a*b*c", "abcbc"));
        assert!(!m("a*b*c", "aXXbYY"));
        assert!(!m("motor", "g_motor"), "a pattern matches the whole name");
        assert!(!m("g_?otor", "g_otor"));
        assert!(m("**x", "abx"));
    }

    #[test]
    fn path_queries() {
        let q = |s: &str| path_query(s).map(|q| (q.parent, q.pattern.into_owned(), q.element));
        let member = |p: &str, pat: &str| Some((p.to_string(), pat.to_string(), false));
        let element = |p: &str, pat: &str| Some((p.to_string(), pat.to_string(), true));
        assert_eq!(q("g_speed*"), None, "a plain name pattern");
        assert_eq!(q("Drive_Counter_0.DriveCounterStamp"), member("Drive_Counter_0", "DriveCounterStamp"));
        // A search box's `*…*`: the leading wildcard moves to the last segment.
        assert_eq!(q("*Drive_Counter_0.DriveCounterStamp*"), member("Drive_Counter_0", "*DriveCounterStamp*"));
        assert_eq!(q("*g_s.a"), member("g_s", "*a"));
        assert_eq!(q("g_s.a*"), member("g_s", "a*"));
        assert_eq!(q("g_s.inner.x"), member("g_s.inner", "x"));
        assert_eq!(q("g_s.arr[2].x"), member("g_s.arr[2]", "x"));
        assert_eq!(q("g_s."), member("g_s", "*"));
        assert_eq!(q("*g_s.*"), member("g_s", "**"));
        // Elements: the index alone; a closing `]` ends it.
        assert_eq!(q("g_buf[3]"), element("g_buf", "3"));
        assert_eq!(q("*g_buf[3]*"), element("g_buf", "3"));
        assert_eq!(q("*g_buf[1*"), element("g_buf", "1*"));
        assert_eq!(q("g_m[1][2]"), element("g_m[1]", "2"));
        assert_eq!(q("g_buf["), element("g_buf", "*"));
        // The core suffix goes to the parent; a `${...}` reference is one piece.
        assert_eq!(q("g_s.a*@2"), member("g_s@2", "a*"));
        assert_eq!(q("${E0/GV:12}.a"), member("${E0/GV:12}", "a"));
        // Nothing left of the parent: a wildcard over it is not searched this way.
        assert_eq!(q("*.DriveCounterStamp"), None);
        assert_eq!(q(".a"), None);
    }

    #[test]
    fn word_beginnings() {
        let m = |q: &str, name: &str| initials(q.as_bytes(), name.to_ascii_lowercase().as_bytes(), name.as_bytes());
        assert!(m("dcs", "DriveCounterStamp"));
        assert!(m("drcost", "Drive_CounterStamp"));
        assert!(m("ddcs", "Drive_Counter_0.DriveCounterStamp"));
        assert!(m("c0dcs", "Drive_Counter_0.DriveCounterStamp"));
        assert!(!m("dsc", "DriveCounterStamp"), "in order");
        assert!(!m("dcx", "DriveCounterStamp"));
        assert!(!m("ab", "abc"), "one word: that is a prefix, not initials");
        assert!(word_start(b"g_cnt", 2) && word_start(b"gCnt", 1) && word_start(b"buf10", 3) && !word_start(b"count", 2));
    }
}
