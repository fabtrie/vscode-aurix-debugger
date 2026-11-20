//! The trace service (`AurixTrace`) and the REST/MCP trace operations: arm a miniMCDS trace from
//! a configuration that names functions, read it back decoded against the code and the symbols.
//!
//! The core moves registers and the trace RAM ([`aurix_core::mcds_trace`]); decoding the program
//! flow needs the instructions (from the ELF's load segments, decoded with Capstone) and the
//! symbols for function names and source lines, which live here.
//!
//! Configuration (JSON):
//! `{"Core":0, "Kind":"program"|"data", "Ranges":[{"Function":"f"} | {"Start":"0x..","Len":n}],
//!   "Stop":"halt"|"full"|"manual"|{"Function":"f"}, "Timestamps":"tsu"|"ticks"}`
//! Program trace: `Ranges` (at most one in this version) limits it to the PC inside the range.
//! Data trace: exactly one range, every write into it.

use std::path::Path;
use std::sync::Arc;

use serde_json::{json, Value};

use aurix_core::mcds_cmd;
use aurix_core::mcds_trace::{self as mt, Owner, Recording};
use mcds::{AddrRange, Kind, Stop, Step, Timestamps, TraceConfig};

use crate::symstore::SymbolSet;
use crate::Shared;

fn range_of(set: Option<&SymbolSet>, v: &Value) -> Result<AddrRange, String> {
    if let Some(name) = v.get("Function").and_then(Value::as_str) {
        let set = set.ok_or("no symbols loaded to look up a function")?;
        for (_, s) in &set.elfs {
            if let Some(f) = s.functions_named(name).into_iter().find(|f| f.high > f.low) {
                return AddrRange::new(f.low, f.high - f.low);
            }
        }
        return Err(format!("unknown function '{name}'"));
    }
    let start = match v.get("Start") {
        Some(Value::String(s)) => aurix_core::util::parse_u32(s)?,
        Some(Value::Number(n)) => n.as_u64().ok_or("bad Start")? as u32,
        _ => return Err("a range needs Function, or Start and Len".into()),
    };
    let len = v.get("Len").and_then(Value::as_u64).ok_or("a range needs Len")? as u32;
    AddrRange::new(start, len)
}

/// Parses a trace configuration, resolving function names with the session's symbols.
pub fn parse_config(sh: &Shared, v: &Value) -> Result<TraceConfig, String> {
    let set = sh.symbols_for("");
    let core = v.get("Core").and_then(Value::as_u64).unwrap_or(0) as u8;
    let ranges: Vec<AddrRange> = match v.get("Ranges") {
        Some(Value::Array(a)) => a.iter().map(|r| range_of(set.as_deref(), r)).collect::<Result<_, _>>()?,
        None | Some(Value::Null) => Vec::new(),
        _ => return Err("Ranges must be an array".into()),
    };
    if ranges.len() > 1 {
        return Err("one range at a time (the inputs of an event are ANDed, so two ranges cannot both enable the trace)".into());
    }
    let kind = match v.get("Kind").and_then(Value::as_str).unwrap_or("program") {
        "program" => Kind::Program { filter: ranges.first().copied() },
        "data" => Kind::Data { range: *ranges.first().ok_or("a data trace needs one range")? },
        o => return Err(format!("unknown Kind '{o}' (program or data)")),
    };
    let stop = match v.get("Stop") {
        None | Some(Value::Null) => match kind {
            Kind::Data { .. } => Stop::Manual,
            Kind::Program { .. } => Stop::Halt,
        },
        Some(Value::String(s)) => match s.as_str() {
            "halt" => Stop::Halt,
            "full" => Stop::BufferFull,
            "manual" => Stop::Manual,
            o => return Err(format!("unknown Stop '{o}' (halt, full, manual or {{\"Function\": ...}})")),
        },
        Some(o) => Stop::Function(range_of(set.as_deref(), o)?),
    };
    let timestamps = match v.get("Timestamps").and_then(Value::as_str).unwrap_or("tsu") {
        "tsu" => Timestamps::Messages,
        "ticks" => Timestamps::Ticks,
        o => return Err(format!("unknown Timestamps '{o}' (tsu or ticks)")),
    };
    Ok(TraceConfig { core, kind, stop, timestamps })
}

/// Estimated window the 8 KB cover, for the configuration form (rough: from the recordings).
fn estimate(cfg: &TraceConfig) -> &'static str {
    match (&cfg.kind, cfg.timestamps) {
        (_, Timestamps::Ticks) => "a few ms (ticks fill the buffer)",
        (Kind::Program { filter: None }, _) => "about 80 µs of execution",
        (Kind::Program { filter: Some(_) }, _) => "depends on how often the range runs (ms)",
        (Kind::Data { .. }, _) => "about 600 writes",
    }
}

pub fn arm_cmd(sh: &Shared, v: &Value) -> Result<Value, String> {
    let cfg = parse_config(sh, v)?;
    let c2 = cfg.clone();
    let plan = sh.handle.call(move |cx| cx.with(|c| mt::arm(c, c2, Owner::Snapshot))).ok_or("the debug server stopped")?.map_err(|e| e.to_string())?;
    Ok(json!({
        "Config": mcds_cmd::config_json(&cfg),
        "Ring": plan.ring,
        "Resources": {
            "PtuComparators": plan.resources.ptu_comparators,
            "DtuComparators": plan.resources.dtu_comparators,
            "Events": plan.resources.events.iter().map(|(e, i)| json!({"Event": e, "Input": i})).collect::<Vec<_>>(),
        },
        "Window": estimate(&cfg),
    }))
}

/// Checks a configuration without arming: the resources it takes, or why it cannot be armed.
pub fn check_cmd(sh: &Shared, v: &Value) -> Result<Value, String> {
    let cfg = parse_config(sh, v)?;
    let plan = cfg.plan()?;
    Ok(json!({
        "Config": mcds_cmd::config_json(&cfg),
        "Ring": plan.ring,
        "Resources": {
            "PtuComparators": plan.resources.ptu_comparators,
            "DtuComparators": plan.resources.dtu_comparators,
            "Events": plan.resources.events.len(),
        },
        "Window": estimate(&cfg),
    }))
}

/// `auto {Config}` arms that configuration whenever the traced core runs (and reads the
/// recording when it halts); `auto {Config: null}` ends it; without `Config` it only reports.
pub fn auto_cmd(sh: &Shared, v: &Value) -> Result<Value, String> {
    match v.get("Config") {
        None => {}
        Some(Value::Null) => {
            sh.handle.call(|cx| cx.with(|c| c.mcds.auto = None)).ok_or("the debug server stopped")?;
        }
        Some(cfg) => {
            let cfg = parse_config(sh, cfg)?;
            cfg.plan()?;
            sh.handle.call(move |cx| cx.with(|c| mt::set_auto(c, Some(cfg)))).ok_or("the debug server stopped")?.map_err(|e| e.to_string())?;
        }
    }
    let auto = sh.handle.call(|cx| cx.with(|c| c.mcds.auto.as_ref().map(mcds_cmd::config_json))).ok_or("the debug server stopped")?;
    Ok(json!({"Auto": auto}))
}

pub fn status_cmd(sh: &Shared) -> Result<Value, String> {
    let r = sh
        .handle
        .call(|cx| {
            cx.with(|c| -> Result<Value, String> {
                let armed = c.mcds.armed.clone();
                let last = c.mcds.last.as_ref().map(mcds_cmd::recording_json);
                let auto = c.mcds.auto.as_ref().map(mcds_cmd::config_json);
                let mut v = json!({"Armed": armed.is_some(), "LostByReset": c.mcds.lost_by_reset, "Last": last, "Auto": auto, "Reverse": c.reverse.enabled()});
                if let Some(a) = armed {
                    v["Owner"] = json!(a.owner.as_str());
                    v["Config"] = mcds_cmd::config_json(&a.cfg);
                    if let Ok(st) = mt::status(c) {
                        v["Done"] = json!(st.done);
                        v["WritePtr"] = json!(st.write_ptr);
                        if let Ok(f) = mt::fill(c, &st) {
                            v["Fill"] = json!(f);
                        }
                    }
                }
                Ok(v)
            })
        })
        .ok_or("the debug server stopped")??;
    Ok(r)
}

pub fn stop_cmd(sh: &Shared) -> Result<Value, String> {
    let r = sh.handle.call(|cx| cx.with(|c| mt::stop(c, Owner::Snapshot))).ok_or("the debug server stopped")?.map_err(|e| e.to_string())?;
    Ok(mcds_cmd::recording_json(&r))
}

/// The code of the ELF files of a symbol set: load segments by address.
#[derive(Default)]
pub(crate) struct Image {
    pub segs: Vec<(u32, Vec<u8>)>,
}

impl Image {
    pub fn load(set: &SymbolSet) -> Image {
        let mut segs = Vec::new();
        for (_, s) in &set.elfs {
            if let Ok(data) = std::fs::read(Path::new(&s.path)) {
                segs.extend(elf_segments(&data));
            }
        }
        Image { segs }
    }

    /// The image of `set`, read once per symbol set and kept in [`Shared::code_image`]: reading a
    /// large ELF takes tens of milliseconds, too much for every decoded step.
    pub fn of(sh: &crate::Shared, set: &std::sync::Arc<SymbolSet>) -> std::sync::Arc<Image> {
        let mut cache = sh.code_image.lock().unwrap();
        if let Some((s, img)) = cache.as_ref() {
            if std::sync::Arc::ptr_eq(s, set) {
                return img.clone();
            }
        }
        let img = std::sync::Arc::new(Image::load(set));
        *cache = Some((set.clone(), img.clone()));
        img
    }

    pub fn read(&self, a: u32, n: usize) -> Option<Vec<u8>> {
        self.segs.iter().find(|(b, d)| a >= *b && ((a - b) as usize) < d.len()).map(|(b, d)| {
            let o = (a - b) as usize;
            d[o..(o + n).min(d.len())].to_vec()
        })
    }
}

/// PT_LOAD segments of a 32-bit little-endian ELF (virtual address, file bytes).
fn elf_segments(d: &[u8]) -> Vec<(u32, Vec<u8>)> {
    let u16_at = |o: usize| d.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    let u32_at = |o: usize| d.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    if d.get(0..4) != Some(b"\x7fELF") || d.get(4) != Some(&1) || d.get(5) != Some(&1) {
        return Vec::new();
    }
    let (Some(phoff), Some(phentsize), Some(phnum)) = (u32_at(0x1C), u16_at(0x2A), u16_at(0x2C)) else { return Vec::new() };
    let mut out = Vec::new();
    for i in 0..phnum as usize {
        let p = phoff as usize + i * phentsize as usize;
        let (Some(ty), Some(off), Some(vaddr), Some(filesz)) = (u32_at(p), u32_at(p + 4), u32_at(p + 8), u32_at(p + 16)) else { continue };
        if ty != 1 || filesz == 0 {
            continue;
        }
        if let Some(bytes) = d.get(off as usize..off as usize + filesz as usize) {
            out.push((vaddr, bytes.to_vec()));
        }
    }
    out
}

fn gap_name(r: mcds::GapReason) -> &'static str {
    match r {
        mcds::GapReason::TraceOff => "trace_off",
        mcds::GapReason::Lost => "lost",
        mcds::GapReason::NoCode => "no_code",
        mcds::GapReason::Inconsistent => "inconsistent",
    }
}

/// Decodes a program recording: instructions with function and source line, gaps.
fn program_json(sh: &Shared, r: &Recording, max: usize) -> Result<Value, String> {
    let set = sh.symbols_for("");
    let pt = crate::trace_insns::program_trace(sh, r)?;
    let (steps, fs, ds, reached) = (&pt.steps, &pt.flow, &pt.decode, pt.reached);
    let halt_pc = pt.halt_pc;
    let (t_start, t_end) = (pt.t_start, pt.t_end);
    // The flow reaches the halt PC when the recording was flushed before the stop (the straight
    // code after the last message is filled in). If it does not (data lost, an older recording),
    // mark the gap; either way the halt point itself is reported as `Halt`.
    let untraced_tail = halt_pc.is_some() && !reached;
    let rel = |t: Option<u64>| t.map(|t| mcds::units_to_ns(t.saturating_sub(t_start.unwrap_or(0))));
    let mut funcs: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    let mut rows = Vec::new();
    let mut truncated = false;
    let skip = steps.len().saturating_sub(max);
    for s in steps.iter().skip(skip) {
        match *s {
            Step::Insn { addr, time } => {
                let (fi, li, fl) = match set.as_deref() {
                    Some(set) => {
                        let fi = set.function_at(addr).map(|f| {
                            funcs.iter().position(|x| *x == f.name).unwrap_or_else(|| {
                                funcs.push(f.name.clone());
                                funcs.len() - 1
                            })
                        });
                        let li = set.line_at(addr);
                        let fl = li.as_ref().map(|l| {
                            files.iter().position(|x| *x == l.file).unwrap_or_else(|| {
                                files.push(l.file.clone());
                                files.len() - 1
                            })
                        });
                        (fi, li.map(|l| l.line), fl)
                    }
                    None => (None, None, None),
                };
                rows.push(json!([addr, rel(time), fi, fl, li]));
            }
            Step::Gap { time, reason } => rows.push(json!({"Gap": gap_name(reason), "T": rel(time)})),
        }
    }
    if skip > 0 {
        truncated = true;
    }
    if untraced_tail {
        rows.push(json!({"Gap": "untraced", "T": rel(t_end)}));
    }
    // The halt point: where the core stands (for a breakpoint, the instruction not yet executed).
    let halt = halt_pc.map(|addr| match set.as_deref() {
        Some(set) => {
            let f = set.function_at(addr).map(|f| f.name.clone());
            let l = set.line_at(addr);
            json!({"Addr": addr, "Function": f, "File": l.as_ref().map(|l| l.file.clone()), "Line": l.map(|l| l.line)})
        }
        None => json!({"Addr": addr}),
    });
    Ok(json!({
        "Kind": "program",
        "Config": mcds_cmd::config_json(&r.cfg),
        "Done": r.done,
        "StartNs": t_start.map(mcds::units_to_ns),
        "EndNs": rel(t_end),
        "HaltPc": r.halt_pc,
        "Halt": halt,
        "ReachesHalt": reached,
        "Functions": funcs,
        "Files": files,
        "Steps": rows,
        "Truncated": truncated,
        "Stats": {
            "Instructions": fs.instructions, "Gaps": fs.trace_off + fs.inconsistent + fs.no_code,
            "Inconsistent": fs.inconsistent, "NoCode": fs.no_code, "TraceOff": fs.trace_off,
            "Messages": ds.messages, "Lost": ds.lost, "Undecodable": ds.undecodable,
        },
        "Columns": ["addr", "t_ns", "function", "file", "line"],
        "Replay": pt.summary(),
        // The core ran, stepped or was reset since the recording ended (the halt above is old).
        "RanSince": r.ran_since,
        // Which recording this is (the status names the last one the same way).
        "StoppedMs": r.stopped_at.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0),
    }))
}

/// Decodes a data recording: the writes, with the variable each one hits.
fn data_json(sh: &Shared, r: &Recording, max: usize) -> Value {
    let set = sh.symbols_for("");
    let pars = r.paragraphs();
    let (ev, stats) = mcds::decode_data(&pars);
    let t_start = ev.iter().find_map(|e| match e {
        mcds::Event::Data(d) => d.time,
        mcds::Event::Lost { time, .. } => *time,
        _ => None,
    });
    let rel = |t: Option<u64>| t.map(|t| mcds::units_to_ns(t.saturating_sub(t_start.unwrap_or(0))));
    let mut rows = Vec::new();
    for e in ev.iter().rev().take(max).collect::<Vec<_>>().into_iter().rev() {
        if let mcds::Event::Data(d) = e {
            let sym = d.addr.and_then(|a| set.as_deref().and_then(|s| global_at(s, a)));
            rows.push(json!({
                "T": rel(d.time), "Addr": d.addr, "Value": d.value, "Size": d.size,
                "Access": match d.access { mcds::Access::Write => "write", mcds::Access::Read => "read" }, "Symbol": sym,
            }));
        } else if let mcds::Event::Lost { time, count, .. } = e {
            rows.push(json!({"T": rel(*time), "Lost": count}));
        }
    }
    json!({
        "Kind": "data",
        "Config": mcds_cmd::config_json(&r.cfg),
        "Done": r.done,
        "StartNs": t_start.map(mcds::units_to_ns),
        "Events": rows,
        "Stats": {"Messages": stats.messages, "Lost": stats.lost, "Undecodable": stats.undecodable, "Events": ev.len()},
    })
}

/// The global at or just below `a` (within 256 bytes; the debug information gives no sizes here),
/// as `name` or `name+offset`.
fn global_at(set: &SymbolSet, a: u32) -> Option<String> {
    let g = set.elfs.iter().flat_map(|(_, s)| s.globals.iter()).filter(|g| g.addr <= a && a - g.addr < 256).max_by_key(|g| g.addr)?;
    let off = a - g.addr;
    Some(if off == 0 { g.name.clone() } else { format!("{}+{off}", g.name) })
}

/// The last recording, decoded (`Max`: the newest `Max` rows).
pub fn read_cmd(sh: &Shared, v: &Value) -> Result<Value, String> {
    let max = v.get("Max").and_then(Value::as_u64).unwrap_or(20_000) as usize;
    let r: Arc<Recording> = sh
        .handle
        .call(|cx| cx.with(|c| c.mcds.last.clone().map(Arc::new)))
        .ok_or("the debug server stopped")?
        .ok_or("no recording (stop the trace first)")?;
    match r.cfg.kind {
        Kind::Program { .. } => program_json(sh, &r, max),
        Kind::Data { .. } => Ok(data_json(sh, &r, max)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elf_segments_of_a_tiny_elf() {
        // 32-bit LE ELF header + one PT_LOAD at 0x80000000 with 4 bytes.
        let mut d = vec![0u8; 0x60];
        d[0..4].copy_from_slice(b"\x7fELF");
        d[4] = 1;
        d[5] = 1;
        d[0x1C..0x20].copy_from_slice(&0x34u32.to_le_bytes());
        d[0x2A..0x2C].copy_from_slice(&32u16.to_le_bytes());
        d[0x2C..0x2E].copy_from_slice(&1u16.to_le_bytes());
        let p = 0x34;
        d[p..p + 4].copy_from_slice(&1u32.to_le_bytes());
        d[p + 4..p + 8].copy_from_slice(&0x5Cu32.to_le_bytes());
        d[p + 8..p + 12].copy_from_slice(&0x8000_0000u32.to_le_bytes());
        d[p + 16..p + 20].copy_from_slice(&4u32.to_le_bytes());
        d[0x5C..0x60].copy_from_slice(&[1, 2, 3, 4]);
        assert_eq!(elf_segments(&d), vec![(0x8000_0000, vec![1, 2, 3, 4])]);
    }
}
