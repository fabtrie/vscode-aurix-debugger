//! Reverse debugging on the hardware, the part that needs the code and the symbols: the RAM
//! regions from the ELF, the decoded recording of every action, which bytes each action wrote,
//! where a Step Back by line or instruction lands, re-execution to a point inside an action, and
//! Reverse Continue. The checkpoints themselves are the core's ([`aurix_core::reverse`]).
//!
//! An operation (`back`, `continue`, `goto`) runs like a source step: the core counts as stepping
//! (its halts are not reported, Pause cancels), and one `contextSuspended` reports where it ended,
//! after Memory/Registers change events. So every client sees the restored state, whoever asked.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use aurix_core::reverse::{self as rv, Region, Written};
use aurix_core::target::{DebugReason, RunMode, TargetState};
use aurix_core::{log_info, ops};
use mcds::replay::{self, solve::Class, Row};
use mcds::Step;

use crate::breakpoints::Kind as BpKind;
use crate::contexts::{cores, read_pc, CoreInfo, CONTAINER};
use crate::forwarder::suspended_event;
use crate::trace_insns::{MemAt, ProgramTrace};
use crate::{wire, Shared};

/// How long one run of a re-execution may take before it counts as lost.
const RUN_TIMEOUT: Duration = Duration::from_secs(10);
/// Gap up to which ELF sections are merged into one region.
const MERGE_GAP: u32 = 64;

fn call<R: Send + 'static>(sh: &Shared, f: impl for<'a, 'b> FnOnce(&'b aurix_core::Cx<'a>) -> R + Send + 'static) -> Result<R, String> {
    sh.handle.call(f).ok_or_else(|| "the debug server stopped".to_string())
}

// ---- regions ------------------------------------------------------------------------------

/// Default linker symbol pairs of the stacks and the context save area of core `n` (iLLD GCC /
/// HighTec, TASKING): `(low or high, high or low)`, the order does not matter.
fn default_private_symbols(n: u32) -> Vec<(String, String)> {
    vec![
        (format!("__USTACK{n}_END"), format!("__USTACK{n}")),
        (format!("__ISTACK{n}_END"), format!("__ISTACK{n}")),
        (format!("__CSA{n}"), format!("__CSA{n}_END")),
        (format!("_lc_ub_ustack_tc{n}"), format!("_lc_ue_ustack_tc{n}")),
        (format!("_lc_ub_istack_tc{n}"), format!("_lc_ue_istack_tc{n}")),
        (format!("_lc_ub_csa_tc{n}"), format!("_lc_ue_csa_tc{n}")),
    ]
}

fn num(v: &Value) -> Option<u32> {
    match v {
        Value::Number(n) => n.as_u64().map(|x| x as u32),
        Value::String(s) => aurix_core::util::parse_u32(s.trim()).ok(),
        _ => None,
    }
}

fn pairs(v: Option<&Value>) -> Option<Vec<(String, String)>> {
    let a = v?.as_array()?;
    Some(a.iter().filter_map(|p| Some((p.get(0)?.as_str()?.to_string(), p.get(1)?.as_str()?.to_string()))).collect())
}

/// Removes `cut` from the ranges in `list`.
fn subtract(list: Vec<Region>, cut: (u32, u32)) -> Vec<Region> {
    let (cs, ce) = (cut.0, cut.0.wrapping_add(cut.1));
    let mut out = Vec::new();
    for r in list {
        if ce <= r.start || cs >= r.end() {
            out.push(r);
            continue;
        }
        if cs > r.start {
            out.push(Region { start: r.start, len: cs - r.start, ..r.clone() });
        }
        if ce < r.end() {
            out.push(Region { start: ce, len: r.end() - ce, ..r.clone() });
        }
    }
    out
}

/// Whether global address `g` is in core `coreid`'s DSPR (TC3xx: CPU0 0x7000_0000, CPU1
/// 0x6000_0000, ...).
fn own_dspr(coreid: u32, g: u32) -> bool {
    g >> 20 == (0x700 - 0x100 * coreid)
}

fn merge(mut list: Vec<Region>) -> Vec<Region> {
    list.sort_by_key(|r| (r.start, r.private));
    let mut out: Vec<Region> = Vec::new();
    for r in list {
        match out.last_mut() {
            Some(l) if l.private == r.private && l.foreign == r.foreign && r.start <= l.end().saturating_add(MERGE_GAP) => {
                let end = l.end().max(r.end());
                l.len = end - l.start;
                if !l.name.split(' ').any(|n| n == r.name) && l.name.len() < 80 {
                    l.name = format!("{} {}", l.name, r.name);
                }
            }
            _ => out.push(r),
        }
    }
    out
}

/// The RAM regions to save for core `coreid`: the ELF's allocated writable sections in RAM, its
/// stacks and context save area (private), the extra `Regions`, without `Exclude`.
fn build_regions(sh: &Shared, ctx: &str, coreid: u32, v: &Value) -> Result<(Vec<Region>, Vec<String>), String> {
    use object::{Object, ObjectSection, ObjectSymbol, SectionFlags};
    let mut notes = Vec::new();
    let core = coreid as u8;
    let ram = |a: u32| replay::place(core, a);
    let mut shared_list: Vec<Region> = Vec::new();
    // ELF symbol table: name -> (address, size).
    let mut symbols: HashMap<String, (u32, u32)> = HashMap::new();
    if let Some(set) = sh.symbols_for(ctx) {
        for (_, s) in &set.elfs {
            let parsed = std::fs::read(&s.path).map_err(|e| e.to_string());
            let data = match parsed {
                Ok(d) => d,
                Err(e) => {
                    notes.push(format!("{} not read ({e}): its sections are not saved", s.path.display()));
                    continue;
                }
            };
            let Ok(obj) = object::File::parse(&*data) else {
                notes.push(format!("{} is not an ELF file: its sections are not saved", s.path.display()));
                continue;
            };
            for sec in obj.sections() {
                let SectionFlags::Elf { sh_flags, .. } = sec.flags() else { continue };
                // SHF_WRITE | SHF_ALLOC
                if (sh_flags.0 as u64) & 0x3 != 0x3 || sec.size() == 0 {
                    continue;
                }
                let (g, class) = ram(sec.address() as u32);
                if class != Class::Ram {
                    continue;
                }
                let name = sec.name().unwrap_or("?").to_string();
                let lname = name.to_ascii_lowercase();
                // A stack or context save area: this core's own (in its DSPR), or another core's.
                let own_area = lname.contains("stack") || lname.contains("csa");
                let mine = own_dspr(coreid, g);
                shared_list.push(Region { start: g, len: sec.size() as u32, private: own_area && mine, foreign: own_area && !mine, name });
            }
            for sym in obj.symbols() {
                if let Ok(n) = sym.name() {
                    symbols.entry(n.to_string()).or_insert((sym.address() as u32, sym.size() as u32));
                }
            }
        }
    }
    // Stacks and context save area from the linker symbols.
    let mut wanted = default_private_symbols(coreid);
    if let Some(p) = pairs(v.get("StackSymbols")) {
        wanted.extend(p);
    }
    if let Some(p) = pairs(v.get("CsaSymbols")) {
        wanted.extend(p);
    }
    let mut private_ranges: Vec<Region> = Vec::new();
    for (a, b) in &wanted {
        if let (Some(&(x, _)), Some(&(y, _))) = (symbols.get(a), symbols.get(b)) {
            let (lo, hi) = (ram(x.min(y)).0, ram(x.max(y)).0);
            if hi > lo && hi - lo <= 0x10_0000 {
                private_ranges.push(Region { start: lo, len: hi - lo, private: true, foreign: false, name: a.trim_end_matches("_END").to_string() });
            }
        }
    }
    for p in &private_ranges {
        shared_list = subtract(shared_list, (p.start, p.len));
    }
    shared_list.extend(private_ranges);
    if let Some(list) = v.get("Regions").and_then(Value::as_array) {
        for r in list {
            let start = r.get("Start").and_then(num).ok_or("a region needs Start")?;
            let len = r.get("Size").or_else(|| r.get("Len")).and_then(num).ok_or("a region needs Size")?;
            let private = r.get("Private").and_then(Value::as_bool).unwrap_or(false);
            let name = r.get("Name").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| format!("{start:#010x}"));
            let g = ram(start).0;
            shared_list = subtract(shared_list, (g, len));
            shared_list.push(Region { start: g, len, private, foreign: false, name });
        }
    }
    if let Some(list) = v.get("Exclude").and_then(Value::as_array) {
        for x in list {
            let range = match x.get("Symbol").and_then(Value::as_str) {
                Some(name) => match symbols.get(name) {
                    Some(&(a, n)) if n > 0 => Some((ram(a).0, n)),
                    _ => {
                        notes.push(format!("exclude: no symbol `{name}` with a size in the ELF"));
                        None
                    }
                },
                None => match (x.get("Start").and_then(num), x.get("Size").or_else(|| x.get("Len")).and_then(num)) {
                    (Some(s), Some(n)) => Some((ram(s).0, n)),
                    _ => None,
                },
            };
            if let Some(r) = range {
                shared_list = subtract(shared_list, r);
            }
        }
    }
    let regions = merge(shared_list);
    if regions.is_empty() {
        return Err("no RAM to save: load the ELF of the application (symbols) or give Regions".into());
    }
    if !regions.iter().any(|r| r.private) {
        notes.push("no stack or context save area found (linker symbols): every region is restored as shared memory; set StackSymbols / CsaSymbols".into());
    }
    let total: u64 = regions.iter().map(|r| r.len as u64).sum();
    if total > 512 * 1024 {
        notes.push(format!("{} KB are saved at every checkpoint: steps get slower; leave large buffers out with Exclude", total / 1024));
    }
    Ok((regions, notes))
}

fn core_of(sh: &Shared, v: &Value) -> Result<CoreInfo, String> {
    let all = cores(&sh.handle);
    if let Some(ctx) = v.get("Context").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        return crate::contexts::find_core(&sh.handle, ctx).ok_or_else(|| format!("no core {ctx}"));
    }
    if let Some(name) = v.get("Target").and_then(Value::as_str).filter(|s| !s.is_empty()) {
        return all.into_iter().find(|c| c.name == name).ok_or_else(|| format!("no target {name}"));
    }
    all.into_iter().find(|c| c.arch.name == "tricore" && c.coreid == 0).ok_or_else(|| "no TriCore core".into())
}

/// `enable {Context|Target, History, Trace, Regions, Exclude, StackSymbols, CsaSymbols, Strict,
/// MaxHits}`.
pub fn enable_cmd(sh: &Shared, v: &Value) -> Result<Value, String> {
    let core = core_of(sh, v)?;
    let (regions, notes) = build_regions(sh, &core.id, core.coreid, v)?;
    let cfg = rv::Config {
        target: core.idx,
        history: v.get("History").and_then(Value::as_u64).map(|x| x as usize).unwrap_or(aurix_core::reverse_cmd::DEFAULT_HISTORY),
        trace: v.get("Trace").and_then(Value::as_bool).unwrap_or(true),
        regions: regions.clone(),
        strict: v.get("Strict").and_then(Value::as_bool).unwrap_or(false),
        max_hits: v.get("MaxHits").and_then(Value::as_u64).map(|x| x as u32).unwrap_or(aurix_core::reverse_cmd::DEFAULT_MAX_HITS),
    };
    let r2 = regions.clone();
    let coreids: Vec<u32> = cores(&sh.handle).iter().filter(|c| c.arch.name == "tricore").map(|c| c.coreid).collect();
    let ms = call(sh, move |cx| {
        cx.with(|c| -> Result<(u64, Vec<(String, u32, bool)>), String> {
            rv::enable(c, cfg).map_err(|e| e.to_string())?;
            // One read of every region: what a checkpoint will cost.
            let t0 = Instant::now();
            let mut io = c.io().map_err(|e| e.to_string())?;
            for r in &r2 {
                io.read(r.start, r.len as usize).map_err(|e| format!("cannot read {} ({:#010x}, {} bytes): {e}", r.name, r.start, r.len))?;
            }
            let ms = t0.elapsed().as_millis() as u64;
            // What the debugger may write: the cores' safety protection regions (read only; a
            // test write could raise an alarm).
            let windows = rv::protection_windows(&mut io, &coreids);
            let mut refused = Vec::new();
            for r in &r2 {
                let n = rv::closed_bytes(&windows, r.start, r.len);
                if n > 0 {
                    refused.push((r.name.clone(), n, r.private));
                }
            }
            Ok((ms, refused))
        })
    })?;
    let (ms, refused) = match ms {
        Ok(v) => v,
        Err(e) => {
            let _ = call(sh, |cx| cx.with(rv::disable));
            return Err(e);
        }
    };
    sh.reverse_cache.lock().unwrap().clear();
    crate::trace_insns::end_replay(sh, None);
    let mut notes = notes;
    for (name, n, private) in &refused {
        notes.push(if *private {
            format!("{n} bytes of {name} are not writable for the debugger (the application's RAM protection, SPR_SPROT): a Step Back over an action that changes them is refused")
        } else {
            format!("{n} bytes of {name} are not writable for the debugger (the application's RAM protection): changes there are not undone")
        });
    }
    let mut st = status_cmd(sh)?;
    st["CheckpointMs"] = json!(ms);
    st["Refused"] = json!(refused.iter().map(|(n, b, p)| json!({"Name": n, "Bytes": b, "Private": p})).collect::<Vec<_>>());
    st["Notes"] = json!(notes);
    log_info!("reverse debugging on for {}: {} bytes per checkpoint, about {ms} ms", core.name, regions.iter().map(|r| r.len as u64).sum::<u64>());
    broadcast_changed(sh);
    Ok(st)
}

pub fn disable_cmd(sh: &Shared) -> Result<Value, String> {
    call(sh, |cx| cx.with(rv::disable))?;
    sh.reverse_cache.lock().unwrap().clear();
    broadcast_changed(sh);
    status_cmd(sh)
}

fn broadcast_changed(sh: &Shared) {
    if let Ok(st) = status_cmd(sh) {
        sh.broadcast(&wire::event("AurixReverse", "changed", &[st.to_string()]));
    }
}

/// `status`: on/off, target, checkpoints, regions.
pub fn status_cmd(sh: &Shared) -> Result<Value, String> {
    let v = call(sh, |cx| cx.with(|c| rv::status_json(c)))?;
    Ok(tcf_keys(v))
}

/// The core's snake_case JSON with TCF's capitalised keys.
fn tcf_keys(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .map(|(k, v)| {
                    let key: String = k.split('_').map(|w| {
                        let mut c = w.chars();
                        c.next().map(|f| f.to_ascii_uppercase().to_string() + c.as_str()).unwrap_or_default()
                    }).collect();
                    (key, tcf_keys(v))
                })
                .collect(),
        ),
        Value::Array(a) => Value::Array(a.into_iter().map(tcf_keys).collect()),
        o => o,
    }
}

/// `history`: the checkpoints with source locations, oldest first.
pub fn history_cmd(sh: &Shared) -> Result<Value, String> {
    let h = snapshot_history(sh)?;
    let set = sh.symbols_for("");
    let loc = |pc: u32| -> Value {
        match set.as_deref() {
            Some(s) => json!({"Function": s.function_at(pc).map(|f| f.name.clone()), "File": s.line_at(pc).map(|l| l.file), "Line": s.line_at(pc).map(|l| l.line)}),
            None => Value::Null,
        }
    };
    let list: Vec<Value> = h
        .cps
        .iter()
        .enumerate()
        .map(|(i, cp)| {
            json!({
                "Index": i,
                "Id": cp.id,
                "Action": cp.kind,
                "Pc": cp.regs.get(rv::REG_PC),
                "At": loc(cp.regs.get(rv::REG_PC).copied().unwrap_or(0)),
                "EndPc": cp.end.as_ref().map(|e| e.pc),
                "Recorded": cp.recording.is_some(),
                "FromStart": cp.from_start,
                "CostMs": cp.cost_ms,
                "Current": h.at_checkpoint && i + 1 == h.cps.len(),
            })
        })
        .collect();
    Ok(json!({"Checkpoints": list, "AtCheckpoint": h.at_checkpoint, "Enabled": h.cfg.is_some()}))
}

// ---- the history as the core keeps it ----------------------------------------------------------

struct History {
    cfg: Option<rv::Config>,
    cps: Vec<rv::Checkpoint>,
    at_checkpoint: bool,
    open: bool,
    coreid: u8,
}

impl History {
    /// The last action that happened (the core stands at its end).
    fn current(&self) -> Option<usize> {
        let n = self.cps.len();
        if self.at_checkpoint {
            n.checked_sub(2)
        } else {
            n.checked_sub(1)
        }
    }
}

fn snapshot_history(sh: &Shared) -> Result<History, String> {
    call(sh, |cx| {
        cx.with(|c| {
            let coreid = c.reverse.target().and_then(|t| match &c.targets[t].core {
                aurix_core::target::Core::Tricore(tc) => Some(tc.coreid as u8),
                _ => None,
            });
            History { cfg: c.reverse.cfg.clone(), cps: c.reverse.history.clone(), at_checkpoint: c.reverse.at_checkpoint, open: c.reverse.open, coreid: coreid.unwrap_or(0) }
        })
    })
}

/// The decoded recording of action `j`, if it has one.
fn action_trace(sh: &Shared, h: &History, j: usize) -> Option<Arc<ProgramTrace>> {
    let cp = h.cps.get(j)?;
    let rec = cp.recording.as_ref()?;
    if let Some((key, t)) = sh.reverse_cache.lock().unwrap().get(&cp.id) {
        if *key == rec.stopped_at {
            return Some(t.clone());
        }
    }
    // Memory at the action's end: the device while the core still stands there, else the next
    // checkpoint's snapshot.
    let mem = if Some(j) == h.current() && !h.at_checkpoint {
        MemAt::Live
    } else {
        let k = j + 1;
        MemAt::Saved(call(sh, move |cx| cx.with(|c| rv::snapshot(c, k))).ok().flatten().unwrap_or_default())
    };
    let entry = cp.from_start.then(|| cp.regs.clone());
    let t = Arc::new(crate::trace_insns::build_with(sh, rec, entry.as_deref(), mem).ok()?);
    sh.reverse_cache.lock().unwrap().insert(cp.id, (rec.stopped_at, t.clone()));
    Some(t)
}

// ---- what an action wrote ------------------------------------------------------------------

#[derive(Default)]
struct Effects {
    written: Written,
    complete: bool,
    /// Stores to peripheral registers (and flash): not undone.
    sfr: Vec<u32>,
    unknown: usize,
    why: Option<&'static str>,
}

fn add_accesses(e: &mut Effects, core: u8, acc: &[replay::AccessOut]) {
    for a in acc.iter().filter(|a| a.store) {
        match a.addr {
            Some(addr) => {
                let (g, class) = replay::place(core, addr);
                match class {
                    Class::Ram => e.written.push((g, a.len as u32)),
                    _ => e.sfr.push(g),
                }
            }
            None => e.unknown += 1,
        }
    }
}

fn effects(sh: &Shared, h: &History, j: usize) -> Effects {
    let cp = &h.cps[j];
    let core = h.coreid;
    match cp.kind.as_str() {
        "write register" => return Effects { complete: true, ..Default::default() },
        "write" => return Effects { written: cp.wrote.clone(), complete: true, ..Default::default() },
        _ => {}
    }
    if let Some(pt) = action_trace(sh, h, j) {
        let mut e = Effects::default();
        let gaps = pt.replay.rows.iter().any(|r| matches!(r, Row::Gap { .. }));
        for r in 0..pt.rows() {
            add_accesses(&mut e, core, &pt.replay.accesses(r));
        }
        e.why = if !pt.from_entry {
            Some("the recording does not reach back to the action's start")
        } else if !pt.reached {
            Some("the recording does not reach the halt")
        } else if gaps {
            Some("the recording has gaps")
        } else if e.unknown > 0 {
            Some("stores with unknown addresses")
        } else {
            None
        };
        e.complete = e.why.is_none();
        return e;
    }
    // An instruction step: one instruction between two known states.
    if let (Some(end), "step") = (cp.end.as_ref(), cp.kind.as_str()) {
        let set = sh.symbols_for("");
        let img = set.as_deref().map(crate::trace::Image::load).unwrap_or_default();
        if let Ok(mut code) = mcds::flow::CapstoneCode::new(|a: u32, n: usize| img.read(a, n)) {
            let steps = vec![Step::Insn { addr: cp.pc(), time: None }];
            let r = replay::Replay::build_between(&steps, &mut code, Some(&replay::HaltRegs(cp.regs.clone())), Some(&replay::HaltRegs(end.regs.clone())), &mut |_, _| None, core);
            let mut e = Effects::default();
            for row in 0..r.rows.len() {
                add_accesses(&mut e, core, &r.accesses(row));
            }
            let gaps = r.rows.iter().any(|x| matches!(x, Row::Gap { .. }));
            e.why = if gaps { Some("the instruction is not known") } else if e.unknown > 0 { Some("stores with unknown addresses") } else { None };
            e.complete = e.why.is_none();
            return e;
        }
    }
    Effects { why: Some(if h.cfg.as_ref().is_some_and(|c| c.trace) { "the action was not recorded" } else { "recording is off" }), ..Default::default() }
}

/// What undoing actions `k..=last` needs: the written bytes when known for all of them.
struct Undo {
    written: Option<Written>,
    sfr: Vec<u32>,
    why: Option<&'static str>,
}

fn undo_of(sh: &Shared, h: &History, k: usize, last: Option<usize>) -> Undo {
    let mut all: Written = Vec::new();
    let mut sfr = Vec::new();
    let mut why = None;
    if let Some(last) = last {
        for j in k..=last {
            let e = effects(sh, h, j);
            all.extend(e.written);
            sfr.extend(e.sfr);
            if !e.complete && why.is_none() {
                why = e.why.or(Some("unknown"));
            }
        }
    }
    sfr.sort();
    sfr.dedup();
    all.sort();
    Undo { written: why.is_none().then_some(all), sfr, why }
}

// ---- positions -----------------------------------------------------------------------------

/// A point in the history: before row `row` of action `action` (row 0: its checkpoint).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Pos {
    action: usize,
    row: usize,
}

fn is_insn(pt: &ProgramTrace, r: usize) -> bool {
    matches!(pt.replay.rows.get(r), Some(Row::Insn { .. }))
}

fn depth_at(pt: &ProgramTrace, p: usize) -> i32 {
    if p < pt.rows() {
        return pt.depth[p] as i32;
    }
    // After the last row.
    let Some(last) = pt.rows().checked_sub(1) else { return 0 };
    let d = pt.depth[last] as i32;
    match &pt.replay.rows[last] {
        Row::Insn { d: dec, .. } if matches!(dec.mnemonic.as_str(), "call" | "calla" | "calli" | "fcall" | "fcalla" | "fcalli") => d + 1,
        Row::Insn { d: dec, .. } if matches!(dec.mnemonic.as_str(), "ret" | "rfe" | "fret") => d - 1,
        _ => d,
    }
}

fn line_at_pos(sh: &Shared, pt: &ProgramTrace, p: usize, end_pc: u32) -> Option<(String, u32)> {
    if p < pt.rows() {
        let (_, fl, l) = pt.loc[p];
        return Some((pt.files[fl? as usize].clone(), l?));
    }
    sh.symbols_for("")?.line_at(end_pc).map(|l| (l.file, l.line))
}

/// Reverse "next" inside one action: from position `cur`, the start of the previous source line
/// in the same function (stepping backwards over calls), or in the caller once the function's
/// entry is passed. `None`: no other line before `cur` in this action.
fn line_back(sh: &Shared, pt: &ProgramTrace, cur: usize, end_pc: u32) -> Option<usize> {
    let l0 = line_at_pos(sh, pt, cur, end_pc);
    let d0 = depth_at(pt, cur);
    let mut p = cur;
    let found = loop {
        if p == 0 {
            break None;
        }
        p -= 1;
        if matches!(pt.replay.rows[p], Row::Gap { .. }) {
            break None;
        }
        if !is_insn(pt, p) {
            continue;
        }
        let d = depth_at(pt, p);
        if d > d0 {
            continue;
        }
        let Some(l) = line_at_pos(sh, pt, p, end_pc) else { continue };
        if d < d0 || Some(&l) != l0.as_ref() {
            break Some((p, l, d));
        }
    };
    let (mut best, lr, dr) = found?;
    let mut q = best;
    while q > 0 {
        q -= 1;
        if matches!(pt.replay.rows[q], Row::Gap { .. }) {
            break;
        }
        if !is_insn(pt, q) {
            continue;
        }
        let d = depth_at(pt, q);
        if d > dr {
            continue;
        }
        if d < dr || line_at_pos(sh, pt, q, end_pc).as_ref() != Some(&lr) {
            break;
        }
        best = q;
    }
    Some(best)
}

/// Where a Step Back with `gran` lands, from the end of the last action.
fn step_back_target(sh: &Shared, h: &History, gran: &str) -> Option<Pos> {
    let mut j = h.current()?;
    if gran == "action" {
        return Some(Pos { action: j, row: 0 });
    }
    // Step Back right after a source step undoes that step: back to where it started (the
    // forward stepper's idea of a line, not a line boundary the backward search might find).
    if gran == "line" && matches!(h.cps[j].kind.as_str(), "step over" | "step into" | "step out") {
        return Some(Pos { action: j, row: 0 });
    }
    let mut first = true;
    loop {
        let cp = &h.cps[j];
        let Some(pt) = action_trace(sh, h, j).filter(|pt| pt.from_entry && pt.reached && pt.rows() > 0) else {
            // Not recorded from its start: back to its checkpoint.
            return Some(Pos { action: j, row: 0 });
        };
        let end_pc = cp.end.as_ref().map(|e| e.pc).unwrap_or(0);
        let cur = pt.rows();
        if gran == "instruction" {
            let p = (0..cur).rev().find(|&r| is_insn(&pt, r)).unwrap_or(0);
            return Some(Pos { action: j, row: p });
        }
        match line_back(sh, &pt, cur, end_pc) {
            Some(p) => return Some(Pos { action: j, row: p }),
            None if j == 0 || !first => return Some(Pos { action: j, row: 0 }),
            None => {
                // The whole action stayed on the line: look in the one before.
                if line_at_pos(sh, &pt, 0, end_pc) != line_at_pos(sh, &pt, cur, end_pc) {
                    return Some(Pos { action: j, row: 0 });
                }
                j -= 1;
                first = false;
            }
        }
    }
}

// ---- the operation frame ---------------------------------------------------------------------

/// The result of an operation: what to report and how the stop reads.
struct Outcome {
    report: Value,
    reason: String,
    bps: Vec<String>,
}

/// Runs a reverse operation on the reversed core like a source step, then reports one stop.
fn operation(sh: &Shared, f: impl FnOnce(&Shared, &History, &CoreInfo, &AtomicBool) -> Result<Outcome, String>) -> Result<Value, String> {
    let h = snapshot_history(sh)?;
    let cfg = h.cfg.clone().ok_or("reverse debugging is off (enable it in the launch configuration or with `reverse on`)")?;
    let t = cfg.target;
    let core = cores(&sh.handle).into_iter().find(|c| c.idx == t).ok_or("the reversed core is gone")?;
    if call(sh, move |cx| ops::state(cx, t).0)? != TargetState::Halted {
        return Err(format!("{} runs: halt it first", core.name));
    }
    if sh.is_stepping(t) {
        return Err("a step is in progress".into());
    }
    if h.open {
        call(sh, move |cx| cx.with(|c| rv::finish_action(c, t)))?;
    }
    let h = snapshot_history(sh)?;
    let cancel = Arc::new(AtomicBool::new(false));
    sh.stepping.lock().unwrap().insert(t, cancel.clone());
    sh.invalidate_stack(t);
    crate::trace_insns::end_replay(sh, None);
    sh.broadcast(&wire::event("RunControl", "contextResumed", &[json!(core.id).to_string()]));
    let t0 = Instant::now();
    let res = f(sh, &h, &core, &cancel);
    // Make sure the core is halted whatever happened.
    let halted = call(sh, move |cx| ops::state(cx, t).0).ok() == Some(TargetState::Halted);
    if !halted {
        let _ = call(sh, move |cx| {
            let _ = ops::halt(cx, t);
            ops::wait_state(cx, t, TargetState::Halted, 2000)
        });
    }
    // For the REST/MCP clients (the forwarder leaves it to the stop below: the core is stepping).
    let name = core.name.clone();
    let pc0 = read_pc(&sh.handle, t);
    aurix_core::events::emit(aurix_core::Event::Restored { target: t, name, pc: pc0 });
    sh.flush_events(Duration::from_secs(2));
    sh.stepping.lock().unwrap().remove(&t);
    sh.stacks.lock().unwrap().clear();
    sh.broadcast(&wire::event("Memory", "memoryChanged", &[json!(CONTAINER).to_string(), json!([{"addr": 0, "size": 0}]).to_string()]));
    sh.broadcast(&wire::event("Registers", "contextChanged", &[]));
    let pc = read_pc(&sh.handle, t);
    let (reason, bps, out) = match res {
        Ok(mut o) => {
            o.report["Ms"] = json!(t0.elapsed().as_millis() as u64);
            o.report["Pc"] = json!(pc);
            if let Some(set) = sh.symbols_for(&core.id) {
                if let Some(pc) = pc {
                    o.report["Function"] = json!(set.function_at(pc).map(|f| f.name.clone()));
                    o.report["File"] = json!(set.line_at(pc).map(|l| l.file));
                    o.report["Line"] = json!(set.line_at(pc).map(|l| l.line));
                }
            }
            (o.reason, o.bps, Ok(o.report))
        }
        Err(e) => ("Step back failed".to_string(), Vec::new(), Err(e)),
    };
    let r = if bps.is_empty() { DebugReason::SingleStep } else { DebugReason::Breakpoint };
    suspended_event(sh, &core, pc, &reason, &bps, r);
    broadcast_changed(sh);
    out
}

fn wait_halted(sh: &Shared, t: usize, cancel: &AtomicBool) -> Result<bool, String> {
    let t0 = Instant::now();
    loop {
        let st = call(sh, move |cx| {
            ops::poll_all(cx);
            ops::state(cx, t).0
        })?;
        if st == TargetState::Halted {
            return Ok(true);
        }
        if cancel.load(Ordering::Relaxed) || t0.elapsed() > RUN_TIMEOUT {
            let _ = call(sh, move |cx| ops::halt(cx, t));
            let _ = call(sh, move |cx| ops::wait_state(cx, t, TargetState::Halted, 2000));
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// Runs core `t` (run mode `mode`) until it has stopped `hits` times at `addr`, with all other
/// breakpoints out of the hardware. With `end`, a halt at `end.0` with PCXI `end.1` ends the
/// counting (first pass of a re-execution); returns the hits.
fn run_hits(sh: &Shared, t: usize, addr: u32, hits: u32, end: Option<(u32, u32)>, mode: RunMode, cancel: &AtomicBool) -> Result<(u32, Option<u32>), String> {
    let saved = call(sh, move |cx| ops::suspend_triggers(cx, t))?.map_err(|e| e.to_string())?;
    let res = (|| -> Result<(u32, Option<u32>), String> {
        call(sh, move |cx| ops::temporary_breakpoint(cx, t, addr))?.map_err(|e| e.to_string())?;
        if let Some((e, _)) = end.filter(|e| e.0 != addr) {
            call(sh, move |cx| ops::temporary_breakpoint(cx, t, e))?.map_err(|e| e.to_string())?;
        }
        let mut n = 0u32;
        while end.is_some() || n < hits {
            call(sh, move |cx| ops::resume_as(cx, t, None, mode))?.map_err(|e| e.to_string())?;
            if !wait_halted(sh, t, cancel)? {
                return Ok((n, read_pc(&sh.handle, t)));
            }
            let regs = call(sh, move |cx| cx.with(|c| c.target_io(t).and_then(|(mut io, e)| e.core.read_regs(&mut io, &[rv::REG_PC, rv::REG_PCXI]))))?
                .map_err(|e| e.to_string())?;
            let (pc, pcxi) = (regs[0], regs[1]);
            if let Some((e, ex)) = end {
                if pc == e && pcxi == ex {
                    return Ok((n, None));
                }
            }
            if pc != addr {
                if end.is_some_and(|e| e.0 == pc) {
                    continue; // the end address in another frame
                }
                return Ok((n, Some(pc)));
            }
            n += 1;
            if end.is_some() && n > hits {
                return Err(format!("more than {hits} hits to count (MaxHits)"));
            }
        }
        Ok((n, None))
    })();
    let back = call(sh, move |cx| ops::resume_triggers(cx, t, &saved)).and_then(|r| r.map_err(|e| e.to_string()));
    let r = res?;
    back?;
    Ok(r)
}

fn restore(sh: &Shared, h: &History, k: usize) -> Result<(rv::Report, Undo), String> {
    let undo = undo_of(sh, h, k, h.current());
    let strict = h.cfg.as_ref().is_some_and(|c| c.strict);
    if strict && (undo.written.is_none() || !undo.sfr.is_empty()) {
        return Err(format!(
            "not everything can be restored ({}); Strict refuses it",
            undo.why.unwrap_or("peripheral registers were written")
        ));
    }
    let w = undo.written.clone();
    let rep = call(sh, move |cx| cx.with(|c| rv::restore(c, k, w.as_ref())))?.map_err(|e| e.to_string())?;
    Ok((rep, undo))
}

fn report_json(rep: &rv::Report, undo: &Undo) -> Value {
    json!({
        "Checkpoint": rep.checkpoint,
        "Action": rep.kind,
        "Registers": rep.registers,
        "Bytes": rep.bytes,
        "ByDifference": rep.by_difference,
        "Why": undo.why,
        "OutsideBytes": rep.outside_bytes,
        "NotUndone": {
            "Peripheral": undo.sfr.iter().map(|a| format!("{a:#010x}")).collect::<Vec<_>>(),
            "Protected": rep.refused.iter().map(|(a, n)| json!([format!("{a:#010x}"), n])).collect::<Vec<_>>(),
        },
    })
}

/// Goes to `pos`: restores its action's checkpoint and, for a row inside the action, runs there
/// again and checks the run against the old recording.
fn go_to(sh: &Shared, h: &History, core: &CoreInfo, pos: Pos, cancel: &AtomicBool) -> Result<Value, String> {
    let t = core.idx;
    let cp = h.cps[pos.action].clone();
    let old = if pos.row > 0 { action_trace(sh, h, pos.action) } else { None };
    let (rep, undo) = restore(sh, h, pos.action)?;
    let mut report = report_json(&rep, &undo);
    report["Position"] = json!({"Checkpoint": pos.action, "Row": pos.row});
    let Some(old) = old else { return Ok(report) };
    let cfg = h.cfg.clone().unwrap_or_else(|| unreachable!());
    let addr = old.row_addr(pos.row).ok_or("the target is not an instruction")?;
    let start_pc = cp.pc();
    let mode = cp.mode;
    let occ_upto = (0..=pos.row).filter(|&r| old.row_addr(r) == Some(addr)).count() as u32;
    let at_start = u32::from(start_pc == addr);
    let hits = if old.from_entry {
        occ_upto - at_start
    } else {
        // The recording lost its start: count the hits to the action's end first, then run again.
        let end = cp.end.clone().ok_or("the action's end is not known")?;
        if !matches!(end.reason, DebugReason::Breakpoint | DebugReason::SingleStep) || end.pc == addr {
            return Err("the action's recording does not reach back to its start and its end cannot be run to again".into());
        }
        let after = (pos.row..old.rows()).filter(|&r| old.row_addr(r) == Some(addr)).count() as u32;
        call(sh, |cx| cx.with(|c| c.reverse.suspended += 1))?;
        let counted = run_hits(sh, t, addr, cfg.max_hits, Some((end.pc, end.pcxi)), mode, cancel);
        call(sh, |cx| cx.with(|c| c.reverse.suspended -= 1))?;
        let (n, stray) = counted?;
        if stray.is_some() {
            let _ = call(sh, |cx| cx.with(|c| rv::restore(c, c.reverse.history.len() - 1, None)));
            return Err(format!("counting the hits stopped elsewhere ({:#010x}): the run took another path", stray.unwrap_or(0)));
        }
        let total = n + at_start;
        let target = (total + 1).checked_sub(after).filter(|&x| x >= 1).ok_or("the counted run took another path")?;
        // Back to the checkpoint again (by difference: the counting run was not recorded).
        call(sh, |cx| cx.with(|c| rv::restore(c, c.reverse.history.len() - 1, None)))?.map_err(|e| e.to_string())?;
        report["Counted"] = json!(n);
        target - at_start
    };
    if hits > cfg.max_hits {
        report["Reexecuted"] = json!(false);
        report["Note"] = json!(format!("{hits} breakpoint hits needed (MaxHits {}): stopped at the action's start", cfg.max_hits));
        return Ok(report);
    }
    // The run again, recorded as the action from the same checkpoint.
    call(sh, move |cx| cx.with(|c| rv::begin_scope(c, t, "step back", mode)))?;
    let ran = run_hits(sh, t, addr, hits, None, mode, cancel);
    call(sh, move |cx| cx.with(|c| rv::end_scope(c, t)))?;
    let (n, stray) = ran?;
    report["Reexecuted"] = json!(true);
    report["Hits"] = json!(n);
    // Check: same path, same exact registers.
    let h2 = snapshot_history(sh)?;
    let mut diverged: Option<Value> = stray.map(|pc| json!({"StoppedAt": pc}));
    if diverged.is_none() {
        if let Some(new) = h2.current().and_then(|j| action_trace(sh, &h2, j)) {
            let old_addrs: Vec<u32> = (0..pos.row).filter_map(|r| old.row_addr(r)).collect();
            let new_addrs: Vec<u32> = (0..new.rows()).filter_map(|r| new.row_addr(r)).collect();
            if new.from_entry {
                if let Some(i) = old_addrs.iter().zip(&new_addrs).position(|(a, b)| a != b) {
                    diverged = Some(json!({"Instruction": i, "Expected": old_addrs[i], "Got": new_addrs[i]}));
                } else if new_addrs.len() != old_addrs.len() {
                    diverged = Some(json!({"Instructions": new_addrs.len(), "Expected": old_addrs.len()}));
                }
            }
        }
    }
    if diverged.is_none() {
        let want = old.replay.regs(pos.row.checked_sub(1));
        let now = call(sh, move |cx| cx.with(|c| c.target_io(t).and_then(|(mut io, e)| e.core.read_regs(&mut io, &(0..32).collect::<Vec<_>>()))))?.map_err(|e| e.to_string())?;
        let bad: Vec<Value> = want
            .iter()
            .take(32)
            .enumerate()
            .filter(|(i, r)| r.is_known() && r.mark == Some(replay::Mark::Exact) && now.get(*i) != Some(&r.value))
            .map(|(i, r)| json!({"Reg": r.name, "Expected": r.value, "Got": now.get(i)}))
            .collect();
        if !bad.is_empty() {
            diverged = Some(json!({"Registers": bad}));
        }
    }
    if let Some(d) = &diverged {
        log_info!("reverse: the run again took another path: {d}");
    }
    report["Diverged"] = diverged.unwrap_or(Value::Null);
    Ok(report)
}

/// `back {Granularity: "line" | "instruction" | "action"}`: Step Back.
pub fn back_cmd(sh: &Shared, v: &Value) -> Result<Value, String> {
    let gran = v.get("Granularity").and_then(Value::as_str).unwrap_or("line").to_string();
    operation(sh, move |sh, h, core, cancel| {
        let pos = step_back_target(sh, h, &gran).ok_or("no history before this point")?;
        let report = go_to(sh, h, core, pos, cancel)?;
        Ok(Outcome { report, reason: "Step back".into(), bps: Vec::new() })
    })
}

/// `goto {Index, Row}`: to checkpoint `Index` (and row `Row` of its action).
pub fn goto_cmd(sh: &Shared, v: &Value) -> Result<Value, String> {
    let index = v.get("Index").and_then(Value::as_u64).ok_or("goto needs Index")? as usize;
    let row = v.get("Row").and_then(Value::as_u64).unwrap_or(0) as usize;
    operation(sh, move |sh, h, core, cancel| {
        if index >= h.cps.len() || h.current().is_none_or(|c| index > c) {
            return Err("no such point in the history".into());
        }
        let report = go_to(sh, h, core, Pos { action: index, row }, cancel)?;
        Ok(Outcome { report, reason: "Step back".into(), bps: Vec::new() })
    })
}

/// Code breakpoints (address -> IDs) and data breakpoints (start, length, store/load bits, ID)
/// of target `t`.
#[allow(clippy::type_complexity)]
fn breakpoints_of(sh: &Shared, t: usize) -> (HashMap<u32, Vec<String>>, Vec<(u32, u32, u32, String)>) {
    let mut code: HashMap<u32, Vec<String>> = HashMap::new();
    let mut data = Vec::new();
    let table = sh.bps.lock().unwrap();
    for (id, b) in &table.bps {
        for i in b.instances.iter().filter(|i| i.error.is_none() && i.target == t) {
            match i.kind {
                BpKind::Code => code.entry(i.addr).or_default().push(id.clone()),
                BpKind::Data { len, access } => data.push((i.addr, len, access.bits(), id.clone())),
            }
        }
    }
    (code, data)
}

/// `continue`: Reverse Continue to the last breakpoint or data breakpoint hit in the history, or
/// to its start.
pub fn continue_cmd(sh: &Shared) -> Result<Value, String> {
    operation(sh, |sh, h, core, cancel| {
        let t = core.idx;
        let (code, data) = breakpoints_of(sh, t);
        let cur = h.current().ok_or("no history before this point")?;
        let place = |a: u32| replay::place(h.coreid, a).0;
        let data: Vec<(u32, u32, u32, String)> = data.into_iter().map(|(a, n, b, id)| (place(a), n, b, id)).collect();
        let mut found: Option<(Pos, Vec<String>, &str)> = None;
        'search: for j in (0..=cur).rev() {
            let cp = &h.cps[j];
            match action_trace(sh, h, j).filter(|pt| pt.from_entry) {
                Some(pt) => {
                    for r in (0..pt.rows()).rev() {
                        if let Some(ids) = pt.row_addr(r).and_then(|a| code.get(&a)) {
                            found = Some((Pos { action: j, row: r }, ids.clone(), "Breakpoint"));
                            break 'search;
                        }
                        for a in pt.replay.accesses(r) {
                            let Some(addr) = a.addr else { continue };
                            let g = place(addr);
                            let bit = if a.store { 1 } else { 2 };
                            if let Some(d) = data.iter().find(|d| d.2 & bit != 0 && g < d.0 + d.1 && d.0 < g + a.len as u32) {
                                found = Some((Pos { action: j, row: r }, vec![d.3.clone()], "Data breakpoint"));
                                break 'search;
                            }
                        }
                    }
                }
                None => {
                    if let Some(ids) = code.get(&cp.pc()) {
                        found = Some((Pos { action: j, row: 0 }, ids.clone(), "Breakpoint"));
                        break 'search;
                    }
                }
            }
        }
        match found {
            Some((pos, ids, what)) => {
                let mut report = go_to(sh, h, core, pos, cancel)?;
                report["Stop"] = json!(what);
                let reason = if what == "Breakpoint" { "Breakpoint (reverse)" } else { "Step back to a data breakpoint" };
                let bps = if what == "Breakpoint" { ids } else { Vec::new() };
                Ok(Outcome { report, reason: reason.into(), bps })
            }
            None => {
                let mut report = go_to(sh, h, core, Pos { action: 0, row: 0 }, cancel)?;
                report["Stop"] = json!("Start of history");
                Ok(Outcome { report, reason: "Step back (start of history)".into(), bps: Vec::new() })
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(start: u32, len: u32, private: bool) -> Region {
        Region { start, len, private, foreign: false, name: format!("{start:x}") }
    }

    #[test]
    fn regions_merge_and_cut() {
        let m = merge(vec![r(0x120, 0x10, false), r(0x100, 0x10, false), r(0x1000, 4, false), r(0x2000, 8, true)]);
        assert_eq!(m.iter().map(|x| (x.start, x.len, x.private)).collect::<Vec<_>>(), vec![(0x100, 0x30, false), (0x1000, 4, false), (0x2000, 8, true)]);
        assert!(own_dspr(0, 0x7000_8000) && !own_dspr(0, 0x6000_8000) && own_dspr(1, 0x6000_0400));
        let s = subtract(vec![r(0x100, 0x100, false)], (0x140, 0x20));
        assert_eq!(s.iter().map(|x| (x.start, x.len)).collect::<Vec<_>>(), vec![(0x100, 0x40), (0x160, 0xA0)]);
        assert_eq!(subtract(vec![r(0x100, 0x10, false)], (0x0, 0x1000)).len(), 0);
    }

    #[test]
    fn protection_windows_count_closed_bytes() {
        // CPU0's DSPR open from 0x7000B400 (as the TC377 application sets it up).
        let w = vec![(0x7000_0000, 0x7010_0000, vec![(0x7000_B400, 0x7002_5400), (0x7002_5400, 0x7003_C000)])];
        assert_eq!(rv::closed_bytes(&w, 0x7000_0400, 0xAA40), 0xAA40);
        assert_eq!(rv::closed_bytes(&w, 0x7000_B400, 0x100), 0);
        assert_eq!(rv::closed_bytes(&w, 0x7000_B300, 0x200), 0x100);
        assert_eq!(rv::closed_bytes(&w, 0x5000_0000, 0x100), 0, "not a protected memory");
    }

    #[test]
    fn keys_are_capitalised() {
        let v = tcf_keys(json!({"snapshot_bytes": 3, "regions": [{"start": "0x1"}]}));
        assert_eq!(v, json!({"SnapshotBytes": 3, "Regions": [{"Start": "0x1"}]}));
    }
}
