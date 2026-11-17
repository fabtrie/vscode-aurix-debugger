//! The `mcds` Tcl command: arm a miniMCDS trace, read it back, decode the data trace.
//!
//! Program flow decoding needs the code and the symbols, which the core does not have: the
//! TCF/REST trace API decodes it; here `mcds decode` lists the raw program trace messages.

use jim::CmdResult;
use serde_json::{json, Value};

use mcds::{AddrRange, Event, Kind, PtuEvent, Stop, Timestamps, TraceConfig};

use crate::ctx::Cx;
use crate::mcds_trace::{self as mt, Owner, Recording};
use crate::util::parse_u32;

pub const USAGE: &str = "usage: mcds arm|plan|status|stop|decode|dump|init ...";

fn flags<'a>(a: &[&'a str]) -> (Vec<&'a str>, bool) {
    (a.iter().copied().filter(|x| *x != "-json").collect(), a.contains(&"-json"))
}

/// Parses the configuration options of `mcds arm` / `mcds plan`.
pub fn parse_config(a: &[&str]) -> Result<TraceConfig, String> {
    let mut core = 0u8;
    let mut kind: Option<Kind> = None;
    let mut stop: Option<Stop> = None;
    let mut ts: Option<Timestamps> = None;
    let mut filter: Option<AddrRange> = None;
    let mut program = false;
    let mut i = 0;
    let range = |i: usize| -> Result<AddrRange, String> {
        let s = a.get(i + 1).ok_or("missing address")?;
        let n = a.get(i + 2).ok_or("missing length")?;
        AddrRange::new(parse_u32(s)?, parse_u32(n)?)
    };
    while i < a.len() {
        match a[i] {
            "-core" => {
                core = parse_u32(a.get(i + 1).ok_or("missing core")?)? as u8;
                i += 2;
            }
            "-data" => {
                kind = Some(Kind::Data { range: range(i)? });
                i += 3;
            }
            "-program" => {
                program = true;
                i += 1;
            }
            "-filter" => {
                filter = Some(range(i)?);
                i += 3;
            }
            "-stop" => {
                stop = Some(match *a.get(i + 1).ok_or("missing stop condition")? {
                    "full" => Stop::BufferFull,
                    "halt" => Stop::Halt,
                    "manual" => Stop::Manual,
                    o => return Err(format!("unknown stop condition '{o}' (full, halt, manual, or -stopat <addr> <len>)")),
                });
                i += 2;
            }
            "-stopat" => {
                stop = Some(Stop::Function(range(i)?));
                i += 3;
            }
            "-ticks" => {
                ts = Some(Timestamps::Ticks);
                i += 1;
            }
            "-tsu" => {
                ts = Some(Timestamps::Messages);
                i += 1;
            }
            o => return Err(format!("unknown option '{o}'")),
        }
    }
    let kind = match (kind, program) {
        (Some(_), true) => return Err("give -data or -program, not both".into()),
        (Some(k), false) => {
            if filter.is_some() {
                return Err("-filter is for -program".into());
            }
            k
        }
        (None, _) => Kind::Program { filter },
    };
    let stop = stop.unwrap_or(match kind {
        Kind::Data { .. } => Stop::Manual,
        Kind::Program { .. } => Stop::Halt,
    });
    Ok(TraceConfig { core, kind, stop, timestamps: ts.unwrap_or(Timestamps::Messages) })
}

pub fn config_json(cfg: &TraceConfig) -> Value {
    let r = |r: &AddrRange| json!({"start": format!("{:#010x}", r.start), "len": r.len});
    json!({
        "core": cfg.core,
        "kind": match &cfg.kind { Kind::Data { .. } => "data", Kind::Program { .. } => "program" },
        "range": match &cfg.kind { Kind::Data { range } => r(range), Kind::Program { filter: Some(f) } => r(f), _ => Value::Null },
        "stop": match cfg.stop { Stop::BufferFull => json!("full"), Stop::Halt => json!("halt"), Stop::Manual => json!("manual"), Stop::Function(f) => json!({"at": r(&f)}) },
        "timestamps": match cfg.timestamps { Timestamps::Ticks => "ticks", Timestamps::Messages => "tsu" },
    })
}

fn plan_json(cfg: &TraceConfig, p: &mcds::Plan) -> Value {
    json!({
        "config": config_json(cfg),
        "start": format!("{:#06x}", p.start),
        "ring": p.ring,
        "writes": p.writes.len(),
        "resources": {
            "ptu_comparators": p.resources.ptu_comparators,
            "dtu_comparators": p.resources.dtu_comparators,
            "events": p.resources.events.iter().map(|(e, i)| json!({"event": e, "input": i})).collect::<Vec<_>>(),
        },
    })
}

pub fn recording_json(r: &Recording) -> Value {
    json!({
        "config": config_json(&r.cfg),
        "write_ptr": format!("{:#06x}", r.write_ptr),
        "done": r.done,
        "ring": r.ring,
        "paragraphs": r.paragraphs().len(),
        "bytes": r.raw.len(),
    })
}

fn status_json(cx: &Cx) -> Result<Value, String> {
    let (armed, last, lost) = cx.with(|c| (c.mcds.armed.clone(), c.mcds.last.as_ref().map(recording_json), c.mcds.lost_by_reset));
    let mut v = json!({"armed": armed.is_some(), "lost_by_reset": lost, "last": last});
    if let Some(a) = armed {
        let (st, fill) = cx
            .with(|c| -> crate::Result<_> {
                let st = mt::status(c)?;
                let fill = mt::fill(c, &st)?;
                Ok((st, fill))
            })
            .map_err(|e| e.to_string())?;
        v["owner"] = json!(a.owner.as_str());
        v["config"] = config_json(&a.cfg);
        v["ctrl"] = json!(format!("{:#06x}", st.ctrl));
        v["write_ptr"] = json!(format!("{:#06x}", st.write_ptr));
        v["done"] = json!(st.done);
        v["fill"] = json!(fill);
    }
    Ok(v)
}

/// The decoded events of a recording (data trace writes; raw program trace messages).
pub fn decode_json(r: &Recording, max: usize) -> Value {
    let pars = r.paragraphs();
    let (ev, stats) = mcds::decode_data(&pars);
    let ns = |t: Option<u64>| t.map(mcds::units_to_ns);
    let mut list = Vec::new();
    for e in ev.iter().take(max) {
        list.push(match e {
            Event::Data(d) => json!({
                "type": match d.access { mcds::Access::Write => "write", mcds::Access::Read => "read" },
                "t_ns": ns(d.time), "addr": d.addr.map(|a| format!("{a:#010x}")), "value": d.value, "size": d.size, "unit": d.unit,
            }),
            Event::Ptu { time, ev } => match ev {
                PtuEvent::Addr(a) => json!({"type": "address", "t_ns": ns(*time), "addr": format!("{a:#010x}")}),
                PtuEvent::Count(n) => json!({"type": "count", "t_ns": ns(*time), "bytes": n}),
                PtuEvent::Off { count, .. } => json!({"type": "trace_off", "t_ns": ns(*time), "bytes": count}),
            },
            Event::Lost { time, count, .. } => json!({"type": "lost", "t_ns": ns(*time), "messages": count}),
            Event::Undecodable { time } => json!({"type": "undecodable", "t_ns": ns(*time)}),
        });
    }
    json!({"events": list, "total": ev.len(), "messages": stats.messages, "lost": stats.lost, "undecodable": stats.undecodable})
}

pub fn cmd_mcds(cx: &Cx, args: &[&str]) -> CmdResult {
    let (a, json) = flags(args);
    let sub = a.get(1).copied().unwrap_or("");
    let rest = if a.len() > 2 { &a[2..] } else { &[][..] };
    let need_init = || -> Result<(), String> {
        if cx.with(|c| c.initialized) {
            Ok(())
        } else {
            Err("The 'mcds' command must be used after 'init'.".into())
        }
    };
    let out = |v: Value, text: String| -> CmdResult { Ok(if json { v.to_string() } else { text }) };
    match sub {
        "plan" => {
            let cfg = parse_config(rest)?;
            let p = cfg.plan()?;
            if json {
                let mut v = plan_json(&cfg, &p);
                v["register_writes"] = json!(p.writes.iter().map(|(a, v)| format!("{a:#010x}={v:#x}")).collect::<Vec<_>>());
                return Ok(v.to_string());
            }
            let mut s: Vec<String> = p.writes.iter().map(|(a, v)| format!("mww {a:#010x} {v:#x}")).collect();
            s.push(format!("# clear the trace RAM ({:#x}, {} bytes), then", mcds::regs::TRAM, mcds::regs::TRAM_SIZE));
            s.push(format!("mww {:#010x} {:#x}", mcds::regs::CTRL, p.start));
            Ok(s.join("\n"))
        }
        "init" => {
            need_init()?;
            cx.with(mt::init).map_err(|e| e.to_string())?;
            Ok(String::new())
        }
        "arm" => {
            need_init()?;
            let cfg = parse_config(rest)?;
            let c2 = cfg.clone();
            let p = cx.with(|c| mt::arm(c, c2, Owner::Snapshot)).map_err(|e| e.to_string())?;
            let v = plan_json(&cfg, &p);
            let text = format!(
                "armed: {} trace of CPU{}, stop {}, {} register writes",
                v["config"]["kind"].as_str().unwrap_or("?"),
                cfg.core,
                v["config"]["stop"],
                p.writes.len()
            );
            out(v, text)
        }
        "status" => {
            need_init()?;
            let v = status_json(cx)?;
            let text = if v["armed"] == json!(true) {
                format!(
                    "armed ({}): write pointer {}, {:.0} % full, {}",
                    v["owner"].as_str().unwrap_or("?"),
                    v["write_ptr"].as_str().unwrap_or("?"),
                    v["fill"].as_f64().unwrap_or(0.0) * 100.0,
                    if v["done"] == json!(true) { "done" } else { "recording" }
                )
            } else if v["lost_by_reset"] == json!(true) {
                "not armed (a reset ended the recording)".into()
            } else {
                "not armed".into()
            };
            out(v, text)
        }
        "stop" => {
            need_init()?;
            let r = cx.with(|c| mt::stop(c, Owner::Snapshot)).map_err(|e| e.to_string())?;
            let v = recording_json(&r);
            let text = format!("stopped: write pointer {:#06x}, {} paragraphs with data{}", r.write_ptr, v["paragraphs"], if r.done { ", finished by itself" } else { "" });
            out(v, text)
        }
        "decode" => {
            let max = rest.first().map(|s| parse_u32(s)).transpose()?.unwrap_or(1000) as usize;
            let r = cx.with(|c| c.mcds.last.clone()).ok_or("no recording (mcds stop first)")?;
            let v = decode_json(&r, max);
            if json {
                return Ok(v.to_string());
            }
            let mut s: Vec<String> = v["events"].as_array().unwrap().iter().map(|e| e.to_string()).collect();
            s.push(format!("{} events, {} messages, {} lost, {} undecodable", v["total"], v["messages"], v["lost"], v["undecodable"]));
            Ok(s.join("\n"))
        }
        "dump" => {
            let file = rest.first().ok_or("usage: mcds dump <file>")?;
            let r = cx.with(|c| c.mcds.last.clone()).ok_or("no recording (mcds stop first)")?;
            std::fs::write(file, r.to_file()).map_err(|e| format!("{file}: {e}"))?;
            Ok(String::new())
        }
        _ => Err(USAGE.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options() {
        let c = parse_config(&["-data", "0x7000B52C", "4"]).unwrap();
        assert_eq!(c.kind, Kind::Data { range: AddrRange::new(0x7000_B52C, 4).unwrap() });
        assert_eq!(c.stop, Stop::Manual);
        let c = parse_config(&["-program", "-filter", "0x80000000", "0x100", "-stop", "full", "-ticks"]).unwrap();
        assert_eq!(c.stop, Stop::BufferFull);
        assert_eq!(c.timestamps, Timestamps::Ticks);
        assert!(parse_config(&["-data", "1", "4", "-program"]).is_err());
        assert!(parse_config(&["-stop", "never"]).is_err());
    }
}
