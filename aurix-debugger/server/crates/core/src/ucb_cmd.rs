//! The `ucb` Tcl command: read, show, stage, review, write, export the TC3xx UCBs.
//!
//! With `-json` the subcommands answer with the JSON the REST/MCP operations and the VS Code UCB
//! panel use; without it with a short text for the console.

use jim::CmdResult;
use serde_json::{json, Value};

use crate::ctx::Cx;
use crate::error::Error;
use crate::image::ImageFile;
use crate::ucb::{self, Class, Selection, State, Which, UCBS};
use crate::ucb_write::{self as uw, WriteOptions};

pub const USAGE: &str = "usage: ucb read|show|set|confirm|unset|discard|staged|compare|apply|revert|review|write|restore|password|unlock|lock|export|dir|status ...";

fn e2s(e: Error) -> String {
    e.to_string()
}

/// Splits `-json` and `-reveal` off the arguments.
fn flags<'a>(a: &[&'a str]) -> (Vec<&'a str>, bool, bool) {
    let json = a.contains(&"-json");
    let reveal = a.contains(&"-reveal");
    (a.iter().copied().filter(|x| *x != "-json" && *x != "-reveal").collect(), json, reveal)
}

fn view(cx: &Cx, r: &ucb::Readout, only: Option<usize>, reveal: bool) -> Value {
    let (staged, dir, enabled, known) = cx.with(|c| {
        let dbg = c.adapter_settings.debug_password.is_some();
        let mut known: Vec<&'static str> = c.ucb.passwords.keys().copied().collect();
        if dbg {
            known.push("DBG");
        }
        (c.ucb.staged.clone(), c.ucb.dir(), c.ucb.write_enabled, known)
    });
    let baseline = ucb::baseline(&dir, r);
    ucb::view_json(r, &staged, &ucb::ViewOptions { reveal, only, baseline: baseline.as_ref(), write_enabled: enabled, known_passwords: &known })
}

fn text_view(v: &Value) -> String {
    let mut out = vec![format!(
        "{} (chip ID {}, UID {})  debug: {}, boot: {}, HSM: {}, OTP: {}",
        v["device"]["name"].as_str().unwrap_or("?"),
        v["device"]["chip_id"].as_str().unwrap_or("?"),
        v["device"]["uid"].as_str().unwrap_or("-"),
        v["status"]["debug"].as_str().unwrap_or("?"),
        v["status"]["boot"].as_str().unwrap_or("?"),
        v["status"]["hsm"].as_str().unwrap_or("?"),
        v["status"]["otp"].as_str().unwrap_or("?"),
    )];
    for u in v["ucbs"].as_array().into_iter().flatten() {
        let mut tags = Vec::new();
        if u["changed"].as_u64().unwrap_or(0) > 0 {
            tags.push(format!("{} changed", u["changed"]));
        }
        if u["orig_copy_differ"] == json!(true) {
            tags.push("ORIG != COPY".into());
        }
        if u["staged"].as_u64().unwrap_or(0) > 0 {
            tags.push(format!("{} staged", u["staged"]));
        }
        out.push(format!("  UCB_{:<9} {:<10} {}", u["name"].as_str().unwrap_or(""), u["state"].as_str().unwrap_or(""), tags.join(", ")));
    }
    out.join("\n")
}

fn detail_text(u: &Value) -> String {
    let mut out = vec![format!("UCB_{} {} ({})", u["name"].as_str().unwrap_or(""), u["state"].as_str().unwrap_or(""), u["about"].as_str().unwrap_or(""))];
    for m in u["messages"].as_array().into_iter().flatten() {
        out.push(format!("  [{}] {}", m["level"].as_str().unwrap_or(""), m["text"].as_str().unwrap_or("")));
    }
    for w in u["words"].as_array().into_iter().flatten() {
        if w["raw"] == json!(true) && w["value"] == json!("0x00000000") {
            continue;
        }
        out.push(format!("  {} {:<22} {}", w["off"].as_str().unwrap_or(""), w["name"].as_str().unwrap_or(""), w["value"].as_str().unwrap_or("(hidden)")));
        if w["fields"].as_array().is_some_and(|f| f.len() > 1) {
            for f in w["fields"].as_array().into_iter().flatten() {
                let mark = if f.get("staged").is_some() { format!(" -> {}", f["staged_text"].as_str().unwrap_or("")) } else { String::new() };
                out.push(format!("      {:<12} {}{mark}", f["name"].as_str().unwrap_or(""), f["text"].as_str().unwrap_or("")));
            }
        }
    }
    out.join("\n")
}

/// The comparison for the console: the UCBs that differ, field by field.
fn compare_text(v: &Value) -> String {
    let mut out = vec![format!(
        "compared with {}: {} UCB(s) in the files, {} differ ({} can be applied)",
        v["files"].as_array().map(|f| f.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default(),
        v["in_file"],
        v["differ"],
        v["applicable"]
    )];
    for u in v["ucbs"].as_array().into_iter().flatten().filter(|u| u["differs"].as_u64().unwrap_or(0) > 0) {
        let ro = if u["read_only_reason"].is_string() { " (read only)" } else { "" };
        out.push(format!("UCB_{}{ro}: {} field(s) differ, {} taken", u["name"].as_str().unwrap_or(""), u["differs"], u["taken"]));
        for w in u["words"].as_array().into_iter().flatten() {
            for f in w["fields"].as_array().into_iter().flatten().filter(|f| f["differs"] == json!(true) && f["kind"] != json!("auto")) {
                let mark = if f["taken"] == json!(true) { "  [taken]" } else { "" };
                out.push(format!(
                    "  {}.{}: {} -> {}{mark}",
                    w["name"].as_str().unwrap_or(""),
                    f["name"].as_str().unwrap_or(""),
                    f["dev_text"].as_str().unwrap_or(""),
                    f["file_text"].as_str().unwrap_or("")
                ));
            }
        }
    }
    out.join("\n")
}

/// `[-offset <o>] [-type <t>] <file> ...` as image files.
fn image_files(args: &[&str]) -> Result<Vec<ImageFile>, String> {
    let mut files = Vec::new();
    let (mut offset, mut kind) = (None, None);
    let mut it = args.iter();
    while let Some(&a) = it.next() {
        match a {
            "-offset" => offset = Some(crate::util::parse_u32(it.next().ok_or("-offset needs a value")?)?),
            "-type" => kind = Some(it.next().ok_or("-type needs a value")?.to_string()),
            p => files.push(ImageFile { path: p.to_string(), offset: offset.take().unwrap_or(0), kind: kind.take() }),
        }
    }
    Ok(files)
}

fn review_out(cx: &Cx, files: &[ImageFile], json_out: bool) -> CmdResult {
    let rv = uw::review(cx, files).map_err(e2s)?;
    let r = uw::readout(cx).map_err(e2s)?;
    let j = uw::review_json(&r, &rv);
    if json_out {
        return Ok(j.to_string());
    }
    if rv.changes.is_empty() {
        return Ok("no UCB changes".into());
    }
    let mut out = Vec::new();
    for c in j["changes"].as_array().into_iter().flatten() {
        out.push(format!("UCB_{} ({}):", c["ucb"].as_str().unwrap_or(""), c["risk_text"].as_str().unwrap_or("")));
        for f in c["fields"].as_array().into_iter().flatten() {
            out.push(format!("  {}: {} -> {}", f["label"].as_str().unwrap_or(""), f["from"].as_str().unwrap_or(""), f["to"].as_str().unwrap_or("")));
        }
    }
    for p in j["problems"].as_array().into_iter().flatten() {
        out.push(format!("[{}] UCB_{}: {} - {}", p["level"].as_str().unwrap_or(""), p["ucb"].as_str().unwrap_or(""), p["title"].as_str().unwrap_or(""), p["detail"].as_str().unwrap_or("")));
    }
    for n in j["notes"].as_array().into_iter().flatten() {
        out.push(format!("note: {}", n.as_str().unwrap_or("")));
    }
    out.push(format!("{} checks passed", j["passed"].as_array().map_or(0, |p| p.len())));
    if rv.blocked() {
        out.push("blocked: fix the problems above".into());
    } else {
        let confirm = if rv.risk == uw::Risk::Permanent { format!(" -confirm {}", rv.device_name) } else { String::new() };
        out.push(format!("to write: ucb write -token {}{confirm}", rv.token));
    }
    Ok(out.join("\n"))
}

pub fn cmd_ucb(cx: &Cx, a: &[&str]) -> CmdResult {
    let (args, json_out, reveal) = flags(&a[1..]);
    let sub = args.first().copied().ok_or(USAGE)?;
    let rest = &args[1..];
    match sub {
        "read" | "show" => {
            let r = if sub == "read" { uw::refresh(cx) } else { uw::readout(cx) }.map_err(e2s)?;
            let only = match rest.first() {
                Some(n) => Some(ucb::find(n).ok_or_else(|| format!("unknown UCB '{n}'"))?),
                None => None,
            };
            let v = view(cx, &r, only, reveal);
            if json_out {
                Ok(v.to_string())
            } else if only.is_some() {
                Ok(v["ucbs"].as_array().and_then(|u| u.first()).map(detail_text).unwrap_or_default())
            } else {
                Ok(text_view(&v))
            }
        }
        "set" => {
            if rest.len() < 2 {
                return Err("usage: ucb set <UCB>.<word>[.<field>] <value> | <UCB>.PW <PW0> .. <PW7>".into());
            }
            uw::stage(cx, rest[0], &rest[1..]).map_err(e2s)
        }
        "confirm" => uw::stage_confirm(cx, rest.first().ok_or("usage: ucb confirm <UCB>")?).map_err(e2s),
        "unset" => {
            let n = rest.first().ok_or("usage: ucb unset <UCB>")?;
            let i = ucb::find(n).ok_or_else(|| format!("unknown UCB '{n}'"))?;
            cx.with(|c| {
                c.ucb.staged.remove(&i);
                c.ucb.review = None;
            });
            Ok(format!("UCB_{}: staged edits discarded", UCBS[i].name))
        }
        "discard" => {
            cx.with(|c| {
                c.ucb.staged.clear();
                c.ucb.review = None;
            });
            Ok("staged UCB edits discarded".into())
        }
        "staged" => {
            let names: Vec<&str> = cx.with(|c| c.ucb.staged.keys().map(|&i| UCBS[i].name).collect());
            Ok(if json_out { json!(names).to_string() } else if names.is_empty() { "nothing staged".into() } else { names.join(" ") })
        }
        "review" => review_out(cx, &image_files(rest)?, json_out),
        "compare" => {
            if rest.first() == Some(&"-close") {
                cx.with(|c| c.ucb.compare = None);
                return Ok("compare closed".into());
            }
            let files = image_files(rest)?;
            let v = uw::compare(cx, Some(&files)).map_err(e2s)?;
            if json_out {
                return Ok(v.to_string());
            }
            Ok(compare_text(&v))
        }
        "apply" => uw::apply(cx, rest.first().ok_or("usage: ucb apply all | <UCB> | <UCB>.<word>[.<field>] | <UCB>.PW")?).map_err(e2s),
        "revert" => uw::revert(cx, rest.first().ok_or("usage: ucb revert all | <UCB> | <UCB>.<word>[.<field>] | <UCB>.PW")?).map_err(e2s),
        "restore" => {
            let f = rest.first().ok_or("usage: ucb restore <backup.hex>")?;
            cx.with(|c| c.ucb.staged.clear());
            review_out(cx, &[ImageFile { path: f.to_string(), offset: 0, kind: Some("ihex".into()) }], json_out)
        }
        "write" => {
            let mut token = None;
            let mut confirm = None;
            let mut reset = true;
            let mut it = rest.iter();
            while let Some(&x) = it.next() {
                match x {
                    "-token" => token = it.next().copied(),
                    "-confirm" => confirm = it.next().copied(),
                    "-noreset" => reset = false,
                    _ => return Err(format!("ucb write: unknown argument '{x}' (usage: ucb write -token <token> [-confirm <device>] [-noreset])")),
                }
            }
            let token = token.ok_or("usage: ucb write -token <token> [-confirm <device>] [-noreset] (the token comes from `ucb review`)")?;
            let v = uw::write(cx, &WriteOptions { token, confirm, reset }).map_err(e2s)?;
            if json_out {
                return Ok(v.to_string());
            }
            let written: Vec<String> = v["written"].as_array().into_iter().flatten().map(|w| format!("UCB_{} (UCB{}): {}", w["ucb"].as_str().unwrap_or(""), w["index"], w["method"].as_str().unwrap_or(""))).collect();
            Ok(format!("{}\nbackup: {}", if written.is_empty() { "nothing written".into() } else { written.join("\n") }, v["backup"].as_str().unwrap_or("")))
        }
        "password" => {
            let n = rest.first().ok_or("usage: ucb password <UCB> <PW0> .. <PW7> | -env <variable> | none")?;
            let i = ucb::find(n).ok_or_else(|| format!("unknown UCB '{n}'"))?;
            let d = &UCBS[i];
            let key: &'static str = if d.class == Class::Boot { "BMHD0" } else { d.name };
            if d.uc.is_none() && d.class != Class::Boot {
                return Err(format!("UCB_{} has no password", d.name));
            }
            match rest.get(1).copied() {
                None => Ok(format!("UCB_{key} password: {}", if cx.with(|c| c.ucb.passwords.contains_key(key)) { "set" } else { "not set" })),
                Some("none") => {
                    cx.with(|c| c.ucb.passwords.remove(key));
                    Ok(format!("UCB_{key} password removed"))
                }
                Some("-env") => {
                    let var = rest.get(2).ok_or("ucb password <UCB> -env <variable>")?;
                    let v = std::env::var(var).map_err(|_| format!("${var} is not set"))?;
                    let words: Vec<&str> = v.split([' ', '\t', ',', ';']).filter(|s| !s.is_empty()).collect();
                    let pw = uw::parse_password(&words)?;
                    cx.with(|c| c.ucb.passwords.insert(key, pw));
                    Ok(format!("UCB_{key} password set"))
                }
                Some(_) => {
                    let pw = uw::parse_password(&rest[1..])?;
                    cx.with(|c| c.ucb.passwords.insert(key, pw));
                    Ok(format!("UCB_{key} password set"))
                }
            }
        }
        "unlock" => uw::unlock(cx, rest.first().ok_or("usage: ucb unlock <UCB>")?).map_err(e2s),
        "lock" => uw::lock(cx).map_err(e2s),
        "export" => cmd_export(cx, rest, json_out),
        "dir" => {
            if let Some(d) = rest.first() {
                cx.with(|c| c.ucb.dir = Some(d.into()));
            }
            Ok(cx.with(|c| c.ucb.dir()).to_string_lossy().into_owned())
        }
        "status" => {
            let v = cx.with(|c| {
                json!({
                    "write_enabled": c.ucb.write_enabled,
                    "staged": c.ucb.staged.keys().map(|&i| UCBS[i].name).collect::<Vec<_>>(),
                    "review": c.ucb.review.as_ref().map(|r| r.token.clone()),
                    "dir": c.ucb.dir(),
                    "passwords": c.ucb.passwords.keys().collect::<Vec<_>>(),
                })
            });
            Ok(if json_out { v.to_string() } else { format!("UCB writing: {}\nstaged: {}\ndirectory: {}", if v["write_enabled"] == json!(true) { "enabled" } else { "off (\"ucbWrite\": true in the launch configuration)" }, v["staged"], v["dir"].as_str().unwrap_or("")) })
        }
        _ => Err(USAGE.into()),
    }
}

/// `ucb export <file>|- [-ucbs {<UCB> ...}] [-state unlocked|confirmed|errored] [-unconfirmed]
/// [-class <class> ...] [-changed] [-which orig|copy|both] [-staged]`
fn cmd_export(cx: &Cx, a: &[&str], json_out: bool) -> CmdResult {
    const USAGE: &str =
        "usage: ucb export <file> [-ucbs {<UCB> ...}] [-state unlocked|confirmed|errored] [-unconfirmed] [-class boot|protection|hsm|otp|infineon] [-changed] [-which orig|copy|both] [-staged]";
    let mut file = None;
    let mut sel = Selection::default();
    let mut it = a.iter();
    while let Some(&x) = it.next() {
        match x {
            "-ucbs" => {
                for n in it.next().ok_or(USAGE)?.split([' ', ',']).filter(|s| !s.is_empty()) {
                    sel.names.push(ucb::find(n).ok_or_else(|| format!("unknown UCB '{n}'"))?);
                }
            }
            "-state" => sel.states.push(State::parse(it.next().ok_or(USAGE)?).ok_or(USAGE)?),
            "-unconfirmed" => sel.states.push(State::Unlocked),
            "-confirmed" => sel.states.push(State::Confirmed),
            "-class" => {
                for c in it.next().ok_or(USAGE)?.split([' ', ',']).filter(|s| !s.is_empty()) {
                    sel.classes.push(Class::parse(c).ok_or_else(|| format!("unknown class '{c}'"))?);
                }
            }
            "-changed" => sel.changed_only = true,
            "-staged" => sel.staged = true,
            "-which" => {
                sel.which = match *it.next().ok_or(USAGE)? {
                    "orig" => Which::Orig,
                    "copy" => Which::Copy,
                    "both" => Which::Both,
                    _ => return Err(USAGE.into()),
                }
            }
            f if file.is_none() && (f == "-" || !f.starts_with('-')) => file = Some(f),
            _ => return Err(USAGE.into()),
        }
    }
    let file = file.ok_or(USAGE)?;
    let r = uw::readout(cx).map_err(e2s)?;
    let (staged, dir) = cx.with(|c| (c.ucb.staged.clone(), c.ucb.dir()));
    let baseline = ucb::baseline(&dir, &r);
    let ex = ucb::export(&r, &staged, baseline.as_ref(), &sel).map_err(e2s)?;
    // `-`: the HEX text comes back in the result (a frontend on another machine saves it).
    let mut v = json!({ "file": file, "exported": ex.exported, "skipped": ex.skipped, "bytes": ex.image.size(), "contains_passwords": ex.passwords });
    if file == "-" {
        v["hex"] = json!(ex.image.to_ihex());
    } else {
        std::fs::write(file, ex.image.to_ihex()).map_err(|e| format!("{file}: {e}"))?;
    }
    if json_out {
        return Ok(v.to_string());
    }
    let mut out = format!("exported {} UCB(s) to {file}: {}", ex.exported.len(), ex.exported.join(" "));
    if !ex.skipped.is_empty() {
        out += &format!("\nskipped: {}", ex.skipped.join("; "));
    }
    if ex.passwords {
        out += "\nnote: the file contains passwords";
    }
    Ok(out)
}
