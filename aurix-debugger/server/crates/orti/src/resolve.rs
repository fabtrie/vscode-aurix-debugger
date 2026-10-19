//! Turning ORTI attributes into values a panel can show.
//!
//! An ORTI attribute is a C expression, but the expressions fall into three very different
//! classes and it matters which one you have:
//!
//! * **Constant.** `vs_SMP_NUMCPU = "2"`, `STACK = "&OsCfg_Stack_OsCore0_Kernel"`,
//!   `SIZE = "sizeof(OsCfg_Stack_OsCore0_Error_Dyn)"`. Resolved once when the file is loaded and
//!   never read from the target again.
//! * **Direct.** `RUNNINGTASK[0] = "OsCfg_Trace_OsCore0_Dyn.CurrentTask"`. A single lvalue at a
//!   fixed address, so it becomes exactly one DAQ point and the sampled bytes *are* the value.
//!   Most live attributes are this, which is why a whole task table costs so few bus accesses.
//! * **Computed.** `STATE = "…OsState == 2 ? ( … ? 0 : …State ) : 0xFF"`. There is no single
//!   address to sample. The planner evaluates it once against the target and records every read
//!   it made; those reads become the DAQ points, and each refresh re-evaluates the expression
//!   over the sampled bytes.
//!
//! A ternary makes the recorded read set **branch dependent**: planning while core 0 is down
//! records the `0xFF` branch and never learns the address of the task's `State` field. So a
//! refresh that reaches an unsampled address is not an error - [`Snapshot`] falls back to a live
//! read and sets [`Snapshot::missed`], which tells the caller to re-plan the attribute.

use std::collections::BTreeMap;

use crate::{AttrDecl, AttrKind, EnumValue};
use symbols::expr::{Eval, Expr, Place, SymbolRef, Target};
use symbols::Symbols;

/// Hex for a value too wide to be a number.
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// One contiguous read an attribute needs from the target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Read {
    pub addr: u32,
    pub len: u32,
}

/// How an attribute's value is produced.
#[derive(Clone, Debug, PartialEq)]
pub enum Plan {
    /// A `STRING` attribute: literal prose, never evaluated. `ACTION = "SET_EVENT task ev"`.
    Text(String),
    /// Needs no target access. The bytes are the value, little endian.
    Const(Vec<u8>),
    /// A single lvalue at a fixed address: one DAQ point.
    Direct { addr: u32, size: u32, bits: Option<(u32, u32)> },
    /// Re-evaluated every refresh. `reads` is what it touched when planned.
    Computed(Vec<Read>),
}

impl Plan {
    /// The reads to register as DAQ points, coalescing is the sampler's job.
    pub fn reads(&self) -> Vec<Read> {
        match self {
            Plan::Text(_) | Plan::Const(_) => Vec::new(),
            // A bit field is sampled as the whole storage word it sits in.
            Plan::Direct { addr, size, .. } => vec![Read { addr: *addr, len: *size }],
            Plan::Computed(r) => r.clone(),
        }
    }

    /// Whether the value can change while the target runs.
    pub fn is_live(&self) -> bool {
        matches!(self, Plan::Direct { .. } | Plan::Computed(_))
    }
}

/// One planned attribute of one object.
#[derive(Clone, Debug)]
pub struct Attribute {
    pub name: String,
    /// The logical core, for an attribute declared `NAME[]`.
    pub index: Option<u32>,
    /// The attribute text exactly as the ORTI file wrote it, so an attribute whose plan went
    /// stale (a ternary took the other branch) can be planned again without the file.
    pub raw: String,
    pub plan: Plan,
    /// Kept for [`Plan::Computed`], which re-evaluates it each refresh.
    pub expr: Option<Expr>,
    /// Display names by ordinal, taken from the **ORTI declaration**, not from DWARF. This is
    /// what turns a task's stored priority ordinal back into "420".
    pub enums: Vec<(i64, String)>,
    /// The object an enumerator points at, by ordinal, for follow-the-link.
    pub links: Vec<(i64, String)>,
    /// Why the attribute could not be planned. It is still listed, so a panel can show the
    /// reason in the cell instead of dropping the row.
    pub error: Option<String>,
}

impl Attribute {
    fn failed(decl: &AttrDecl, index: Option<u32>, raw: &str, error: String) -> Attribute {
        Attribute {
            name: decl.name.clone(),
            index,
            raw: raw.to_string(),
            plan: Plan::Const(Vec::new()),
            expr: None,
            enums: Vec::new(),
            links: Vec::new(),
            error: Some(error),
        }
    }

    /// The display text for a value, from the ORTI enumerator list.
    pub fn enum_name(&self, v: i64) -> Option<&str> {
        self.enums.iter().find(|(k, _)| *k == v).map(|(_, n)| n.as_str())
    }

    /// Raw bytes turned into something a panel can put in a cell.
    pub fn show(&self, bytes: &[u8]) -> Shown {
        let plain = |text: String| Shown { text, number: None, link: None };
        if let Plan::Text(s) = &self.plan {
            return plain(s.clone());
        }
        if bytes.is_empty() {
            return plain(String::new());
        }
        if bytes.len() > 8 {
            return plain(hex(bytes));
        }
        let mut raw = 0u64;
        for (i, &b) in bytes.iter().enumerate() {
            raw |= (b as u64) << (8 * i);
        }
        let v = raw as i64;
        Shown {
            text: self.enum_name(v).map_or_else(|| v.to_string(), str::to_string),
            number: Some(v),
            link: self.links.iter().find(|(k, _)| *k == v).map(|(_, l)| l.clone()),
        }
    }

}

/// A value as a panel should render it.
#[derive(Clone, Debug, PartialEq)]
pub struct Shown {
    /// Always renderable: an enumerator name, a number, or literal text.
    pub text: String,
    /// The raw integer behind `text`, when there is one.
    pub number: Option<i64>,
    /// The object this value points at, when the matching enumerator names one. This is what
    /// makes a task's stack, or a spinlock's owner, clickable.
    pub link: Option<String>,
}

/// A target that reaches nothing. Used to name the type parameter when calling an associated
/// function of [`Eval`] that does not need a target, and as a stand-in where reads are a bug.
pub struct NoTarget;

impl Target for NoTarget {
    fn pc(&self) -> Option<u32> {
        None
    }
    fn reg(&self, _: u16) -> Option<u32> {
        None
    }
    fn read(&mut self, addr: u32, _: usize) -> Result<Vec<u8>, String> {
        Err(format!("0x{addr:08x} is not readable here"))
    }
    fn symbol(&mut self, _: &str) -> Option<SymbolRef> {
        None
    }
}

/// A `Target` that notes every read an evaluation makes.
struct Recorder<'a> {
    inner: &'a mut dyn Target,
    reads: Vec<Read>,
}

impl Target for Recorder<'_> {
    fn pc(&self) -> Option<u32> {
        None
    }
    fn reg(&self, _: u16) -> Option<u32> {
        None
    }
    fn read(&mut self, addr: u32, len: usize) -> Result<Vec<u8>, String> {
        self.reads.push(Read { addr, len: len as u32 });
        self.inner.read(addr, len)
    }
    fn symbol(&mut self, _: &str) -> Option<SymbolRef> {
        None
    }
}

/// Reads served from sampled bytes, falling back to the target for anything not covered.
///
/// The fallback is what makes a branch change safe: the value is right immediately, and
/// [`Self::missed`] tells the caller the read set has moved and the attribute wants re-planning.
pub struct Snapshot<'a> {
    blocks: BTreeMap<u32, Vec<u8>>,
    fallback: Option<&'a mut dyn Target>,
    /// Set when a read could not be served from the samples.
    pub missed: bool,
}

impl<'a> Snapshot<'a> {
    pub fn new(fallback: Option<&'a mut dyn Target>) -> Snapshot<'a> {
        Snapshot { blocks: BTreeMap::new(), fallback, missed: false }
    }

    /// Adds sampled bytes. Later blocks win, so a fresher sample replaces an older one.
    ///
    /// Used by the sampling path; a one-shot REST read leaves the snapshot empty and lets every
    /// read fall through to the target.
    #[allow(dead_code)]
    pub fn insert(&mut self, addr: u32, bytes: Vec<u8>) {
        self.blocks.insert(addr, bytes);
    }

    fn served(&self, addr: u32, len: usize) -> Option<Vec<u8>> {
        // The block starting at or before `addr` is the only one that can cover it.
        let (&start, bytes) = self.blocks.range(..=addr).next_back()?;
        let off = (addr - start) as usize;
        bytes.get(off..off + len).map(<[u8]>::to_vec)
    }
}

impl Target for Snapshot<'_> {
    fn pc(&self) -> Option<u32> {
        None
    }
    fn reg(&self, _: u16) -> Option<u32> {
        None
    }
    fn read(&mut self, addr: u32, len: usize) -> Result<Vec<u8>, String> {
        if let Some(b) = self.served(addr, len) {
            return Ok(b);
        }
        self.missed = true;
        match &mut self.fallback {
            Some(t) => t.read(addr, len),
            None => Err(format!("0x{addr:08x} was not sampled")),
        }
    }
    fn symbol(&mut self, _: &str) -> Option<SymbolRef> {
        None
    }
}

/// The enumerator tables, with every value reduced to the integer the target will hold.
///
/// Most enumerators are ordinals, but the interesting ones are addresses:
/// `"IdleTask_OsCore0" = "&OsCfg_Trace_IdleTask_OsCore0"`. Without evaluating those, `RUNNINGTASK`
/// shows a bare pointer instead of the name of the task that is running. They need symbols but no
/// target - taking an address reads nothing - so an enumerator that does want a read is dropped
/// rather than given a value that was true once.
fn enum_tables(syms: &Symbols, decl: &AttrDecl) -> (Vec<(i64, String)>, Vec<(i64, String)>) {
    let mut names = Vec::new();
    let mut links = Vec::new();
    let types = |n: &str| Eval::<NoTarget>::resolve_type(syms, n);
    for e in &decl.enums {
        let v = match &e.value {
            EnumValue::Const(v) => *v,
            EnumValue::Expr(x) => match parse_int(x).or_else(|| const_value(syms, x, &types)) {
                Some(v) => v,
                None => continue,
            },
        };
        names.push((v, e.name.clone()));
        if let Some(l) = &e.link {
            links.push((v, l.clone()));
        }
    }
    (names, links)
}

/// Evaluates an enumerator expression that needs no target access, such as `&OsCfg_Trace_Idle`
/// or `~((Os_Hal_AddressType)0)`.
fn const_value(syms: &Symbols, text: &str, types: &dyn Fn(&str) -> Option<symbols::types::TypeId>) -> Option<i64> {
    let e = symbols::expr::parse(text, types).ok()?;
    let mut t = NoTarget;
    let mut ev = Eval::new(syms, &mut t);
    let v = ev.eval(&e).ok()?;
    let b = ev.load(&v).ok()?;
    let mut raw = 0u64;
    for (i, &x) in b.iter().take(8).enumerate() {
        raw |= (x as u64) << (8 * i);
    }
    Some(raw as i64)
}

/// An integer literal as ORTI writes them in quotes: `"2"`, `"0x02"`, `"0xFF"`.
fn parse_int(s: &str) -> Option<i64> {
    let s = s.trim();
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r.trim()),
        None => (false, s),
    };
    let v = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => i64::from_str_radix(h, 16).ok().or_else(|| u64::from_str_radix(h, 16).ok().map(|v| v as i64))?,
        None => s.parse::<i64>().ok()?,
    };
    Some(if neg { -v } else { v })
}

/// Plans one attribute.
///
/// `target` is read from: planning a computed attribute means evaluating it once. That is a
/// handful of reads per attribute, done when the ORTI file is loaded, not per refresh.
pub fn plan(syms: &Symbols, target: &mut dyn Target, decl: &AttrDecl, index: Option<u32>, raw: &str) -> Attribute {
    if decl.kind == AttrKind::String {
        return Attribute {
            name: decl.name.clone(),
            index,
            raw: raw.to_string(),
            plan: Plan::Text(raw.to_string()),
            expr: None,
            enums: Vec::new(),
            links: Vec::new(),
            error: None,
        };
    }

    let (enums, links) = enum_tables(syms, decl);
    let types = |n: &str| Eval::<NoTarget>::resolve_type(syms, n);
    let expr = match symbols::expr::parse(raw, &types) {
        Ok(e) => e,
        Err(e) => return Attribute::failed(decl, index, raw, format!("{e} in `{raw}`")),
    };

    let mut rec = Recorder { inner: target, reads: Vec::new() };
    let value = match Eval::new(syms, &mut rec).eval(&expr) {
        Ok(v) => v,
        Err(e) => {
            let mut a = Attribute::failed(decl, index, raw, format!("{e} in `{raw}`"));
            a.enums = enums;
            a.links = links;
            return a;
        }
    };
    let reads = rec.reads;

    let plan = match (&value.place, reads.is_empty()) {
        // An lvalue reached without reading anything is a fixed address: one point.
        (Place::Memory(a), true) => {
            let size = syms.type_info(&value.ty).size.max(1);
            Plan::Direct { addr: *a, size, bits: None }
        }
        (Place::Bits { addr, storage, shift, bits }, true) => {
            Plan::Direct { addr: *addr, size: (*storage).max(1), bits: Some((*shift, *bits)) }
        }
        // A computed value that needed no reads is constant for the life of the program.
        (Place::None, true) => match value.bytes.clone() {
            Some(b) => Plan::Const(b),
            None => return Attribute::failed(decl, index, raw, format!("`{raw}` produced no value")),
        },
        (Place::Register(_), _) => {
            return Attribute::failed(decl, index, raw, format!("`{raw}` is a register value, which has no address to sample"))
        }
        // Anything that touched memory on the way: re-evaluate over samples.
        _ => Plan::Computed(merge(reads)),
    };

    Attribute { name: decl.name.clone(), index, raw: raw.to_string(), plan, expr: Some(expr), enums, links, error: None }
}

/// Reads one planned attribute straight from the target: a one-shot refresh with no sampling.
///
/// `Text` and `Const` never touch the target, which on a real file is nearly half of every table.
pub fn value_live(syms: &Symbols, target: &mut dyn Target, attr: &Attribute) -> Result<Vec<u8>, String> {
    let mut snap = Snapshot::new(Some(target));
    value(syms, &mut snap, attr)
}

/// Sorts reads and joins the ones that touch or overlap, so the same word is not sampled twice.
fn merge(mut reads: Vec<Read>) -> Vec<Read> {
    reads.sort();
    let mut out: Vec<Read> = Vec::with_capacity(reads.len());
    for r in reads {
        match out.last_mut() {
            Some(p) if r.addr <= p.addr + p.len => {
                let end = (r.addr + r.len).max(p.addr + p.len);
                p.len = end - p.addr;
            }
            _ => out.push(r),
        }
    }
    out
}

/// Reads one planned attribute's bytes out of a snapshot of sampled memory.
///
/// Check [`Snapshot::missed`] afterwards: a computed attribute that took a different branch this
/// time reached an address nobody sampled, and wants re-planning.
pub fn value(syms: &Symbols, snap: &mut Snapshot, attr: &Attribute) -> Result<Vec<u8>, String> {
    match &attr.plan {
        Plan::Text(s) => Ok(s.as_bytes().to_vec()),
        Plan::Const(b) => Ok(b.clone()),
        Plan::Direct { addr, size, bits } => {
            let raw = snap.read(*addr, *size as usize)?;
            let Some((shift, n)) = bits else { return Ok(raw) };
            let mut w = 0u64;
            for (i, &b) in raw.iter().take(8).enumerate() {
                w |= (b as u64) << (8 * i);
            }
            let mask = if *n >= 64 { u64::MAX } else { (1u64 << n) - 1 };
            let field = (w >> (*shift).min(63)) & mask;
            Ok(field.to_le_bytes()[..(*size as usize).min(8)].to_vec())
        }
        Plan::Computed(_) => {
            let expr = attr.expr.as_ref().ok_or("no expression to evaluate")?;
            let mut ev = Eval::new(syms, snap);
            let v = ev.eval(expr)?;
            ev.load(&v)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A target with nothing mapped: every read fails.
    struct NoMem;
    impl Target for NoMem {
        fn pc(&self) -> Option<u32> {
            None
        }
        fn reg(&self, _: u16) -> Option<u32> {
            None
        }
        fn read(&mut self, addr: u32, _: usize) -> Result<Vec<u8>, String> {
            Err(format!("no memory at 0x{addr:08x}"))
        }
        fn symbol(&mut self, _: &str) -> Option<SymbolRef> {
            None
        }
    }


    /// A target where everything reads as zero, so planning can run with no hardware.
    struct Zeros;
    impl Target for Zeros {
        fn pc(&self) -> Option<u32> {
            None
        }
        fn reg(&self, _: u16) -> Option<u32> {
            None
        }
        fn read(&mut self, _: u32, len: usize) -> Result<Vec<u8>, String> {
            Ok(vec![0; len])
        }
        fn symbol(&mut self, _: &str) -> Option<SymbolRef> {
            None
        }
    }

    fn fixtures() -> Option<(Symbols, crate::OrtiFile)> {
        let (Ok(elf), Ok(ort)) = (std::env::var("AURIX_TEST_ELF"), std::env::var("AURIX_TEST_ORTI")) else {
            eprintln!("AURIX_TEST_ELF / AURIX_TEST_ORTI not set, skipping");
            return None;
        };
        if !std::path::Path::new(&elf).exists() || !std::path::Path::new(&ort).exists() {
            eprintln!("test ELF or ORTI file not found, skipping");
            return None;
        }
        let syms = Symbols::load(std::path::Path::new(&elf)).unwrap();
        let f = crate::parse(&std::fs::read_to_string(&ort).unwrap()).unwrap();
        Some((syms, f))
    }

    /// Every attribute of the real file plans against the real ELF, and lands in the class it
    /// should. This is the seam between the ORTI grammar, DWARF and the sampler.
    #[test]
    fn plans_every_attribute_of_the_real_file() {
        let Some((syms, f)) = fixtures() else { return };
        let mut t = Zeros;

        let (mut text, mut konst, mut direct, mut computed) = (0, 0, 0, 0);
        let mut failures = Vec::new();
        for obj in &f.objects {
            let Some(decl) = f.decl(&obj.type_name) else { continue };
            for a in &obj.attrs {
                let Some(d) = decl.attr(&a.name) else { continue };
                let p = plan(&syms, &mut t, d, a.index, &a.value);
                if let Some(e) = &p.error {
                    failures.push(format!("{} {} . {}: {e}", obj.type_name, obj.name, a.name));
                    continue;
                }
                match p.plan {
                    Plan::Text(_) => text += 1,
                    Plan::Const(_) => konst += 1,
                    Plan::Direct { .. } => direct += 1,
                    Plan::Computed(_) => computed += 1,
                }
            }
        }
        assert!(failures.is_empty(), "{} attributes did not plan:
{}", failures.len(), failures.join("
"));
        eprintln!("planned: {direct} direct, {computed} computed, {konst} const, {text} text");
        // The split is the whole point: most live values are a single address, and only the
        // ternary-guarded ones have to be re-evaluated.
        assert!(direct > 100, "expected many single-address attributes, got {direct}");
        assert!(computed > 100, "expected many computed attributes, got {computed}");
        assert!(konst > 100 && text > 100, "const {konst}, text {text}");

        // The per-core OS attributes the SMP extension adds are all plain addresses, so the
        // sidebar costs one point each.
        let os = f.os().unwrap();
        let os_decl = f.decl("OS").unwrap();
        for name in ["RUNNINGTASK", "RUNNINGTASKPRIORITY", "RUNNINGISR2", "SERVICETRACE", "LASTERROR"] {
            for core in 0..f.core_count() {
                let raw = os.attr_at(name, core).unwrap();
                let p = plan(&syms, &mut t, os_decl.attr(name).unwrap(), Some(core), raw);
                assert!(matches!(p.plan, Plan::Direct { .. }), "OS.{name}[{core}] is {:?}", p.plan);
            }
        }
        // ...but CURRENTAPPMODE is guarded by a ternary on OsState, so it is computed.
        let m = plan(&syms, &mut t, os_decl.attr("CURRENTAPPMODE").unwrap(), Some(0), os.attr_at("CURRENTAPPMODE", 0).unwrap());
        assert!(matches!(m.plan, Plan::Computed(_)), "{:?}", m.plan);

        let task_decl = f.decl("TASK").unwrap();
        let task = f.objects_of("TASK").find(|o| o.name == "Task_Core0").unwrap();
        let state = plan(&syms, &mut t, task_decl.attr("STATE").unwrap(), None, task.attr("STATE").unwrap());
        assert!(matches!(state.plan, Plan::Computed(_)), "{:?}", state.plan);
        // `STACK = "&OsCfg_Stack_..."` is an address constant: no sampling, ever.
        let stack = plan(&syms, &mut t, task_decl.attr("STACK").unwrap(), None, task.attr("STACK").unwrap());
        assert!(matches!(stack.plan, Plan::Const(_)), "{:?}", stack.plan);
        assert!(stack.plan.reads().is_empty() && !stack.plan.is_live());
        // ...and so is `SIZE = "sizeof(OsCfg_Stack_OsCore0_Error_Dyn)"`.
        let st = f.objects_of("STACK").find(|o| o.name == "OsCfg_Stack_OsCore0_Error").unwrap();
        let size = plan(&syms, &mut t, f.decl("STACK").unwrap().attr("SIZE").unwrap(), None, st.attr("SIZE").unwrap());
        match &size.plan {
            Plan::Const(b) => assert!(b.iter().any(|&x| x != 0), "sizeof should not be zero: {b:?}"),
            other => panic!("{other:?}"),
        }
        // Prose never reaches the evaluator.
        let dir_decl = f.decl("STACK").unwrap().attr("STACKDIRECTION").unwrap();
        assert!(matches!(plan(&syms, &mut t, dir_decl, None, "DOWN").plan, Plan::Text(_)));
    }


    /// The enumerators of `RUNNINGTASK` are addresses, not ordinals, so naming the running task
    /// means evaluating `&OsCfg_Trace_<task>` for every one of them against DWARF. Without that
    /// the panel can only show a bare pointer.
    #[test]
    fn the_running_task_is_named_from_its_trace_address() {
        let Some((syms, f)) = fixtures() else { return };
        let mut t = Zeros;
        let os = f.os().unwrap();
        let decl = f.decl("OS").unwrap().attr("RUNNINGTASK").unwrap();
        let attr = plan(&syms, &mut t, decl, Some(0), os.attr_at("RUNNINGTASK", 0).unwrap());

        // Every task in the file got an address, and they are real ones.
        assert!(attr.enums.len() > 30, "{} enumerators resolved", attr.enums.len());
        let (addr, _) = *attr.enums.iter().find(|(_, n)| n == "Task_Core0").unwrap();
        assert!(addr > 0x8000_0000 || (0x7000_0000..0x7800_0000).contains(&(addr as u64)), "0x{addr:x} is not a plausible address");
        // INVALID_TASK is the null pointer the OS writes when no task is running.
        assert_eq!(attr.enum_name(0), Some("INVALID_TASK"));

        let Plan::Direct { addr: at, size, .. } = attr.plan else { panic!("{:?}", attr.plan) };
        let mut snap = Snapshot::new(None);
        snap.insert(at, (addr as u32).to_le_bytes()[..size as usize].to_vec());
        let shown = attr.show(&value(&syms, &mut snap, &attr).unwrap());
        assert_eq!(shown.text, "Task_Core0");
        assert_eq!(shown.number, Some(addr));

        // A task's stack enumerator carries a link, which is what makes the cell clickable.
        let task = f.objects_of("TASK").find(|o| o.name == "Task_Core0").unwrap();
        let sd = f.decl("TASK").unwrap().attr("STACK").unwrap();
        let sa = plan(&syms, &mut t, sd, None, task.attr("STACK").unwrap());
        let Plan::Const(bytes) = &sa.plan else { panic!("{:?}", sa.plan) };
        let shown = sa.show(bytes);
        assert_eq!(shown.text, "OsCfg_Stack_Task_Core0");
        assert_eq!(shown.link.as_deref(), Some("OsCfg_Stack_Task_Core0"));
    }

    /// A one-shot read goes straight to the target and skips the attributes that cannot change.
    #[test]
    fn a_live_read_only_touches_what_can_change() {
        let Some((syms, f)) = fixtures() else { return };
        let mut t = Zeros;
        let task = f.objects_of("TASK").find(|o| o.name == "Task_Core0").unwrap();
        let decl = f.decl("TASK").unwrap();

        struct Counting(u32);
        impl Target for Counting {
            fn pc(&self) -> Option<u32> {
                None
            }
            fn reg(&self, _: u16) -> Option<u32> {
                None
            }
            fn read(&mut self, _: u32, len: usize) -> Result<Vec<u8>, String> {
                self.0 += 1;
                Ok(vec![0; len])
            }
            fn symbol(&mut self, _: &str) -> Option<SymbolRef> {
                None
            }
        }

        let mut reads = Counting(0);
        for a in &task.attrs {
            let Some(d) = decl.attr(&a.name) else { continue };
            let attr = plan(&syms, &mut t, d, a.index, &a.value);
            value_live(&syms, &mut reads, &attr).unwrap();
        }
        // Twelve attributes, but the constants and the prose cost nothing.
        assert_eq!(task.attrs.len(), 12);
        assert!(reads.0 > 0 && reads.0 < 12, "{} reads for 12 attributes", reads.0);
    }

    /// Sample bytes in, displayed value out - through the ORTI enumerator table rather than DWARF.
    #[test]
    fn a_sampled_priority_reads_back_as_its_orti_name() {
        let Some((syms, f)) = fixtures() else { return };
        let mut t = Zeros;
        let task = f.objects_of("TASK").find(|o| o.name == "Task_Core0").unwrap();
        let decl = f.decl("TASK").unwrap().attr("PRIORITY").unwrap();
        let attr = plan(&syms, &mut t, decl, None, task.attr("PRIORITY").unwrap());

        let Plan::Direct { addr, size, .. } = attr.plan else { panic!("{:?}", attr.plan) };
        assert_eq!(attr.plan.reads(), vec![Read { addr, len: size }]);

        // The field holds an ordinal; ordinal 12 is priority 420 in this file's enumerator list.
        let mut snap = Snapshot::new(None);
        snap.insert(addr, 12u32.to_le_bytes()[..size as usize].to_vec());
        let bytes = value(&syms, &mut snap, &attr).unwrap();
        assert!(!snap.missed);
        assert_eq!(attr.show(&bytes).text, "420");

        // An ordinal nobody declared stays a number.
        let mut snap = Snapshot::new(None);
        snap.insert(addr, 99u32.to_le_bytes()[..size as usize].to_vec());
        let bytes = value(&syms, &mut snap, &attr).unwrap();
        assert_eq!(attr.show(&bytes).text, "99");
    }

    /// A computed attribute re-evaluates over sampled bytes, and says when the branch moved.
    #[test]
    fn a_computed_attribute_is_re_evaluated_over_samples() {
        let Some((syms, f)) = fixtures() else { return };
        let mut t = Zeros;
        let task = f.objects_of("TASK").find(|o| o.name == "Task_Core0").unwrap();
        let decl = f.decl("TASK").unwrap().attr("STATE").unwrap();
        // Planned with OsState reading 0, so the expression took the `: 0xFF` branch and the only
        // address it learned is OsState itself.
        let attr = plan(&syms, &mut t, decl, None, task.attr("STATE").unwrap());
        let reads = attr.plan.reads();
        assert_eq!(reads.len(), 1, "{reads:?}");

        // Replaying that sample gives the same answer, with no fallback needed.
        let mut snap = Snapshot::new(None);
        snap.insert(reads[0].addr, vec![0; reads[0].len as usize]);
        let bytes = value(&syms, &mut snap, &attr).unwrap();
        assert!(!snap.missed);
        assert_eq!(attr.show(&bytes).text, "UNKNOWN");

        // Once the core is up, OsState reads 2 and the other branch needs an address that was
        // never sampled: the fallback answers and the miss is reported so the caller re-plans.
        let mut live = Zeros;
        let mut snap = Snapshot::new(Some(&mut live));
        let mut os_state = vec![0u8; reads[0].len as usize];
        os_state[0] = 2;
        snap.insert(reads[0].addr, os_state);
        let bytes = value(&syms, &mut snap, &attr).unwrap();
        assert!(snap.missed, "a branch change must be reported so the read set can be re-planned");
        // Zeros everywhere else means CurrentTask is null, so it is not this task and State wins.
        assert_eq!(attr.show(&bytes).text, "RUNNING");
    }

    #[test]
    fn quoted_integer_literals_are_recognised() {
        assert_eq!(parse_int("2"), Some(2));
        assert_eq!(parse_int(" 0x02 "), Some(2));
        assert_eq!(parse_int("0xFF"), Some(255));
        assert_eq!(parse_int("-1"), Some(-1));
        assert_eq!(parse_int("&OsCfg_Trace_Idle"), None);
        assert_eq!(parse_int(""), None);
    }

    #[test]
    fn overlapping_reads_are_joined() {
        let r = |addr, len| Read { addr, len };
        // Two halves of the same word, out of order, plus a neighbour that touches it.
        assert_eq!(merge(vec![r(104, 4), r(100, 4), r(100, 2)]), vec![r(100, 8)]);
        // A gap is kept: merging across it would read memory nobody asked for.
        assert_eq!(merge(vec![r(100, 4), r(200, 4)]), vec![r(100, 4), r(200, 4)]);
        assert_eq!(merge(vec![]), vec![]);
    }

    #[test]
    fn a_snapshot_serves_reads_that_fall_inside_a_sample() {
        let mut s = Snapshot::new(None);
        s.insert(0x7000_0000, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(s.read(0x7000_0000, 4).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(s.read(0x7000_0004, 2).unwrap(), vec![5, 6]);
        assert!(!s.missed);
        // Past the end of the block, and before it: not sampled.
        assert!(s.read(0x7000_0006, 4).is_err());
        assert!(s.missed);
        let mut s = Snapshot::new(None);
        s.insert(0x7000_0010, vec![9]);
        assert!(s.read(0x7000_000F, 1).is_err());
    }

    #[test]
    fn a_missed_read_falls_back_and_says_so() {
        let mut inner = NoMem;
        let mut s = Snapshot::new(Some(&mut inner));
        s.insert(0x100, vec![7, 7]);
        assert_eq!(s.read(0x100, 2).unwrap(), vec![7, 7]);
        assert!(!s.missed);
        assert!(s.read(0x900, 2).is_err());
        assert!(s.missed, "a read served by the fallback must be reported");
    }

    #[test]
    fn values_are_shown_through_the_orti_enumerators() {
        let a = Attribute {
            name: "PRIORITY".into(),
            index: None,
            raw: "Cfg_Dyn.Priority".into(),
            plan: Plan::Direct { addr: 0, size: 4, bits: None },
            expr: None,
            // The inverse map a MICROSAR priority enum really is.
            enums: vec![(0, "700".into()), (27, "4294967295".into())],
            links: Vec::new(),
            error: None,
        };
        assert_eq!(a.show(&[0, 0, 0, 0]), Shown { text: "700".into(), number: Some(0), link: None });
        assert_eq!(a.show(&27u32.to_le_bytes()).text, "4294967295");
        // No enumerator matches: a number, not an invented name.
        assert_eq!(a.show(&5u32.to_le_bytes()).text, "5");
        // Nothing read at all: an empty cell, not a zero.
        assert_eq!(a.show(&[]), Shown { text: String::new(), number: None, link: None });

        let text = Attribute {
            name: "ACTION".into(),
            index: None,
            raw: "SET_EVENT Task_Core1 Event_B".into(),
            plan: Plan::Text("SET_EVENT Task_Core1 Event_B".into()),
            expr: None,
            enums: Vec::new(),
            links: Vec::new(),
            error: None,
        };
        assert_eq!(text.show(b"ignored").text, "SET_EVENT Task_Core1 Event_B");
    }
}
