//! The instruction trace: a program recording decoded once, with the register replay
//! ([`mcds::replay`]), served in pages (`insns`), as registers at a row (`regs`), searched
//! (`find`), and as the debugger's view of the core at a row (`replay`).
//!
//! The decoded recording is cached in [`Shared::trace`] until another recording replaces it.
//! The replay starts from the halt registers the recording keeps; memory at the halt is read
//! from the device only while the core still stands where the recording stopped (program flash
//! always comes from the ELF).

use std::sync::Arc;
use std::time::SystemTime;

use serde_json::{json, Value};

use aurix_core::mcds_trace::{self as mt, Recording};
use aurix_core::unwind::Frame;
use mcds::replay::{self, Mark, RegOut, Replay, Row};
use mcds::flow::FlowStats;
use mcds::Step;

use crate::trace::Image;
use crate::Shared;

/// A decoded program recording with its replay.
pub struct ProgramTrace {
    key: (SystemTime, u32),
    pub steps: Vec<Step>,
    pub flow: FlowStats,
    pub decode: mcds::decode::Stats,
    pub reached: bool,
    pub halt_pc: Option<u32>,
    pub t_start: Option<u64>,
    pub t_end: Option<u64>,
    pub replay: Replay,
    /// Per row: function, file (indices into `funcs`, `files`) and line.
    pub(crate) loc: Vec<(Option<u32>, Option<u32>, Option<u32>)>,
    pub funcs: Vec<String>,
    pub files: Vec<String>,
    /// Per row: call depth relative to the shallowest row.
    pub(crate) depth: Vec<u16>,
    /// The call stack at the halt (live unwinder), when anchored.
    halt_frames: Vec<Frame>,
    /// Target index of the traced core.
    pub target: Option<usize>,
    /// Whether memory at the halt came from the device (the core still stood at the halt).
    pub memory: bool,
    /// The flow starts at the PC the core was started from (a reverse debugging checkpoint), and
    /// the replay from its registers as well.
    pub from_entry: bool,
}

/// Where the replay takes memory at the halt from.
pub enum MemAt {
    /// The device, while the core still stands where the recording stopped.
    Live,
    /// Saved bytes (a reverse debugging snapshot of the state at the end): `(start, bytes)`.
    Saved(Vec<(u32, Vec<u8>)>),
}

/// Whether a ring recording went round its trace RAM (its start is lost).
pub fn lapped(r: &Recording) -> bool {
    r.ring && r.paragraphs().first().is_some_and(|p| p.0 != 0)
}

fn is_call(m: &str) -> bool {
    matches!(m, "call" | "calla" | "calli" | "fcall" | "fcalla" | "fcalli")
}
fn is_ret(m: &str) -> bool {
    matches!(m, "ret" | "rfe" | "fret")
}

/// The decoded recording for `r`, from the cache or decoded now.
pub fn program_trace(sh: &Shared, r: &Recording) -> Result<Arc<ProgramTrace>, String> {
    let key = (r.stopped_at, r.write_ptr);
    if let Some(t) = sh.trace.lock().unwrap().as_ref().filter(|t| t.key == key) {
        return Ok(t.clone());
    }
    // A reverse debugging action's recording starts at its checkpoint: decode it from there.
    let (stopped, wp) = (r.stopped_at, r.write_ptr);
    let entry = sh
        .handle
        .call(move |cx| {
            cx.with(|c| {
                c.reverse
                    .history
                    .iter()
                    .find(|cp| cp.from_start && cp.recording.as_ref().is_some_and(|x| x.stopped_at == stopped && x.write_ptr == wp))
                    .map(|cp| cp.regs.clone())
            })
        })
        .flatten();
    let t = Arc::new(build_with(sh, r, entry.as_deref(), MemAt::Live)?);
    *sh.trace.lock().unwrap() = Some(t.clone());
    Ok(t)
}

/// Decodes `r` with its register replay. `entry`: the registers the core was started from when
/// the recording was armed right there (reverse debugging), so the flow and the replay start
/// from them too; `mem`: memory at the end of the recording.
pub fn build_with(sh: &Shared, r: &Recording, entry: Option<&[u32]>, mem: MemAt) -> Result<ProgramTrace, String> {
    let set = sh.symbols_for("");
    let img = set.as_deref().map(Image::load).unwrap_or_default();
    if img.segs.is_empty() {
        return Err("no code to decode the program trace with (load the ELF of the application)".into());
    }
    let mut code = mcds::flow::CapstoneCode::new(|a: u32, n: usize| img.read(a, n))?;
    let pars = r.paragraphs();
    // A ring recording ends where the core halted (stop on halt, or a manual stop of a halted core:
    // nothing is recorded while it is halted); a linear one may have filled long before.
    let halt_pc = r.halt_pc.filter(|_| r.ring);
    let entry = entry.filter(|e| e.len() > 36 && !lapped(r));
    let start_pc = entry.map(|e| e[36]);
    let (steps, flow, decode, reached) = mcds::decode_program_between(&pars, &mut code, start_pc, halt_pc);
    let t_start = steps.iter().find_map(|s| match s {
        Step::Insn { time, .. } | Step::Gap { time, .. } => *time,
    });
    let t_end = steps.iter().rev().find_map(|s| match s {
        Step::Insn { time, .. } | Step::Gap { time, .. } => *time,
    });

    // The replay starts from the halt registers when the flow reaches the halt. Memory at the
    // halt is only right while the core has not run since the stop.
    let core = r.cfg.core as u32;
    let (target, now_regs) = sh
        .handle
        .call(move |cx| {
            cx.with(|c| {
                let t = c.targets.iter().position(|t| matches!(&t.core, aurix_core::target::Core::Tricore(tc) if tc.coreid == core));
                (t, mt::halted_regs(c, core))
            })
        })
        .unwrap_or((None, None));
    let anchor = r.halt_regs.clone().filter(|_| reached && halt_pc.is_some());
    let live = matches!(mem, MemAt::Live) && anchor.is_some() && now_regs.is_some() && now_regs == r.halt_regs;
    let saved = match mem {
        MemAt::Saved(v) => v,
        MemAt::Live => Vec::new(),
    };
    let memory = live || (!saved.is_empty() && anchor.is_some());
    let mut read = |a: u32, n: usize| -> Option<Vec<u8>> {
        if a >> 28 == 0x8 {
            if let Some(b) = img.read(a, n).filter(|b| b.len() == n) {
                return Some(b);
            }
        }
        if !memory {
            return None;
        }
        if !live {
            let ga = replay::place(r.cfg.core, a).0;
            return saved.iter().find(|(s, b)| ga >= *s && (ga - s) as usize + n <= b.len()).map(|(s, b)| b[(ga - s) as usize..(ga - s) as usize + n].to_vec());
        }
        sh.handle.call(move |cx| cx.with(|c| c.io().and_then(|mut io| io.read(a, n)).ok())).flatten()
    };
    let halt = anchor.map(replay::HaltRegs);
    let entry_regs = entry.map(|e| replay::HaltRegs(e.to_vec()));
    let replay = Replay::build_between(&steps, &mut code, entry_regs.as_ref(), halt.as_ref(), &mut read, r.cfg.core);

    let halt_frames = match (target, halt.is_some() && live) {
        (Some(t), true) => crate::stack::unwind_core(sh, t).unwrap_or_default(),
        _ => Vec::new(),
    };

    // Source locations and call depth per row.
    let mut funcs: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let mut loc = Vec::with_capacity(replay.rows.len());
    let mut depth_raw: Vec<i32> = Vec::with_capacity(replay.rows.len());
    let mut d = 0i32;
    for row in &replay.rows {
        match row {
            Row::Insn { addr, d: dec, .. } => {
                let l = match set.as_deref() {
                    Some(set) => {
                        let fi = set.function_at(*addr).map(|f| index_of(&mut funcs, &f.name));
                        let li = set.line_at(*addr);
                        let fl = li.as_ref().map(|l| index_of(&mut files, &l.file));
                        (fi, fl, li.map(|l| l.line))
                    }
                    None => (None, None, None),
                };
                loc.push(l);
                depth_raw.push(d);
                if is_call(&dec.mnemonic) {
                    d += 1;
                } else if is_ret(&dec.mnemonic) {
                    d -= 1;
                }
            }
            Row::Entry { .. } => {
                loc.push((None, None, None));
                d += 1;
                depth_raw.push(d);
            }
            Row::Gap { .. } => {
                loc.push((None, None, None));
                depth_raw.push(d);
            }
        }
    }
    let min = depth_raw.iter().copied().min().unwrap_or(0);
    let depth = depth_raw.iter().map(|&x| (x - min).min(u16::MAX as i32) as u16).collect();
    Ok(ProgramTrace {
        key: (r.stopped_at, r.write_ptr),
        steps,
        flow,
        decode,
        reached,
        halt_pc,
        t_start,
        t_end,
        replay,
        loc,
        funcs,
        files,
        depth,
        halt_frames,
        target,
        memory,
        from_entry: entry.is_some(),
    })
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

fn mark_name(m: Option<Mark>) -> Value {
    match m {
        Some(Mark::Exact) => json!("exact"),
        Some(Mark::Mem) => json!("mem"),
        None => Value::Null,
    }
}

fn reg_json(r: &RegOut) -> Value {
    json!([r.name, if r.known == 0 { Value::Null } else { json!(r.value) }, mark_name(r.mark), if r.known == u32::MAX || r.known == 0 { Value::Null } else { json!(r.known) }])
}

impl ProgramTrace {
    pub fn rows(&self) -> usize {
        self.replay.rows.len()
    }

    fn time(&self, row: usize) -> Option<u64> {
        let step = match self.replay.rows.get(row)? {
            Row::Insn { step, .. } | Row::Entry { step, .. } | Row::Gap { step } => *step,
        };
        match self.steps.get(step)? {
            Step::Insn { time, .. } | Step::Gap { time, .. } => time.map(|t| mcds::units_to_ns(t.saturating_sub(self.t_start.unwrap_or(0)))),
        }
    }

    pub(crate) fn row_addr(&self, row: usize) -> Option<u32> {
        match self.replay.rows.get(row)? {
            Row::Insn { addr, .. } => Some(*addr),
            _ => None,
        }
    }

    /// The address executed after `row` (the halt PC after the last one).
    pub(crate) fn next_pc(&self, row: usize) -> Option<u32> {
        match self.replay.rows.get(row + 1) {
            Some(Row::Insn { addr, .. }) => Some(*addr),
            Some(Row::Entry { vector, .. }) => Some(*vector),
            Some(Row::Gap { .. }) => None,
            None => self.halt_pc.filter(|_| self.reached),
        }
    }

    /// Whether a conditional branch at `row` was taken.
    fn branch(&self, row: usize) -> Option<bool> {
        let Row::Insn { addr, d, .. } = self.replay.rows.get(row)? else { return None };
        let ops = replay::operand::parse(&d.ops)?;
        match replay::model::classify(&d.mnemonic, &ops) {
            replay::model::Next::Cond(t) => {
                let n = match self.replay.rows.get(row + 1) {
                    Some(Row::Insn { addr, .. }) => *addr,
                    None => self.halt_pc?,
                    _ => return None,
                };
                let fall = addr.wrapping_add(d.size as u32);
                if n == t && t != fall {
                    Some(true)
                } else if n == fall {
                    Some(false)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    pub fn row_json(&self, i: usize) -> Value {
        let (f, fl, l) = self.loc.get(i).copied().unwrap_or((None, None, None));
        let mut v = json!({"I": i, "T": self.time(i), "D": self.depth.get(i)});
        match &self.replay.rows[i] {
            Row::Insn { addr, d, .. } => {
                v["K"] = json!("i");
                v["A"] = json!(addr);
                v["S"] = json!(d.size);
                v["M"] = json!(d.mnemonic);
                v["O"] = json!(d.ops);
                v["F"] = json!(f);
                v["Fl"] = json!(fl);
                v["L"] = json!(l);
                if let Some(b) = self.branch(i) {
                    v["B"] = json!(b);
                }
            }
            Row::Entry { resume, vector, .. } => {
                v["K"] = json!("e");
                v["A"] = json!(vector);
                v["Resume"] = json!(resume);
            }
            Row::Gap { .. } => v["K"] = json!("g"),
        }
        let w: Vec<Value> = self.replay.written(i).iter().map(reg_json).collect();
        if !w.is_empty() {
            v["W"] = json!(w);
        }
        let x: Vec<Value> = self
            .replay
            .accesses(i)
            .into_iter()
            .filter(|a| !a.context)
            .map(|a| json!({"S": a.store, "A": a.addr, "L": a.len, "V": a.value}))
            .collect();
        if !x.is_empty() {
            v["X"] = json!(x);
        }
        if self.replay.accesses(i).iter().any(|a| a.context) {
            v["Ctx"] = json!(true);
        }
        v
    }

    /// The replay summary for `trace_read`.
    pub fn summary(&self) -> Value {
        let n = self.rows();
        let near = n.checked_sub(1).map(|r| self.replay.known_share(Some(r)));
        let start = (n > 0).then(|| self.replay.known_share(Some(0)));
        let unmodelled: u32 = self.replay.stats.unmodelled.iter().map(|x| x.1).sum();
        json!({
            "Rows": n,
            "Anchored": self.replay.stats.anchored,
            "Memory": self.memory,
            "Entries": self.replay.stats.entries,
            "KnownNearHalt": near,
            "KnownAtStart": start,
            "Unmodelled": unmodelled,
            "UnmodelledTop": self.replay.stats.unmodelled.iter().take(8).map(|(m, c)| json!([m, c])).collect::<Vec<_>>(),
            "Conflicts": self.replay.stats.conflicts,
        })
    }

    /// The call stack after `row`: the frames the trace shows open, the frames it shows end
    /// later (returns below the window's first frame), then the halt's outer frames.
    pub fn frames(&self, row: usize) -> Vec<Frame> {
        let regs_frame = |state: Option<usize>, pc: u32, upper_only: bool| -> Frame {
            let rs = self.replay.regs(state);
            let mut regs = [None; 32];
            for (k, r) in rs.iter().take(32).enumerate() {
                let upper = matches!(k, 8..=15 | 26..=31);
                if r.is_known() && (!upper_only || upper) {
                    regs[k] = Some(r.value);
                }
            }
            let pcxi = rs[33].is_known().then_some(rs[33].value).unwrap_or(0);
            let psw = rs[32].is_known().then_some(rs[32].value);
            Frame { pc, pcxi, psw, regs, arm: false }
        };
        let kind = |r: &Row| match r {
            Row::Insn { d, .. } => (is_call(&d.mnemonic), is_ret(&d.mnemonic)),
            Row::Entry { .. } => (true, false),
            Row::Gap { .. } => (false, false),
        };
        // Calls open at `row`.
        let mut open: Vec<usize> = Vec::new();
        for (i, r) in self.replay.rows.iter().enumerate().take(row + 1) {
            match kind(r) {
                (true, _) => open.push(i),
                (_, true) => {
                    open.pop();
                }
                _ => {}
            }
        }
        // Returns after `row` that end frames older than the window's first one.
        let (mut still_open, mut pushed) = (open.len(), 0usize);
        let mut later_pops: Vec<usize> = Vec::new();
        for (i, r) in self.replay.rows.iter().enumerate().skip(row + 1) {
            match kind(r) {
                (true, _) => pushed += 1,
                (_, true) if pushed > 0 => pushed -= 1,
                (_, true) if still_open > 0 => still_open -= 1,
                (_, true) => later_pops.push(i),
                _ => {}
            }
        }
        let mut frames = vec![regs_frame(Some(row), self.next_pc(row).unwrap_or(0), false)];
        for &c in open.iter().rev() {
            let pc = match &self.replay.rows[c] {
                Row::Insn { addr, d, .. } => addr.wrapping_add(d.size as u32),
                Row::Entry { resume, .. } => resume.unwrap_or(0),
                _ => 0,
            };
            frames.push(regs_frame(c.checked_sub(1), pc, true));
        }
        for &p in &later_pops {
            frames.push(regs_frame(Some(p), self.next_pc(p).unwrap_or(0), true));
        }
        // The halt's frames below the ones the window opened.
        let opened_at_halt = self.open_at_halt();
        frames.extend(self.halt_frames.iter().skip(1 + opened_at_halt).cloned());
        frames
    }

    /// How many frames of the halt's call stack the window itself opened.
    fn open_at_halt(&self) -> usize {
        let mut d = 0usize;
        for r in &self.replay.rows {
            match r {
                Row::Insn { d: dec, .. } if is_call(&dec.mnemonic) => d += 1,
                Row::Insn { d: dec, .. } if is_ret(&dec.mnemonic) => d = d.saturating_sub(1),
                Row::Entry { .. } => d += 1,
                _ => {}
            }
        }
        d
    }

    fn explain(&self, r: &RegOut) -> String {
        self.replay.explain(r, &|row| format!("#{}", row + 1))
    }

    /// Registers after `row` with their provenance, and the call stack there.
    pub fn regs_json(&self, sh: &Shared, row: usize) -> Value {
        let regs = self.replay.regs(Some(row));
        let written: Vec<&'static str> = self.replay.written(row).iter().map(|r| r.name).collect();
        let set = sh.symbols_for("");
        let frames: Vec<Value> = self
            .frames(row)
            .iter()
            .map(|f| {
                let (func, file, line) = match set.as_deref() {
                    Some(s) => {
                        // A caller frame's PC is its return address: look up the call.
                        let at = f.pc;
                        (s.function_at(at).map(|x| x.name.clone()), s.line_at(at).map(|l| l.file), s.line_at(at).map(|l| l.line))
                    }
                    None => (None, None, None),
                };
                json!({"Pc": f.pc, "Function": func, "File": file, "Line": line})
            })
            .collect();
        json!({
            "At": row,
            "Pc": self.next_pc(row),
            "Regs": regs.iter().map(|r| json!({
                "N": r.name,
                "V": if r.known == 0 { Value::Null } else { json!(r.value) },
                "K": if r.known == u32::MAX || r.known == 0 { Value::Null } else { json!(r.known) },
                "M": mark_name(r.mark),
                "W": written.contains(&r.name),
                "Why": self.explain(r),
            })).collect::<Vec<_>>(),
            "Frames": frames,
        })
    }

    /// The next row from `from` (exclusive) in direction `dir` matching `q`: an address
    /// (`0x8000...`), a register condition (`d15=0xC`, `a4 changes`), or text in the instruction,
    /// function or file.
    pub fn find(&self, q: &str, from: Option<usize>, back: bool) -> Option<usize> {
        let q = q.trim();
        let n = self.rows();
        if n == 0 || q.is_empty() {
            return None;
        }
        let order: Vec<usize> = match (from, back) {
            (Some(f), false) => (f + 1..n).collect(),
            (Some(f), true) => (0..f.min(n)).rev().collect(),
            (None, false) => (0..n).collect(),
            (None, true) => (0..n).rev().collect(),
        };
        let addr = q.strip_prefix("0x").and_then(|h| u32::from_str_radix(h, 16).ok());
        let cond = q.split_once('=').map(|(r, v)| (r.trim().to_ascii_lowercase(), aurix_core::util::parse_u32(v.trim()).ok()));
        let changes = q.strip_suffix(" changes").map(|r| r.trim().to_ascii_lowercase());
        let ql = q.to_ascii_lowercase();
        order.into_iter().find(|&i| {
            if let Some(a) = addr {
                return self.row_addr(i) == Some(a);
            }
            if let Some((r, Some(v))) = &cond {
                return self.replay.regs(Some(i)).iter().any(|x| x.name == r && x.is_known() && x.value == *v);
            }
            if let Some(r) = &changes {
                return self.replay.written(i).iter().any(|x| x.name == r);
            }
            let (f, fl, _) = self.loc[i];
            let text = match &self.replay.rows[i] {
                Row::Insn { d, .. } => format!("{} {}", d.mnemonic, d.ops),
                Row::Entry { .. } => "interrupt entry".into(),
                Row::Gap { .. } => "gap".into(),
            };
            text.to_ascii_lowercase().contains(&ql)
                || f.is_some_and(|f| self.funcs[f as usize].to_ascii_lowercase().contains(&ql))
                || fl.is_some_and(|f| self.files[f as usize].to_ascii_lowercase().contains(&ql))
        })
    }

    /// The debugger's registers (debugger numbering) at a replay position, `None` = unknown.
    pub fn overlay_regs(&self, row: usize) -> Vec<Option<u32>> {
        let rs = self.replay.regs(Some(row));
        let mut out = vec![None; 42];
        for (k, r) in rs.iter().take(32).enumerate() {
            out[k] = r.is_known().then_some(r.value);
        }
        out[33] = rs[34].is_known().then_some(rs[34].value);
        out[34] = rs[33].is_known().then_some(rs[33].value);
        out[35] = rs[32].is_known().then_some(rs[32].value);
        out[36] = self.next_pc(row);
        out[37] = rs[35].is_known().then_some(rs[35].value);
        out
    }
}

/// The last recording, if it is a program trace, decoded.
pub fn current(sh: &Shared) -> Result<Arc<ProgramTrace>, String> {
    current_with(sh).map(|(t, _)| t)
}

/// [`current`] and why the traced core no longer stands where it ended (see
/// `Recording::ran_since`): the cached decode does not change with that.
pub fn current_with(sh: &Shared) -> Result<(Arc<ProgramTrace>, Option<&'static str>), String> {
    let r: Arc<Recording> = sh
        .handle
        .call(|cx| cx.with(|c| c.mcds.last.clone().map(Arc::new)))
        .ok_or("the debug server stopped")?
        .ok_or("no recording (stop the trace first)")?;
    if !matches!(r.cfg.kind, mcds::Kind::Program { .. }) {
        return Err("the last recording is a data trace".into());
    }
    Ok((program_trace(sh, &r)?, r.ran_since))
}

/// Why a replay of a recording is refused when the core ran since it ended.
fn ran_since_text(why: &str) -> String {
    let what = match why {
        "reset" => "the device was reset",
        "step" => "the core stepped",
        _ => "the core ran",
    };
    format!("the trace ends at an earlier halt ({what} since): there is no trace up to the current halt; arm it and run again")
}

/// `insns {From, Count}`: a page of rows.
pub fn insns_cmd(sh: &Shared, v: &Value) -> Result<Value, String> {
    let (t, ran_since) = current_with(sh)?;
    let n = t.rows();
    let from = (v.get("From").and_then(Value::as_u64).unwrap_or(0) as usize).min(n);
    let count = v.get("Count").and_then(Value::as_u64).unwrap_or(500).min(5000) as usize;
    let rows: Vec<Value> = (from..(from + count).min(n)).map(|i| t.row_json(i)).collect();
    Ok(json!({"Total": n, "From": from, "Rows": rows, "Functions": t.funcs, "Files": t.files, "EndNs": t.t_end.map(|e| mcds::units_to_ns(e.saturating_sub(t.t_start.unwrap_or(0)))), "HaltPc": t.halt_pc, "Reached": t.reached, "Replay": t.summary(), "RanSince": ran_since}))
}

/// `regs {At}`: the registers after a row.
pub fn regs_cmd(sh: &Shared, v: &Value) -> Result<Value, String> {
    let t = current(sh)?;
    let n = t.rows();
    if n == 0 {
        return Err("the recording has no instructions".into());
    }
    let at = (v.get("At").and_then(Value::as_u64).unwrap_or(n as u64 - 1) as usize).min(n - 1);
    if v.get("Before").and_then(Value::as_bool) == Some(true) {
        // The state before the row (before the first row: where the recording starts).
        let rs = t.replay.regs(at.checked_sub(1));
        return Ok(json!({"Before": at, "Regs": rs.iter().map(|r| json!({"N": r.name, "V": if r.known == 0 { Value::Null } else { json!(r.value) }, "K": if r.known == u32::MAX || r.known == 0 { Value::Null } else { json!(r.known) }, "M": mark_name(r.mark), "Why": t.explain(r)})).collect::<Vec<_>>()}));
    }
    Ok(t.regs_json(sh, at))
}

/// `find {Query, From, Back}`.
pub fn find_cmd(sh: &Shared, v: &Value) -> Result<Value, String> {
    let t = current(sh)?;
    let q = v.get("Query").and_then(Value::as_str).unwrap_or("");
    let from = v.get("From").and_then(Value::as_u64).map(|x| x as usize);
    let back = v.get("Back").and_then(Value::as_bool).unwrap_or(false);
    Ok(json!({"At": t.find(q, from, back)}))
}

/// The replay position the debugger shows instead of the halted core.
#[derive(Clone)]
pub struct ReplayView {
    pub target: usize,
    pub row: usize,
    pub regs: Vec<Option<u32>>,
    pub frames: Vec<Frame>,
}

/// The replay frames for target `idx`, while the debugger shows a replay of it.
pub fn replay_frames(sh: &Shared, idx: usize) -> Option<Vec<Frame>> {
    sh.replay.lock().unwrap().as_ref().filter(|r| r.target == idx).map(|r| r.frames.clone())
}

/// Ends a replay of target `idx` (it ran, halted again or was reset), or of any target.
pub fn end_replay(sh: &Shared, idx: Option<usize>) {
    let mut r = sh.replay.lock().unwrap();
    if r.as_ref().is_some_and(|v| idx.is_none_or(|i| v.target == i)) {
        *r = None;
    }
}

/// `replay {At}` / `replay {Off: true}`: shows the core at a trace row in the debugger (stack,
/// registers, locals) or the live core again.
pub fn replay_cmd(sh: &Shared, v: &Value) -> Result<Value, String> {
    if v.get("Off").and_then(Value::as_bool) == Some(true) {
        let old = sh.replay.lock().unwrap().take();
        let ctx = old.as_ref().map(|o| crate::contexts::core_id_of(&sh.handle, o.target));
        if let Some(o) = old {
            sh.stacks.lock().unwrap().remove(&o.target);
        }
        return Ok(json!({"Replay": false, "Ctx": ctx}));
    }
    if sh.handle.call(|cx| cx.with(|c| c.reverse.enabled())).unwrap_or(false) {
        return Err("reverse debugging is on: Step Back restores the target instead of replaying the trace".into());
    }
    let (t, ran_since) = current_with(sh)?;
    if let Some(why) = ran_since {
        return Err(ran_since_text(why));
    }
    let target = t.target.ok_or("the traced core is not a target of this session")?;
    if !t.replay.stats.anchored {
        return Err("the recording does not end at the core's halt: there is no state to replay from".into());
    }
    let n = t.rows();
    let row = (v.get("At").and_then(Value::as_u64).unwrap_or(n as u64 - 1) as usize).min(n.saturating_sub(1));
    let halted = sh
        .handle
        .call(move |cx| cx.with(|c| c.targets.get(target).is_some_and(|x| x.core.state() == aurix_core::target::TargetState::Halted)))
        .unwrap_or(false);
    if !halted {
        return Err("the traced core runs: replay needs it halted where the trace ended".into());
    }
    let frames = t.frames(row);
    let nframes = frames.len();
    *sh.replay.lock().unwrap() = Some(ReplayView { target, row, regs: t.overlay_regs(row), frames });
    let (f, fl, l) = t.loc.get(row + 1).or(t.loc.get(row)).copied().unwrap_or((None, None, None));
    Ok(json!({
        "Replay": true,
        "Ctx": crate::contexts::core_id_of(&sh.handle, target),
        "At": row,
        "Total": n,
        "Pc": t.next_pc(row),
        "Function": f.map(|f| t.funcs[f as usize].clone()),
        "File": fl.map(|f| t.files[f as usize].clone()),
        "Line": l,
        "T": t.time(row),
        "EndNs": t.t_end.map(|e| mcds::units_to_ns(e.saturating_sub(t.t_start.unwrap_or(0)))),
        "Frames": nframes,
        // Where the core really stands (the replay is view only).
        "HaltPc": t.halt_pc,
        "HaltFile": t.halt_pc.and_then(|pc| sh.symbols_for("").and_then(|s| s.line_at(pc)).map(|l| l.file)),
        "HaltLine": t.halt_pc.and_then(|pc| sh.symbols_for("").and_then(|s| s.line_at(pc)).map(|l| l.line)),
    }))
}

/// Replay steps for the debugger: the row a step/back/continue lands on.
/// `Kind`: `stepBack`, `step`, `stepInsnBack`, `stepInsn`, `reverseContinue`, `continue`,
/// `stepOut`, `stepOutBack`; `Breakpoints`: addresses to stop at.
pub fn replay_move(sh: &Shared, v: &Value) -> Result<Value, String> {
    let t = current(sh)?;
    let n = t.rows();
    if n == 0 {
        return Err("no instructions".into());
    }
    let cur = sh.replay.lock().unwrap().as_ref().map(|r| r.row).unwrap_or(n - 1);
    let kind = v.get("Kind").and_then(Value::as_str).unwrap_or("stepBack");
    // Breakpoints to stop at: given, or the code breakpoints of the session.
    let bps: Vec<u32> = match v.get("Breakpoints").and_then(Value::as_array) {
        Some(a) => a.iter().filter_map(Value::as_u64).map(|x| x as u32).collect(),
        None => sh
            .bps
            .lock()
            .unwrap()
            .bps
            .values()
            .flat_map(|b| b.instances.iter().filter(|i| i.error.is_none() && i.kind == crate::breakpoints::Kind::Code).map(|i| i.addr))
            .collect(),
    };
    // The position "after row r" stands before row r+1: its line is that row's line.
    let line_at = |r: usize| t.loc.get(r + 1).map(|x| (x.1, x.2)).unwrap_or((None, None));
    let depth_at = |r: usize| t.depth.get(r + 1).copied().unwrap_or(0);
    let insn_row = |r: usize| matches!(t.replay.rows.get(r + 1), Some(Row::Insn { .. })) || r + 1 == n;
    let target = match kind {
        "stepInsnBack" => cur.saturating_sub(1),
        "stepInsn" => (cur + 1).min(n),
        "stepBack" | "step" | "stepIn" | "stepInBack" => {
            let back = kind.ends_with("Back");
            let into = kind.starts_with("stepIn");
            let (l0, d0) = (line_at(cur), depth_at(cur));
            let mut r = cur;
            loop {
                if back {
                    if r == 0 {
                        break;
                    }
                    r -= 1;
                } else {
                    r += 1;
                    if r >= n {
                        break;
                    }
                }
                let (l, dd) = (line_at(r), depth_at(r));
                if insn_row(r) && l.1.is_some() && (l != l0 || dd != d0) && (into || dd <= d0) {
                    break;
                }
            }
            r
        }
        "stepOutBack" | "stepOut" => {
            let d0 = depth_at(cur);
            let mut r = cur;
            loop {
                if kind == "stepOutBack" {
                    if r == 0 {
                        break;
                    }
                    r -= 1;
                } else {
                    r += 1;
                    if r >= n {
                        break;
                    }
                }
                if depth_at(r) < d0 && insn_row(r) {
                    break;
                }
            }
            r
        }
        "reverseContinue" | "continue" => {
            let mut r = cur;
            loop {
                if kind == "reverseContinue" {
                    if r == 0 {
                        break;
                    }
                    r -= 1;
                } else {
                    r += 1;
                    if r >= n {
                        break;
                    }
                }
                if t.row_addr(r + 1).is_some_and(|a| bps.contains(&a)) {
                    break;
                }
            }
            r
        }
        o => return Err(format!("unknown replay step '{o}'")),
    };
    if target + 1 >= n && !kind.ends_with("Back") && kind != "reverseContinue" {
        // At or past the last row, the position is the halt itself: show the live core.
        let mut v = replay_cmd(sh, &json!({"Off": true}))?;
        v["AtHalt"] = json!(true);
        return Ok(v);
    }
    replay_cmd(sh, &json!({"At": target}))
}
