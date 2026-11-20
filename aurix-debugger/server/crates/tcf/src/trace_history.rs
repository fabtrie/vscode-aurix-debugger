//! The instruction trace of the whole reverse debugging history.
//!
//! With reverse debugging on, every action is recorded from its own checkpoint, so the last
//! recording reaches back only to the last checkpoint. This view puts the actions of the history
//! one after the other: each recorded action's decoded rows ([`crate::reverse::action_trace`],
//! exact registers at both ends), an instruction step as its one instruction, a write as a
//! marker. Every row keeps its position in the history (`Act`: the checkpoint index, `R`: the
//! row of that action), which `reverse goto` takes.
//!
//! The view is built when the history changes (checked with a cheap fingerprint per request) and
//! shared by `insns`, `regs` and `find`, so their row numbers always agree.

use std::sync::Arc;
use std::time::SystemTime;

use serde_json::{json, Value};

use aurix_core::reverse as rv;
use mcds::replay::{Decoded, Disasm};

use crate::reverse::{action_trace, snapshot_history, History};
use crate::trace_insns::ProgramTrace;
use crate::Shared;

/// What identifies the history the view was built from: per checkpoint its ID, action, end of
/// the recording and whether the action ended; and whether the core stands at the last one.
type Fingerprint = (Vec<(u64, String, Option<SystemTime>, bool)>, bool);

pub struct HistoryView {
    key: Fingerprint,
    parts: Vec<Part>,
    /// First global row of each part.
    starts: Vec<usize>,
    /// Registers at each part's checkpoint.
    checkpoint_regs: Vec<Vec<u32>>,
    total: usize,
    pub funcs: Vec<String>,
    pub files: Vec<String>,
}

struct Part {
    /// Index of the action's checkpoint in the history.
    action: usize,
    kind: String,
    rows: Rows,
}

enum Rows {
    /// A recorded action. `lost_start`: the recording does not reach back to the checkpoint (a
    /// gap row comes first). `funcs` / `files`: the trace's indices mapped to the view's.
    Trace { t: Arc<ProgramTrace>, lost_start: bool, funcs: Vec<u32>, files: Vec<u32> },
    /// An instruction step: its instruction, the registers before and after (exact).
    Step { pc: u32, d: Option<Decoded>, func: Option<u32>, file: Option<u32>, line: Option<u32>, before: Vec<u32>, after: Vec<u32> },
    /// An action without instructions to show (a write, an action that was not recorded).
    Marker { note: String, regs: Vec<u32> },
}

impl Part {
    fn len(&self) -> usize {
        match &self.rows {
            Rows::Trace { t, lost_start, .. } => t.rows() + usize::from(*lost_start),
            _ => 1,
        }
    }
}

fn index_of(v: &mut Vec<String>, s: &str) -> u32 {
    match v.iter().position(|x| x == s) {
        Some(i) => i as u32,
        None => {
            v.push(s.to_string());
            (v.len() - 1) as u32
        }
    }
}

fn fingerprint(sh: &Shared) -> Option<Fingerprint> {
    sh.handle
        .call(|cx| {
            cx.with(|c| {
                let cps = c.reverse.history.iter().map(|cp| (cp.id, cp.kind.clone(), cp.recording.as_ref().map(|r| r.stopped_at), cp.end.is_some() || cp.kind.starts_with("write"))).collect();
                (cps, c.reverse.at_checkpoint)
            })
        })
}

/// Whether the trace commands show the whole history: reverse debugging is on with at least one
/// finished action, and the request does not ask for the last recording only (`Scope: "action"`).
pub fn wanted(sh: &Shared, v: &Value) -> bool {
    if v.get("Scope").and_then(Value::as_str) == Some("action") {
        return false;
    }
    sh.handle.call(|cx| cx.with(|c| c.reverse.enabled() && !c.reverse.history.is_empty())).unwrap_or(false)
}

/// The view of the current history, from the cache or built now.
pub fn view(sh: &Shared) -> Result<Arc<HistoryView>, String> {
    let key = fingerprint(sh).ok_or("the debug server stopped")?;
    if let Some(v) = sh.trace_history.lock().unwrap().as_ref().filter(|v| v.key == key) {
        return Ok(v.clone());
    }
    let h = snapshot_history(sh)?;
    let v = Arc::new(build(sh, &h, key));
    *sh.trace_history.lock().unwrap() = Some(v.clone());
    Ok(v)
}

fn build(sh: &Shared, h: &History, key: Fingerprint) -> HistoryView {
    let set = sh.symbols_for("");
    let mut funcs: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let mut parts = Vec::new();
    let last = h.current();
    let img = set.as_ref().map(|s| crate::trace::Image::of(sh, s)).unwrap_or_default();
    let mut code = mcds::flow::CapstoneCode::new(|a: u32, n: usize| img.read(a, n)).ok();
    for j in 0..last.map_or(0, |l| l + 1) {
        let cp = &h.cps[j];
        let after = h.cps.get(j + 1).map(|n| n.regs.clone()).or_else(|| cp.end.as_ref().map(|e| e.regs.clone())).unwrap_or_else(|| cp.regs.clone());
        let rows = if cp.recording.is_some() {
            match action_trace(sh, h, j) {
                Some(t) => {
                    let fmap = t.funcs.iter().map(|f| index_of(&mut funcs, f)).collect();
                    let flmap = t.files.iter().map(|f| index_of(&mut files, f)).collect();
                    Rows::Trace { lost_start: !t.from_entry, t, funcs: fmap, files: flmap }
                }
                None => Rows::Marker { note: format!("{}: the recording could not be decoded", cp.kind), regs: after },
            }
        } else if cp.kind == "step" {
            let pc = cp.pc();
            let d = code.as_mut().and_then(|c| c.disasm(pc));
            let (func, file, line) = match set.as_deref() {
                Some(s) => {
                    let li = s.line_at(pc);
                    (s.function_at(pc).map(|f| index_of(&mut funcs, &f.name)), li.as_ref().map(|l| index_of(&mut files, &l.file)), li.map(|l| l.line))
                }
                None => (None, None, None),
            };
            Rows::Step { pc, d, func, file, line, before: cp.regs.clone(), after }
        } else {
            let note = match cp.kind.as_str() {
                "write" => "memory written".to_string(),
                "write register" => "register written".to_string(),
                k => format!("{k}: not recorded"),
            };
            Rows::Marker { note, regs: after }
        };
        parts.push(Part { action: j, kind: cp.kind.clone(), rows });
    }
    let mut starts = Vec::with_capacity(parts.len());
    let mut total = 0;
    for p in &parts {
        starts.push(total);
        total += p.len();
    }
    let checkpoint_regs = parts.iter().map(|p| h.cps[p.action].regs.clone()).collect();
    HistoryView { key, parts, starts, checkpoint_regs, total, funcs, files }
}

impl HistoryView {
    pub fn rows(&self) -> usize {
        self.total
    }

    /// The part and the row inside it of global row `i`.
    fn locate(&self, i: usize) -> Option<(usize, usize)> {
        if i >= self.total {
            return None;
        }
        let p = self.starts.partition_point(|&s| s <= i) - 1;
        Some((p, i - self.starts[p]))
    }

    pub fn row_json(&self, i: usize) -> Value {
        let Some((p, local)) = self.locate(i) else { return Value::Null };
        let part = &self.parts[p];
        let mut v = match &part.rows {
            Rows::Trace { lost_start: true, .. } if local == 0 => json!({"K": "g", "Note": "the start of this action was not recorded (the trace RAM went round)", "R": 0}),
            Rows::Trace { t, lost_start, funcs, files } => {
                let r = local - usize::from(*lost_start);
                let mut v = t.row_json(r);
                if let Some(f) = v.get("F").and_then(Value::as_u64) {
                    v["F"] = json!(funcs.get(f as usize));
                }
                if let Some(f) = v.get("Fl").and_then(Value::as_u64) {
                    v["Fl"] = json!(files.get(f as usize));
                }
                v["R"] = json!(r);
                v
            }
            Rows::Step { pc, d, func, file, line, before, after } => {
                let changed: Vec<Value> = (0..rv::NREGS)
                    .filter(|&k| k != rv::REG_PC && before.get(k) != after.get(k))
                    .filter_map(|k| after.get(k).map(|v| json!([rv::REG_NAMES[k], v, Value::Null, Value::Null])))
                    .collect();
                let mut v = json!({"K": "i", "A": pc, "S": d.as_ref().map(|d| d.size), "M": d.as_ref().map_or("?", |d| d.mnemonic.as_str()), "O": d.as_ref().map_or("", |d| d.ops.as_str()), "F": func, "Fl": file, "L": line, "D": 0, "R": 0});
                if !changed.is_empty() {
                    v["W"] = json!(changed);
                }
                v
            }
            Rows::Marker { note, .. } => json!({"K": "m", "Note": note, "D": 0, "R": 0}),
        };
        v["I"] = json!(i);
        v["Act"] = json!(part.action);
        v["ActKind"] = json!(part.kind);
        // Times restart with every action: not comparable across the view.
        v["T"] = Value::Null;
        v
    }

    /// Registers after row `i` (exact for steps and markers, the replay's for recorded rows).
    pub fn regs_json(&self, sh: &Shared, i: usize) -> Value {
        let Some((p, local)) = self.locate(i) else { return Value::Null };
        let part = &self.parts[p];
        let exact = |regs: &[u32], before: Option<&[u32]>, why: &str| -> Value {
            let list: Vec<Value> = (0..rv::NREGS)
                .filter(|&k| k != rv::REG_PC)
                .filter_map(|k| {
                    regs.get(k).map(|v| json!({"N": rv::REG_NAMES[k], "V": v, "K": Value::Null, "M": Value::Null, "W": before.is_some_and(|b| b.get(k) != Some(v)), "Why": why}))
                })
                .collect();
            json!({"At": i, "Pc": regs.get(rv::REG_PC), "Regs": list, "Frames": []})
        };
        let mut v = match &part.rows {
            Rows::Trace { t, lost_start, .. } => {
                if *lost_start && local == 0 {
                    return exact(&[], None, "");
                }
                t.regs_json(sh, local - usize::from(*lost_start))
            }
            Rows::Step { before, after, .. } => exact(after, Some(before), "exact (saved after the step)"),
            Rows::Marker { regs, .. } => exact(regs, None, "exact (saved at the next checkpoint)"),
        };
        v["At"] = json!(i);
        v
    }

    /// Registers before row `i`: after the row before it, or at the action's checkpoint for its
    /// first row.
    pub fn regs_before_json(&self, sh: &Shared, i: usize) -> Value {
        match self.locate(i) {
            Some((p, 0)) => {
                let regs = match &self.parts[p].rows {
                    Rows::Step { before, .. } => before.clone(),
                    _ => self.checkpoint_regs.get(p).cloned().unwrap_or_default(),
                };
                let list: Vec<Value> = (0..rv::NREGS)
                    .filter(|&k| k != rv::REG_PC)
                    .filter_map(|k| regs.get(k).map(|v| json!({"N": rv::REG_NAMES[k], "V": v, "K": Value::Null, "M": Value::Null, "W": false, "Why": "exact (checkpoint)"})))
                    .collect();
                json!({"Pc": regs.get(rv::REG_PC), "Regs": list})
            }
            Some(_) => self.regs_json(sh, i - 1),
            None => Value::Null,
        }
    }

    /// The next row from `from` (exclusive) in direction `back` matching `q`, as
    /// [`ProgramTrace::find`] does inside a recording.
    pub fn find(&self, q: &str, from: Option<usize>, back: bool) -> Option<usize> {
        let n = self.parts.len();
        if n == 0 || q.trim().is_empty() {
            return None;
        }
        let start = from.and_then(|f| self.locate(f));
        let order: Vec<usize> = match (start, back) {
            (Some((p, _)), false) => (p..n).collect(),
            (Some((p, _)), true) => (0..=p).rev().collect(),
            (None, false) => (0..n).collect(),
            (None, true) => (0..n).rev().collect(),
        };
        let ql = q.trim().to_ascii_lowercase();
        let addr = ql.strip_prefix("0x").and_then(|h| u32::from_str_radix(h, 16).ok());
        for p in order {
            let part = &self.parts[p];
            let base = self.starts[p];
            // Inside the starting part: from the row after (before) the cursor.
            let local_from = match start {
                Some((sp, l)) if sp == p => Some(l),
                _ => None,
            };
            let hit = match &part.rows {
                Rows::Trace { t, lost_start, .. } => {
                    let off = usize::from(*lost_start);
                    let from_r = local_from.map(|l| l.saturating_sub(off));
                    // A cursor on the gap row: forward searches start at the first row.
                    let found = match (local_from, *lost_start) {
                        (Some(0), true) if !back => t.find(q, None, false),
                        (Some(0), true) => None,
                        _ => t.find(q, from_r, back),
                    };
                    found.map(|r| r + off)
                }
                Rows::Step { pc, d, func, file, .. } => {
                    let text = d.as_ref().map(|d| format!("{} {}", d.mnemonic, d.ops)).unwrap_or_default().to_ascii_lowercase();
                    let named = |x: &Option<u32>, v: &Vec<String>| x.is_some_and(|i| v[i as usize].to_ascii_lowercase().contains(&ql));
                    let m = match addr {
                        Some(a) => *pc == a,
                        None => text.contains(&ql) || named(func, &self.funcs) || named(file, &self.files),
                    };
                    (m && local_from.is_none()).then_some(0)
                }
                Rows::Marker { note, .. } => (note.to_ascii_lowercase().contains(&ql) && local_from.is_none()).then_some(0),
            };
            if let Some(r) = hit {
                return Some(base + r);
            }
        }
        None
    }

    /// The `Replay` summary of `insns`: what the view holds.
    pub fn summary(&self) -> Value {
        let recorded = self.parts.iter().filter(|p| matches!(p.rows, Rows::Trace { .. })).count();
        let lost = self.parts.iter().filter(|p| matches!(p.rows, Rows::Trace { lost_start: true, .. })).count();
        json!({"Rows": self.total, "Anchored": true, "Memory": true, "History": {"Actions": self.parts.len(), "Recorded": recorded, "LostStart": lost}})
    }
}
