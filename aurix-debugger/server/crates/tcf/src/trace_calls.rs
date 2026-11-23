//! The flow timeline: a program recording as calls (where each call starts and ends, at which
//! call depth) and, when the recording holds the OS variables that say which task or ISR runs
//! (ORTI `RUNNINGTASK` / `RUNNINGISR2` recorded with the data trace besides the program trace),
//! the task or ISR each call ran in.
//!
//! The calls come from the rows of the instruction trace ([`crate::trace_insns::ProgramTrace`]): a call instruction,
//! an interrupt entry or a `syscall` opens a frame at the next instruction, a `ret` / `rfe` closes
//! it, and code that changes function without either (a tail jump) replaces the frame. Every task
//! and ISR keeps its own stack of open frames: a switch puts the old one aside and continues the
//! new one where it was left. Frames that began before the recording (a return below the first
//! frame) or still run at its end are marked open on that side. Depths are relative: the
//! shallowest frame of each task or ISR is depth 0.
//!
//! The OS variables are written a little before the switch they announce (in the OS code that
//! makes it), so the last instructions of that OS code count to the new task or ISR, and a write
//! is placed at the start of the stretch of code it happened in (the program trace reports code
//! in stretches between branches).
//!
//! Times are spread over the rows between the trace's time stamps (one every ~160 ns), so a call
//! shorter than that still gets a width.

use serde_json::{json, Value};

use mcds::replay::Row;
use mcds::{AddrRange, Step};

use crate::Shared;

/// An OS variable that says what runs on the core.
#[derive(Clone)]
pub struct CtxVar {
    pub attr: orti::Attribute,
    pub addr: u32,
    pub size: u32,
    /// `RUNNINGISR2` (or another `RUNNINGISR*`): an ISR runs while it holds a valid value.
    pub isr: bool,
}

/// Largest span of the OS variables recorded with the program trace (one data comparator range:
/// writes to other variables inside it fill the trace buffer too).
pub const MAX_SPAN: u32 = 256;

/// The loaded ORTI file's `RUNNINGTASK` and `RUNNINGISR*` of `core` that are plain variables.
pub fn context_vars(sh: &Shared, core: u8) -> Result<Vec<CtxVar>, String> {
    let aw = sh.orti.lock().unwrap().clone().ok_or("no ORTI file loaded: load it to record which task or ISR runs")?;
    let os = aw.objects_of("OS").next().ok_or("the ORTI file has no OS object")?;
    let core = core as u32;
    let mine = |a: &&orti::Attribute| a.index == Some(core) || (a.index.is_none() && core == 0);
    let mut out = Vec::new();
    for a in os.attrs.iter().filter(mine).filter(|a| a.name == "RUNNINGTASK" || a.name.starts_with("RUNNINGISR")) {
        if let orti::Plan::Direct { addr, size, .. } = a.plan {
            out.push(CtxVar { attr: a.clone(), addr, size, isr: a.name != "RUNNINGTASK" });
        }
    }
    if !out.iter().any(|v| !v.isr) {
        return Err(format!("the ORTI file's RUNNINGTASK of core {core} is not a variable at a fixed address, so it cannot be traced"));
    }
    Ok(out)
}

/// The data range that records the variables: the task variable and the ISR variables within
/// [`MAX_SPAN`] bytes of it.
pub fn context_range(vars: &[CtxVar]) -> Result<AddrRange, String> {
    let task = vars.iter().find(|v| !v.isr).ok_or("no RUNNINGTASK variable")?;
    let near: Vec<&CtxVar> = vars.iter().filter(|v| v.addr.abs_diff(task.addr) < MAX_SPAN).collect();
    let lo = near.iter().map(|v| v.addr).min().unwrap_or(task.addr);
    let hi = near.iter().map(|v| v.addr as u64 + v.size.max(1) as u64).max().unwrap_or(task.addr as u64 + 4);
    AddrRange::new(lo, (hi - lo as u64) as u32)
}

/// What a row of the instruction trace does to the call stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum K {
    Insn,
    Call,
    Ret,
    /// `syscall`: a trap into the OS, left with `rfe`.
    Trap,
    /// An interrupt or trap entered before the next instruction.
    Entry,
    Gap,
}

/// A write to one of the OS variables, at the row it comes before.
#[derive(Debug, Clone)]
pub(crate) struct Switch {
    pub row: usize,
    pub var: usize,
    /// The value names a task or ISR (not "none": 0 or an `INVALID_*` enumerator).
    pub valid: bool,
    pub name: String,
}

/// The frame flags of a call.
pub(crate) const OPEN_L: u8 = 1;
pub(crate) const OPEN_R: u8 = 2;
pub(crate) const ENTRY: u8 = 4;
pub(crate) const TRAP: u8 = 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Call {
    pub func: Option<u32>,
    pub ctx: usize,
    pub depth: i32,
    pub r0: usize,
    pub r1: usize,
    pub flags: u8,
}

#[derive(Debug, Default)]
pub(crate) struct Built {
    /// `(kind, name)`: `task`, `isr`, `none` (no task or ISR: OS code), `unknown` (not recorded
    /// yet), or `all` (the recording has no OS variables).
    pub contexts: Vec<(&'static str, String)>,
    /// `(context, first row, last row)`, gaps excluded.
    pub segments: Vec<(usize, usize, usize)>,
    pub calls: Vec<Call>,
    pub gaps: Vec<usize>,
}

#[derive(Debug, Clone, Copy)]
struct Open {
    func: Option<u32>,
    r0: usize,
    depth: i32,
    flags: u8,
}

#[derive(Default)]
struct Stack {
    open: Vec<Open>,
    /// The last frame returned below the first one seen: the next instruction is in a caller that
    /// began before, at this depth.
    under: Option<i32>,
}

/// Builds the calls and the task / ISR segments. `isr[v]`: variable `v` is an ISR variable;
/// `end[v]`: its value at the end of the recording, when known (used for a variable the
/// recording never wrote); without variables everything runs in one context `all`.
pub(crate) fn build(kinds: &[K], funcs: &[Option<u32>], isr: &[bool], switches: &[Switch], end: &[Option<(bool, String)>]) -> Built {
    let n = kinds.len();
    let mut b = Built::default();
    let tracked = !isr.is_empty();
    // Values before the first write: the value at the end for a variable never written; for an
    // ISR variable whose first write enters an ISR, no ISR (nesting aside); else unknown.
    let mut vals: Vec<Option<(bool, String)>> = (0..isr.len())
        .map(|v| match switches.iter().find(|s| s.var == v) {
            None => end.get(v).cloned().flatten(),
            Some(first) if isr[v] && first.valid => Some((false, String::new())),
            Some(_) => None,
        })
        .collect();
    let mut intern = |b: &mut Built, kind: &'static str, name: String| -> usize {
        match b.contexts.iter().position(|c| c.0 == kind && c.1 == name) {
            Some(i) => i,
            None => {
                b.contexts.push((kind, name));
                b.contexts.len() - 1
            }
        }
    };
    let ctx_of = |vals: &[Option<(bool, String)>], b: &mut Built, intern: &mut dyn FnMut(&mut Built, &'static str, String) -> usize| -> usize {
        if !tracked {
            return intern(b, "all", String::new());
        }
        let mut isr_unknown = false;
        for (v, val) in vals.iter().enumerate().filter(|(v, _)| isr[*v]) {
            match val {
                Some((true, name)) => return intern(b, "isr", name.clone()),
                Some((false, _)) => {}
                None => isr_unknown = true,
            }
            let _ = v;
        }
        if isr_unknown {
            return intern(b, "unknown", String::new());
        }
        match vals.iter().enumerate().find(|(v, _)| !isr[*v]).and_then(|(_, x)| x.clone()) {
            Some((true, name)) => intern(b, "task", name),
            Some((false, _)) => intern(b, "none", String::new()),
            None => intern(b, "unknown", String::new()),
        }
    };

    let mut cur = ctx_of(&vals, &mut b, &mut intern);
    let mut stacks: Vec<Stack> = Vec::new();
    let mut seg_start: Option<usize> = None;
    let mut pending: Option<u8> = None;
    let mut after_ret = false;
    let mut sw = switches.iter().peekable();
    let close = |calls: &mut Vec<Call>, ctx: usize, o: Open, r1: usize, extra: u8| {
        calls.push(Call { func: o.func, ctx, depth: o.depth, r0: o.r0, r1: r1.max(o.r0), flags: o.flags | extra });
    };

    for i in 0..n {
        // The OS variables written before this row.
        let mut changed = false;
        while let Some(s) = sw.peek() {
            if s.row > i {
                break;
            }
            if s.var < vals.len() {
                vals[s.var] = Some((s.valid, s.name.clone()));
                changed = true;
            }
            sw.next();
        }
        if changed {
            let c = ctx_of(&vals, &mut b, &mut intern);
            if c != cur {
                if let Some(s0) = seg_start.take() {
                    if i > s0 {
                        b.segments.push((cur, s0, i - 1));
                    }
                }
                // An ISR that ends (no ISR runs any more) is done: its frames end here. A task
                // that is left may be resumed later: its frames stay open.
                if b.contexts[cur].0 == "isr" && b.contexts[c].0 != "isr" {
                    if let Some(st) = stacks.get_mut(cur) {
                        for o in st.open.drain(..) {
                            close(&mut b.calls, cur, o, i.saturating_sub(1), 0);
                        }
                        st.under = None;
                    }
                }
                cur = c;
            }
        }
        if kinds[i] == K::Gap {
            if let Some(s0) = seg_start.take() {
                if i > s0 {
                    b.segments.push((cur, s0, i - 1));
                }
            }
            // The flow is unknown across a gap: every frame ends before it.
            for (ctx, st) in stacks.iter_mut().enumerate() {
                for o in st.open.drain(..) {
                    close(&mut b.calls, ctx, o, i.saturating_sub(1), OPEN_R);
                }
                st.under = None;
            }
            pending = None;
            after_ret = false;
            b.gaps.push(i);
            continue;
        }
        if seg_start.is_none() {
            seg_start = Some(i);
        }
        if stacks.len() <= cur {
            stacks.resize_with(cur + 1, Stack::default);
        }
        if kinds[i] == K::Entry {
            pending = Some(ENTRY);
            continue;
        }
        let f = funcs.get(i).copied().flatten();
        let st = &mut stacks[cur];
        if let Some(fl) = pending.take() {
            let depth = st.open.last().map_or(0, |o| o.depth + 1);
            st.under = None;
            st.open.push(Open { func: f, r0: i, depth, flags: fl });
        } else if let Some(depth) = st.under.take() {
            st.open.push(Open { func: f, r0: i, depth, flags: OPEN_L });
        } else if st.open.is_empty() {
            st.open.push(Open { func: f, r0: i, depth: 0, flags: OPEN_L });
        } else if st.open.last().is_some_and(|o| o.func != f) {
            // A jump into another function (a tail call), or a return to a caller other than the
            // frame below: the frame at this depth is another function from here on.
            let o = st.open.pop().unwrap();
            close(&mut b.calls, cur, o, i - 1, 0);
            st.open.push(Open { func: f, r0: i, depth: o.depth, flags: if after_ret { OPEN_L } else { 0 } });
        }
        after_ret = false;
        match kinds[i] {
            K::Call => pending = Some(0),
            K::Trap => pending = Some(TRAP),
            K::Ret => {
                if let Some(o) = st.open.pop() {
                    close(&mut b.calls, cur, o, i, 0);
                    if st.open.is_empty() {
                        st.under = Some(o.depth - 1);
                    }
                }
                after_ret = true;
            }
            _ => {}
        }
    }
    if let Some(s0) = seg_start {
        if n > s0 {
            b.segments.push((cur, s0, n - 1));
        }
    }
    for (ctx, st) in stacks.iter_mut().enumerate() {
        for o in st.open.drain(..) {
            close(&mut b.calls, ctx, o, n.saturating_sub(1), OPEN_R);
        }
    }
    // Only the contexts something ran in (the state before the first write may be unused).
    let mut used = vec![false; b.contexts.len()];
    b.segments.iter().for_each(|s| used[s.0] = true);
    b.calls.iter().for_each(|c| used[c.ctx] = true);
    let mut map = vec![0usize; b.contexts.len()];
    let mut kept = Vec::new();
    for (i, c) in b.contexts.iter().enumerate() {
        if used[i] {
            map[i] = kept.len();
            kept.push(c.clone());
        }
    }
    b.contexts = kept;
    b.segments.iter_mut().for_each(|s| s.0 = map[s.0]);
    b.calls.iter_mut().for_each(|c| c.ctx = map[c.ctx]);
    // Depth 0: the shallowest frame of each task or ISR.
    for ctx in 0..b.contexts.len() {
        let min = b.calls.iter().filter(|c| c.ctx == ctx).map(|c| c.depth).min().unwrap_or(0);
        for c in b.calls.iter_mut().filter(|c| c.ctx == ctx) {
            c.depth -= min;
        }
    }
    b.calls.sort_by_key(|c| (c.r0, c.depth));
    b
}

/// Start time of every row (and the end, at index `n`) in ns from the first row: the time stamps
/// of the steps spread evenly over the rows between them. Rows before the first stamp (after a
/// ring lap the first paragraph's stamps lack their base) go back at the average rate.
pub(crate) fn spread(raw: &[Option<f64>]) -> Vec<f64> {
    let n = raw.len();
    let mut out = vec![0.0; n + 1];
    let known: Vec<(usize, f64)> = raw.iter().enumerate().filter_map(|(i, t)| t.map(|t| (i, t))).collect();
    if known.is_empty() {
        // No time stamps: rows at the nominal 3.3 ns (one instruction per cycle at 300 MHz).
        for (i, o) in out.iter_mut().enumerate() {
            *o = i as f64 * 3.3;
        }
        return out;
    }
    // Groups of rows that carry the same stamp: each spread up to the next stamp.
    let mut groups: Vec<(usize, f64)> = Vec::new();
    for &(i, t) in &known {
        if groups.last().is_none_or(|g| g.1 != t) {
            groups.push((i, t));
        }
    }
    let first = groups[0];
    let last = *groups.last().unwrap();
    let rate = if last.0 > first.0 && last.1 > first.1 { (last.1 - first.1) / (last.0 - first.0) as f64 } else { 3.3 };
    for i in 0..first.0 {
        out[i] = first.1 - (first.0 - i) as f64 * rate;
    }
    for (k, &(i0, t0)) in groups.iter().enumerate() {
        let (i1, t1) = groups.get(k + 1).copied().unwrap_or((n, t0 + (n - i0) as f64 * rate));
        let len = (i1 - i0).max(1) as f64;
        for (j, o) in out.iter_mut().enumerate().take(i1).skip(i0) {
            *o = t0 + (t1 - t0) * (j - i0) as f64 / len;
        }
        if k + 1 == groups.len() {
            out[n] = t1;
        }
    }
    for i in 1..=n {
        if out[i] < out[i - 1] {
            out[i] = out[i - 1];
        }
    }
    let base = out[0];
    out.iter_mut().for_each(|t| *t -= base);
    out
}

fn row_kind(r: &Row) -> K {
    match r {
        Row::Insn { d, .. } => match d.mnemonic.as_str() {
            "call" | "calla" | "calli" | "fcall" | "fcalla" | "fcalli" => K::Call,
            "ret" | "fret" | "rfe" => K::Ret,
            "syscall" => K::Trap,
            _ => K::Insn,
        },
        Row::Entry { .. } => K::Entry,
        Row::Gap { .. } => K::Gap,
    }
}

fn row_step(r: &Row) -> usize {
    match r {
        Row::Insn { step, .. } | Row::Entry { step, .. } | Row::Gap { step } => *step,
    }
}

/// Whether an OS variable's value names a task or ISR: not 0 and not an `INVALID_*` enumerator.
fn valid_value(v: i64, name: &str) -> bool {
    v != 0 && !name.to_ascii_uppercase().contains("INVALID")
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// `calls`: the last program recording as calls with their task or ISR (see the module).
pub fn calls_cmd(sh: &Shared, _v: &Value) -> Result<Value, String> {
    let (t, ran_since) = crate::trace_insns::current_with(sh)?;
    let rows = &t.replay.rows;
    let n = rows.len();
    let kinds: Vec<K> = rows.iter().map(row_kind).collect();
    let funcs: Vec<Option<u32>> = (0..n).map(|i| t.loc.get(i).and_then(|l| l.0)).collect();

    // The OS variables the recording holds, and the writes to them.
    let mut note: Option<String> = None;
    let mut vars: Vec<CtxVar> = Vec::new();
    if let Some(range) = t.data {
        match context_vars(sh, t.core) {
            Ok(v) => vars = v.into_iter().filter(|x| range.contains(x.addr)).collect(),
            Err(e) => note = Some(format!("the recording holds the task / ISR variables, but {e}")),
        }
        if vars.is_empty() && note.is_none() {
            note = Some("the recorded data range holds none of the ORTI file's RUNNINGTASK / RUNNINGISR variables".into());
        }
    }
    let show = |var: &CtxVar, value: u64| -> (bool, String) {
        let bytes = value.to_le_bytes();
        let s = var.attr.show(&bytes[..(var.size as usize).clamp(1, 8)]);
        let v = s.number.unwrap_or(value as i64);
        (valid_value(v, &s.text), s.text)
    };
    // The row each write comes before: the first row of its stretch of code.
    let row_of_step = |step: usize| rows.partition_point(|r| row_step(r) < step);
    let mut switches: Vec<Switch> = Vec::new();
    for (step, w) in &t.writes {
        let (Some(addr), Some(value)) = (w.addr, w.value) else { continue };
        if let Some(var) = vars.iter().position(|x| addr == x.addr) {
            let (valid, name) = show(&vars[var], value);
            switches.push(Switch { row: row_of_step(*step), var, valid, name });
        }
    }
    // Values at the end: read while the core still stands where the recording stopped.
    let end: Vec<Option<(bool, String)>> = vars
        .iter()
        .map(|x| {
            if !t.memory {
                return None;
            }
            let (a, len) = (x.addr, x.size.clamp(1, 8) as usize);
            let bytes = sh.handle.call(move |cx| cx.with(|c| c.io().and_then(|mut io| io.read(a, len)).ok())).flatten()?;
            let mut v = [0u8; 8];
            v[..bytes.len().min(8)].copy_from_slice(&bytes[..bytes.len().min(8)]);
            Some(show(x, u64::from_le_bytes(v)))
        })
        .collect();
    let isr: Vec<bool> = vars.iter().map(|x| x.isr).collect();
    let built = build(&kinds, &funcs, &isr, &switches, &end);

    // Times: the steps' stamps spread over the rows.
    let t0 = t.t_start.unwrap_or(0);
    let raw: Vec<Option<f64>> = rows
        .iter()
        .map(|r| match t.steps.get(row_step(r)) {
            Some(Step::Insn { time, .. } | Step::Gap { time, .. }) => time.map(|x| mcds::units_to_ns(x.saturating_sub(t0)) as f64),
            None => None,
        })
        .collect();
    let at = spread(&raw);
    let tm = |r: usize| round1(at[r.min(n)]);

    // Code without a function symbol: named by its address.
    let mut funcs_out = t.funcs.clone();
    let calls: Vec<Value> = built
        .calls
        .iter()
        .map(|c| {
            let (_, fl, line) = t.loc.get(c.r0).copied().unwrap_or((None, None, None));
            let func = c.func.or_else(|| {
                let a = rows.get(c.r0).and_then(|r| match r {
                    Row::Insn { addr, .. } => Some(*addr),
                    Row::Entry { vector, .. } => Some(*vector),
                    Row::Gap { .. } => None,
                })?;
                let name = format!("{a:#010x}");
                Some(funcs_out.iter().position(|f| *f == name).unwrap_or_else(|| {
                    funcs_out.push(name);
                    funcs_out.len() - 1
                }) as u32)
            });
            json!([func, c.ctx, c.depth, c.r0, c.r1, tm(c.r0), tm(c.r1 + 1), c.flags, fl, line])
        })
        .collect();
    let segments: Vec<Value> = built.segments.iter().map(|&(c, r0, r1)| json!([c, r0, r1, tm(r0), tm(r1 + 1)])).collect();
    let gaps: Vec<Value> = built.gaps.iter().map(|&r| json!([r, tm(r), tm(r + 1)])).collect();
    let contexts: Vec<Value> = built.contexts.iter().map(|(k, name)| json!({"Kind": k, "Name": name})).collect();
    let source = if vars.is_empty() { "none" } else { "trace" };
    Ok(json!({
        "Rows": n,
        "EndNs": tm(n),
        "Functions": funcs_out,
        "Files": t.files,
        "Columns": ["function", "context", "depth", "row0", "row1", "t0_ns", "t1_ns", "flags", "file", "line"],
        "Flags": {"OpenStart": OPEN_L, "OpenEnd": OPEN_R, "Interrupt": ENTRY, "Trap": TRAP},
        "Calls": calls,
        "Contexts": contexts,
        "Segments": segments,
        "Gaps": gaps,
        "Source": source,
        "Variables": vars.iter().map(|x| json!({"Name": x.attr.name, "Addr": x.addr, "Size": x.size})).collect::<Vec<_>>(),
        "Switches": switches.len(),
        "Note": note,
        "HaltPc": t.halt_pc,
        "Reached": t.reached,
        "RanSince": ran_since,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sw(row: usize, var: usize, name: &str) -> Switch {
        Switch { row, var, valid: !name.is_empty(), name: name.into() }
    }

    fn by_fn(b: &Built, f: u32) -> Vec<(usize, i32, usize, usize, u8)> {
        b.calls.iter().filter(|c| c.func == Some(f)).map(|c| (c.ctx, c.depth, c.r0, c.r1, c.flags)).collect()
    }

    #[test]
    fn calls_nest_and_open_ends_are_marked() {
        use K::*;
        // f0: insn, call -> f1: insn, call -> f2: insn, ret -> f1: ret -> f0: insn, ret -> caller f3 (began before)
        let kinds = [Insn, Call, Insn, Call, Insn, Ret, Ret, Insn, Ret, Insn, Insn];
        let funcs = [0, 0, 1, 1, 2, 2, 1, 0, 0, 3, 3].map(Some);
        let b = build(&kinds, &funcs, &[], &[], &[]);
        assert_eq!(b.contexts, vec![("all", String::new())]);
        assert_eq!(b.segments, vec![(0, 0, 10)]);
        // f3 is the shallowest: depth 0; f0 began before the recording.
        assert_eq!(by_fn(&b, 3), vec![(0, 0, 9, 10, OPEN_L | OPEN_R)]);
        assert_eq!(by_fn(&b, 0), vec![(0, 1, 0, 8, OPEN_L)]);
        assert_eq!(by_fn(&b, 1), vec![(0, 2, 2, 6, 0)]);
        assert_eq!(by_fn(&b, 2), vec![(0, 3, 4, 5, 0)]);
    }

    #[test]
    fn tail_jump_replaces_the_frame_and_a_gap_ends_all() {
        use K::*;
        let kinds = [Insn, Insn, Insn, Gap, Insn];
        let funcs = [Some(0), Some(1), Some(1), None, Some(2)];
        let b = build(&kinds, &funcs, &[], &[], &[]);
        assert_eq!(by_fn(&b, 0), vec![(0, 0, 0, 0, OPEN_L)]);
        assert_eq!(by_fn(&b, 1), vec![(0, 0, 1, 2, OPEN_R)]);
        assert_eq!(by_fn(&b, 2), vec![(0, 0, 4, 4, OPEN_L | OPEN_R)]);
        assert_eq!(b.gaps, vec![3]);
        assert_eq!(b.segments, vec![(0, 0, 2), (0, 4, 4)]);
    }

    #[test]
    fn tasks_and_isrs_keep_their_own_stacks() {
        use K::*;
        // Task A (var 0) runs f0 and calls f1; an interrupt enters f5; the ISR variable (var 1)
        // names ISR X at row 4 (f6 in the ISR), cleared at row 6; the entry stub returns (rfe)
        // and task A goes on in f1.
        let kinds = [Insn, Call, Insn, Entry, Insn, Insn, Insn, Ret, Insn];
        let funcs = [Some(0), Some(0), Some(1), None, Some(5), Some(6), Some(5), Some(5), Some(1)];
        let switches = [sw(0, 0, "TaskA"), sw(5, 1, "IsrX"), sw(6, 1, "")];
        let b = build(&kinds, &funcs, &[false, true], &switches, &[None, None]);
        let names: Vec<String> = b.contexts.iter().map(|c| format!("{}:{}", c.0, c.1)).collect();
        // The ISR variable's first write enters an ISR: no ISR before it.
        assert_eq!(names, vec!["task:TaskA", "isr:IsrX"]);
        assert_eq!(b.segments, vec![(0, 0, 4), (1, 5, 5), (0, 6, 8)]);
        // In the task: f0 (began before), f1 below it, the interrupt entry stub f5 below f1.
        assert_eq!(by_fn(&b, 0), vec![(0, 0, 0, 8, OPEN_L | OPEN_R)]);
        assert_eq!(by_fn(&b, 1), vec![(0, 1, 2, 8, OPEN_R)]);
        assert_eq!(by_fn(&b, 5), vec![(0, 2, 4, 7, ENTRY)]);
        // In the ISR: its own stack, depth 0, ended with the ISR.
        assert_eq!(by_fn(&b, 6), vec![(1, 0, 5, 5, OPEN_L)]);
    }

    #[test]
    fn unknown_until_the_task_variable_is_written_unless_known_at_the_end() {
        use K::*;
        let kinds = [Insn, Insn, Insn];
        let funcs = [Some(0), Some(0), Some(1)];
        let b = build(&kinds, &funcs, &[false], &[sw(2, 0, "TaskB")], &[None]);
        assert_eq!(b.contexts.iter().map(|c| c.0).collect::<Vec<_>>(), vec!["unknown", "task"]);
        assert_eq!(b.segments, vec![(0, 0, 1), (1, 2, 2)]);
        // Never written: the value at the end holds throughout.
        let b = build(&kinds, &funcs, &[false, true], &[], &[Some((true, "TaskC".into())), Some((false, "INVALID_ISR".into()))]);
        assert_eq!(b.contexts, vec![("task", "TaskC".to_string())]);
        assert_eq!(b.segments, vec![(0, 0, 2)]);
    }

    #[test]
    fn times_spread_between_stamps() {
        let t = spread(&[Some(0.0), Some(0.0), Some(0.0), Some(0.0), Some(160.0), Some(160.0)]);
        assert_eq!(t[..5], [0.0, 40.0, 80.0, 120.0, 160.0]);
        // The last group goes on at the average rate (40 ns per row).
        assert_eq!(t[5], 200.0);
        assert_eq!(t[6], 240.0);
        // Before the first stamp: back at the average rate (100 ns per row), from 0.
        let t = spread(&[None, None, Some(100.0), Some(200.0)]);
        assert_eq!(t, vec![0.0, 100.0, 200.0, 300.0, 400.0]);
    }

    #[test]
    fn invalid_values_are_not_tasks() {
        assert!(!valid_value(0, "0"));
        assert!(!valid_value(5, "INVALID_TASK"));
        assert!(valid_value(0x8001_2340, "Task_10ms"));
    }
}
