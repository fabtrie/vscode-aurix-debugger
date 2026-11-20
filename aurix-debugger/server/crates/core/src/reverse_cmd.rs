//! The `reverse` Tcl command: reverse debugging on the hardware ([`crate::reverse`]).
//!
//! Without the ELF the regions to save are given here (`-region`); the TCF/REST operations take
//! them from the ELF. A restore from Tcl compares the snapshot with the current memory (the
//! decoded recordings that restore shared memory selectively live in the TCF frontend), and goes
//! back by whole actions only.

use jim::CmdResult;
use serde_json::{json, Value};

use crate::ctx::Cx;
use crate::events::{emit, Event};
use crate::reverse::{self, Config, Region};
use crate::util::parse_u32;

pub const USAGE: &str = "usage: reverse on ?-target <name>? ?-history <n>? ?-notrace? ?-strict? ?-verify? ?-maxhits <n>? -region <start> <len> ?-private? ... | off | status | history | back | goto <index> ?-json?";

/// Default most breakpoint hits counted by a re-execution.
pub const DEFAULT_MAX_HITS: u32 = 200;
/// Default checkpoints kept.
pub const DEFAULT_HISTORY: usize = 50;

fn parse_on(cx: &Cx, a: &[&str]) -> Result<Config, String> {
    let mut target = None;
    let mut cfg = Config { target: 0, history: DEFAULT_HISTORY, trace: true, regions: Vec::new(), strict: false, max_hits: DEFAULT_MAX_HITS, verify: false };
    let mut i = 0;
    let arg = |i: usize, what: &str| a.get(i + 1).copied().ok_or_else(|| format!("missing {what}"));
    while i < a.len() {
        match a[i] {
            "-target" => {
                let n = arg(i, "target")?;
                target = Some(cx.with(|c| c.target_by_name(n)).ok_or_else(|| format!("no target {n}"))?);
                i += 2;
            }
            "-history" => {
                cfg.history = parse_u32(arg(i, "count")?)? as usize;
                i += 2;
            }
            "-maxhits" => {
                cfg.max_hits = parse_u32(arg(i, "count")?)?;
                i += 2;
            }
            "-notrace" => {
                cfg.trace = false;
                i += 1;
            }
            "-strict" => {
                cfg.strict = true;
                i += 1;
            }
            "-verify" => {
                cfg.verify = true;
                i += 1;
            }
            "-region" => {
                let start = parse_u32(arg(i, "start")?)?;
                let len = parse_u32(a.get(i + 2).ok_or("missing length")?)?;
                if len == 0 {
                    return Err("a region needs a length".into());
                }
                i += 3;
                let private = a.get(i) == Some(&"-private");
                if private {
                    i += 1;
                }
                cfg.regions.push(Region { start, len, private, foreign: false, name: format!("{start:#010x}") });
            }
            o => return Err(format!("unknown option {o}\n{USAGE}")),
        }
    }
    cfg.target = match target {
        Some(t) => t,
        None => cx.with(|c| c.current_target()).map_err(|e| e.to_string())?,
    };
    Ok(cfg)
}

/// Restores history entry `k` by difference and tells the frontends.
pub fn restore_and_report(cx: &Cx, k: usize) -> Result<Value, String> {
    let rep = cx.with(|c| reverse::restore(c, k, None)).map_err(|e| e.to_string())?;
    let (t, name, pc) = cx.with(|c| {
        let t = c.reverse.target().unwrap_or(0);
        let pc = c.reverse.history.last().map(|cp| cp.pc());
        (t, c.targets[t].name.clone(), pc)
    });
    emit(Event::Restored { target: t, name, pc });
    Ok(report_json(&rep))
}

pub fn report_json(r: &reverse::Report) -> Value {
    json!({
        "checkpoint": r.checkpoint,
        "action": r.kind,
        "registers": r.registers,
        "bytes": r.bytes,
        "by_difference": r.by_difference,
        "outside_bytes": r.outside_bytes,
        "refused": r.refused.iter().map(|(a, n)| serde_json::json!([format!("{a:#010x}"), n])).collect::<Vec<_>>(),
        "ms": r.ms,
    })
}

/// The history entry a Step Back over a whole action goes to: the last checkpoint, or the one
/// before it when the core already stands at the last.
pub fn back_index(c: &crate::ctx::Ctx) -> Option<usize> {
    let n = c.reverse.history.len();
    if c.reverse.at_checkpoint {
        n.checked_sub(2)
    } else {
        n.checked_sub(1)
    }
}

pub fn cmd_reverse(cx: &Cx, args: &[&str]) -> CmdResult {
    let json_out = args.contains(&"-json");
    let a: Vec<&str> = args.iter().copied().filter(|x| *x != "-json").collect();
    let sub = a.get(1).copied().unwrap_or("status");
    let rest = if a.len() > 2 { &a[2..] } else { &[][..] };
    let out = |v: Value, text: String| -> CmdResult { Ok(if json_out { v.to_string() } else { text }) };
    match sub {
        "on" => {
            if !cx.with(|c| c.initialized) {
                return Err("The 'reverse' command must be used after 'init'.".into());
            }
            let cfg = parse_on(cx, rest)?;
            cx.with(|c| reverse::enable(c, cfg)).map_err(|e| e.to_string())?;
            let v = cx.with(|c| reverse::status_json(c));
            let text = format!("reverse debugging on for {}: {} bytes saved per checkpoint", v["target"].as_str().unwrap_or("?"), v["snapshot_bytes"]);
            out(v, text)
        }
        "off" => {
            cx.with(reverse::disable);
            out(json!({"enabled": false}), "reverse debugging off".into())
        }
        "status" => {
            let v = cx.with(|c| reverse::status_json(c));
            let text = if v["enabled"] == json!(true) {
                format!("on for {}: {} checkpoints, {} bytes each", v["target"].as_str().unwrap_or("?"), v["checkpoints"], v["snapshot_bytes"])
            } else {
                "off".into()
            };
            out(v, text)
        }
        "history" => {
            let v = cx.with(|c| reverse::history_json(c));
            let text = v
                .as_array()
                .map(|l| l.iter().map(|e| format!("{} {} at {} -> {}", e["index"], e["kind"].as_str().unwrap_or(""), e["pc"].as_str().unwrap_or(""), e["end_pc"].as_str().unwrap_or("-"))).collect::<Vec<_>>().join("\n"))
                .unwrap_or_default();
            out(v, text)
        }
        "back" => {
            if !cx.with(|c| c.reverse.enabled()) {
                return Err("reverse debugging is off".into());
            }
            let k = cx.with(|c| back_index(c)).ok_or("no history before this point")?;
            let v = restore_and_report(cx, k)?;
            let text = format!("back before {} (checkpoint {}): {} registers, {} bytes", v["action"].as_str().unwrap_or(""), v["checkpoint"], v["registers"].as_array().map_or(0, |r| r.len()), v["bytes"]);
            out(v, text)
        }
        "peek" => {
            // Diagnostics: bytes of a checkpoint's snapshot.
            let k = parse_u32(rest.first().ok_or("usage: reverse peek <index> <addr> <len>")?)? as usize;
            let a = parse_u32(rest.get(1).ok_or("missing address")?)?;
            let n = parse_u32(rest.get(2).ok_or("missing length")?)? as usize;
            let snap = cx.with(|c| reverse::snapshot(c, k)).ok_or("no such checkpoint")?;
            let (s, b) = snap.iter().find(|(s, b)| a >= *s && (a - s) as usize + n <= b.len()).ok_or("not in a region")?;
            let o = (a - s) as usize;
            Ok(b[o..o + n].iter().map(|x| format!("{x:02x}")).collect())
        }
        "goto" => {
            let k = parse_u32(rest.first().ok_or("usage: reverse goto <index>")?)? as usize;
            let v = restore_and_report(cx, k)?;
            let text = format!("at checkpoint {}: {} registers, {} bytes", v["checkpoint"], v["registers"].as_array().map_or(0, |r| r.len()), v["bytes"]);
            out(v, text)
        }
        _ => Err(USAGE.into()),
    }
}
