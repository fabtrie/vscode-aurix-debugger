//! Hardware prototype: talks to a BlueBox directly, with winIDEA closed.
//!
//! ```text
//! cargo run -p bluebox --example probe -- --replay <trace.jsonl> [--step "connect: begin_attach"]
//!     [--serial 01ABC] [--toggle 0|1] [--rewrite 0x70000000]
//!     [--kernel C:/winIDEA/iC6kKrnl.s] [--restart-kernel] [--no-bringup]
//! cargo run -p bluebox --example probe -- --attach [--watch 2]   # bring-up + built-in attach
//! cargo run -p bluebox --example probe -- --script <trace.jsonl> [--skip a-b,c] [--drop path:b0:len]
//!     [--list | --export <file>] [--watch 2]                      # reduce a captured attach
//! ```
//!
//! 1. Opens the probe over WinUSB and brings the link up (`bluebox::lowlevel`):
//!    hello, handshake, and, when no kernel runs (after a power cycle), loads
//!    `iC6kKrnl.s` from `--kernel` or `C:/winIDEA` and sets the session.
//!    `--restart-kernel` first stops a running kernel, as winIDEA does before
//!    loading another version's, to try the cold path without a power cycle.
//! 2. With `--replay`, plays back the attach captured in the trace (see
//!    `bluebox::replay`). That attach writes OCDS and `STMx_OCS` registers,
//!    as winIDEA's does.
//! 3. Reads with this crate's own encoder: SCU_CHIPID, the start of program
//!    flash, erased flash, DSPR0 and a long flash block.
//! 4. With `--rewrite ADDR`, writes the 4 bytes just read at ADDR back
//!    unchanged, to exercise the write path without changing the target.
//!
//! Without `--replay` and `--toggle`, it tries both toggle values on the first read.

#[cfg(not(windows))]
fn main() {
    eprintln!("the BlueBox link needs WinUSB (Windows)");
}

#[cfg(windows)]
fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

#[cfg(windows)]
fn run() -> Result<(), Box<dyn std::error::Error>> {
    use std::time::Instant;

    use bluebox::replay::Replay;
    use bluebox::usb::{self, UsbLink};
    use bluebox::Probe;

    let mut replay_path = None;
    let mut step = "connect: begin_attach".to_string();
    let mut serial = None;
    let mut toggle: Option<bool> = None;
    let mut rewrite: Option<u32> = None;
    let mut kernel_path: Option<std::path::PathBuf> = None;
    let mut restart_kernel = false;
    let mut bringup = true;
    let mut map: Vec<(u32, usize)> = Vec::new();
    let mut watch: Option<f64> = None;
    let mut script: Option<std::path::PathBuf> = None;
    let mut skip: Vec<(usize, usize)> = Vec::new();
    let mut list = false;
    let mut builtin_attach = false;
    let mut until_a = false;
    // Captured steps to send after the attach: (trace, step, only these, skip these).
    #[allow(clippy::type_complexity)]
    let mut then: Vec<(std::path::PathBuf, String, Vec<usize>, Vec<(usize, usize)>)> = Vec::new();
    let mut drop_kinds: Vec<(String, u8, usize)> = Vec::new();
    let mut export: Option<std::path::PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().ok_or_else(|| format!("{a} needs a value"));
        match a.as_str() {
            "--replay" => replay_path = Some(std::path::PathBuf::from(val()?)),
            "--step" => step = val()?,
            "--serial" => serial = Some(val()?),
            "--toggle" => toggle = Some(val()? == "1"),
            "--rewrite" => rewrite = Some(parse_u32(&val()?)?),
            "--kernel" => kernel_path = Some(std::path::PathBuf::from(val()?)),
            "--restart-kernel" => restart_kernel = true,
            "--no-bringup" => bringup = false,
            "--watch" => watch = Some(val()?.parse()?),
            "--script" => script = Some(std::path::PathBuf::from(val()?)),
            "--list" => list = true,
            "--attach" => builtin_attach = true,
            "--until-a" => until_a = true,
            "--then" => {
                let t = val()?;
                let st = val()?;
                then.push((std::path::PathBuf::from(t), st, Vec::new(), Vec::new()));
            }
            "--then-only" => {
                let v = val()?;
                let last = then.last_mut().ok_or("--then-only after --then")?;
                for r in v.split(',') {
                    let (a, b) = r.split_once('-').unwrap_or((r, r));
                    let (a, b): (usize, usize) = (a.parse()?, b.parse()?);
                    last.2.extend(a..=b);
                }
            }
            "--then-skip" => {
                let v = val()?;
                let last = then.last_mut().ok_or("--then-skip after --then")?;
                for r in v.split(',') {
                    let (a, b) = r.split_once('-').unwrap_or((r, r));
                    last.3.push((a.parse()?, b.parse()?));
                }
            }
            "--export" => export = Some(std::path::PathBuf::from(val()?)),
            "--drop" => {
                // path:body0:len, e.g. //5///19/:00:24
                let v = val()?;
                let mut it = v.rsplitn(3, ':');
                let len = it.next().ok_or("--drop path:b0:len")?.parse()?;
                let b0 = u8::from_str_radix(it.next().ok_or("--drop path:b0:len")?, 16)?;
                let path = it.next().ok_or("--drop path:b0:len")?.to_string();
                drop_kinds.push((path, b0, len));
            }
            "--skip" => {
                for r in val()?.split(',') {
                    let (a, b) = r.split_once('-').unwrap_or((r, r));
                    skip.push((a.parse()?, b.parse()?));
                }
            }
            "--map" => {
                let addr = parse_u32(&val()?)?;
                let len = parse_u32(&val()?)? as usize;
                map.push((addr, len));
            }
            _ => return Err(format!("unknown argument {a}").into()),
        }
    }

    if let (true, Some(path)) = (list, &script) {
        for (i, x) in select_script(path, &step, until_a)?.iter().enumerate() {
            println!("{i:5} {}", label(&x.payload));
        }
        return Ok(());
    }
    if let (Some(out), Some(path)) = (&export, &script) {
        let xs = select_script(path, &step, until_a)?;
        let keep: Vec<&bluebox::Exchange> = xs
            .iter()
            .enumerate()
            .filter(|(i, x)| {
                !skip.iter().any(|&(a, b)| *i >= a && *i <= b)
                    && !bluebox::message::decode(&x.payload).is_some_and(|(_, p, body)| {
                        drop_kinds.iter().any(|(dp, b0, n)| dp == p && body.first() == Some(b0) && body.len() == *n)
                    })
            })
            .map(|(_, x)| x)
            .collect();
        let note = format!(
            "exchanges from {} (step '{step}'), selected with: {}",
            path.file_name().map(|f| f.to_string_lossy()).unwrap_or_default(),
            {
                // the selection, without the local paths of --script and --export
                let a: Vec<String> = std::env::args().skip(1).collect();
                let mut kept = Vec::new();
                let mut it = a.iter();
                while let Some(x) = it.next() {
                    if x == "--script" || x == "--export" {
                        it.next();
                    } else {
                        kept.push(x.clone());
                    }
                }
                kept.join(" ")
            }
        );
        std::fs::write(out, bluebox::attach::Script::render(&keep, &note))?;
        println!("wrote {} exchanges to {}", keep.len(), out.display());
        return Ok(());
    }

    let paths = usb::device_paths()?;
    println!("probes: {paths:?}");
    let t = Instant::now();
    let mut link = UsbLink::open(serial.as_deref())?;
    println!("opened {} in {:.1} ms", link.path(), ms(t));

    if bringup {
        let drained = bluebox::Link::drain(&mut link);
        if drained > 0 {
            println!("discarded {drained} stale bytes from the IN pipe");
        }
        let t = Instant::now();
        let mut p = Probe::start(link)?;
        let mut hs = p.handshake()?;
        println!(
            "handshake in {:.0} ms: kernel running {}, board ids {:?}",
            ms(t),
            hs.kernel_running,
            hs.board_ids()
        );
        if restart_kernel && hs.kernel_running {
            p.restart_kernel()?;
            println!("kernel stopped (////// 01)");
            drop(p);
            p = Probe::start(reopen(serial.as_deref())?)?;
            hs = p.handshake()?;
            println!("handshake again: kernel running {}", hs.kernel_running);
        }
        if !hs.kernel_running {
            let path = kernel_path
                .clone()
                .or_else(bluebox::kernel::find_in_winidea)
                .ok_or("no kernel running and no iC6kKrnl.s found; pass --kernel")?;
            let k = bluebox::Kernel::from_file(&path)?;
            let total = k.len();
            let t = Instant::now();
            p.load_kernel(&k, |n| eprint!("\r  kernel {n}/{total}"))?;
            eprintln!();
            p.set_session(&bluebox::lowlevel::SESSION_9_21_356)?;
            println!("kernel loaded from {} ({total} bytes) and started in {:.0} ms; session set", path.display(), ms(t));
        }
        if builtin_attach {
            let t = Instant::now();
            let rep = p.attach(&bluebox::attach::Script::tc37x())?;
            println!(
                "built-in attach: {} exchanges in {:.0} ms, {} replies of another length",
                rep.sent,
                ms(t),
                rep.other_len.len()
            );
            for (trace, st, then_only, then_skip) in &then {
                watch_target(&mut p, 0.1)?;
                // One captured step (e.g. "reset: reset()"), sent as exchanges.
                let mut r = Replay::from_trace(trace, st)?;
                r.disarm_breakpoints();
                let xs = r.exchanges()?;
                let t = Instant::now();
                let mut sent = 0;
                for (i, x) in xs.iter().enumerate() {
                    if (!then_only.is_empty() && !then_only.contains(&i)) || then_skip.iter().any(|&(a, b)| i >= a && i <= b) {
                        continue;
                    }
                    let got = p.exchange(x).map_err(|e| format!("then {i} ({}): {e}", label(&x.payload)))?;
                    sent += 1;
                    if list {
                        println!("  then {i:3} {:<34} -> {}", label(&x.payload), hex(got.first().map_or(&[][..], |g| &g[16.min(g.len())..]), 24));
                    }
                }
                println!("then '{st}': {sent} of {} exchanges in {:.0} ms", xs.len(), ms(t));
            }
            watch_target(&mut p, watch.unwrap_or(2.0))?;
            return Ok(());
        }
        if let Some(path) = &script {
            // The captured attach after its handshake, exchange by exchange, in
            // this session (live toggles), without the ranges in --skip.
            let xs = select_script(path, &step, until_a)?;
            if list {
                for (i, x) in xs.iter().enumerate() {
                    println!("{i:5} {}", label(&x.payload));
                }
                return Ok(());
            }
            let t = Instant::now();
            let (mut sent, mut differ) = (0, 0);
            for (i, x) in xs.iter().enumerate() {
                if skip.iter().any(|&(a, b)| i >= a && i <= b) {
                    continue;
                }
                if let Some((_, path, body)) = bluebox::message::decode(&x.payload) {
                    if drop_kinds.iter().any(|(p, b0, n)| p == path && body.first() == Some(b0) && body.len() == *n) {
                        continue;
                    }
                }
                let got = p
                    .exchange(x)
                    .map_err(|e| format!("exchange {i} ({}), fetch {:?}, reads {:?}: {e}", label(&x.payload), x.fetch, x.reads))?;
                sent += 1;
                if got.iter().map(|g| g.len()).ne(x.expect.iter().map(|e| e.len())) {
                    differ += 1;
                }
            }
            println!("script: {sent} of {} exchanges sent in {:.0} ms, {differ} replies of another length", xs.len(), ms(t));
            watch_target(&mut p, watch.unwrap_or(2.0))?;
            return Ok(());
        }
        drop(p);
        // What follows (a replay, or direct reads) starts on a fresh open.
        link = reopen(serial.as_deref())?;
    }

    if let Some(p) = &replay_path {
        let t = Instant::now();
        let mut r = Replay::from_trace(p, &step)?;
        let disarmed = r.disarm_breakpoints();
        if disarmed > 0 {
            println!("replay: {disarmed} breakpoint(s) the attach would arm are sent as cleared");
        }
        println!(
            "replay: {} transfers ({} writes, {} bytes out) from '{step}', loaded in {:.0} ms",
            r.transfers.len(),
            r.writes(),
            r.bytes_out(),
            ms(t)
        );
        let n = r.transfers.len();
        let t = Instant::now();
        let rep = r.play(&mut link, |i| {
            if i % 1000 == 0 {
                eprint!("\r  {i}/{n}");
            }
        })?;
        eprintln!("\r  {n}/{n}");
        println!(
            "replayed in {:.0} ms: {} writes, {} reads; replies identical {}, same length {}, other length {}",
            ms(t),
            rep.writes,
            rep.reads,
            rep.same,
            rep.same_len,
            rep.other_len
        );
        for d in &rep.diffs {
            let path = bluebox::message::decode(&d.request).map(|(_, p, b)| format!("{p} body {}", hex(b, 24)));
            println!("  transfer {} after {}", d.index, path.unwrap_or_else(|| hex(&d.request, 24)));
            println!("    expected {:4} bytes {}", d.expected.len(), dump(&d.expected[16.min(d.expected.len())..], 96));
            println!("    got      {:4} bytes {}", d.got.len(), dump(&d.got[16.min(d.got.len())..], 96));
        }
        if toggle.is_none() {
            toggle = r.toggle_after();
            println!("toggle after replay: {toggle:?}");
        }
    }

    if let Some(secs) = watch {
        let mut probe = Probe::new(link, toggle.unwrap_or(false));
        watch_target(&mut probe, secs)?;
        return Ok(());
    }

    // First read: settles the toggle if it is not known.
    let mut probe = Probe::new(link, toggle.unwrap_or(false));
    const CHIPID: u32 = 0xF003_6140;
    let first = match toggle {
        Some(_) => probe.read_memory(CHIPID, 4),
        None => {
            // A stale toggle loses one exchange: try once with each value.
            link_timeout(&mut probe, 1000);
            let a = probe.read_memory(CHIPID, 4);
            match a {
                Ok(m) => Ok(m),
                Err(e) => {
                    println!("toggle 0: {e}; trying toggle 1");
                    probe.set_toggle(true);
                    probe.read_memory(CHIPID, 4)
                }
            }
        }
    };
    link_timeout(&mut probe, usb::DEFAULT_TIMEOUT_MS);
    show("SCU_CHIPID", CHIPID, 4, first.map_err(Into::into));
    let raw = probe.transact(&bluebox::memory::request(CHIPID, 4, None))?;
    println!("    raw reply {} bytes: {}", raw.len(), hex(&raw, 96));

    for (label, addr, len) in [
        ("PFLASH0 start", 0x8000_0000u32, 16usize),
        ("erased flash", 0x8000_0100, 4),
        ("DSPR0", 0x7000_0000, 64),
    ] {
        let t = Instant::now();
        let r = probe.read_memory(addr, len);
        let el = ms(t);
        show(label, addr, len, r.map_err(Into::into));
        println!("    {el:.2} ms");
    }

    let t = Instant::now();
    let r = probe.read_memory(0x8000_0000, 65536)?;
    let el = ms(t);
    println!(
        "  PFLASH0 64 KiB: {} bytes, {} failed, {el:.1} ms ({:.1} MB/s)",
        r.data.len(),
        r.failed.iter().filter(|f| **f).count(),
        65536.0 / el / 1000.0
    );
    // Timing for the DAQ budget: a run of small reads.
    let t = Instant::now();
    for _ in 0..100 {
        probe.read_memory(CHIPID, 4)?;
    }
    println!("  100 x 4-byte read: {:.2} ms each", ms(t) / 100.0);

    for &(a, n) in &map {
        let r = probe.read_memory(a, n)?;
        let mut runs: Vec<(usize, usize)> = Vec::new();
        for (i, f) in r.failed.iter().enumerate() {
            if !*f { continue; }
            match runs.last_mut() { Some((s, e)) if *e == i => *e = i + 1, _ => runs.push((i, i + 1)) }
        }
        let shown: Vec<String> = runs.iter().take(12).map(|(s, e)| format!("0x{:08x}+{}", a as usize + s, e - s)).collect();
        println!("  failed in 0x{a:08x}+{n}: {} runs: {}", runs.len(), shown.join(" "));
    }

    if let Some(a) = rewrite {
        let before = probe.read_memory(a, 4)?;
        let w = probe.write_memory(a, &before.data)?;
        let after = probe.read_memory(a, 4)?;
        println!(
            "  rewrite 0x{a:08x}: wrote {} back, write ok={}, read back {} ok={}",
            hex(&before.data, 4),
            w.all_ok(),
            hex(&after.data, 4),
            after.all_ok()
        );
    }
    Ok(())
}

/// A trace's attach script (`Replay::attach_script`), cut after its first `A`
/// command with `until_a`: where the connect ends and winIDEA's own work starts.
#[cfg(windows)]
fn select_script(path: &std::path::Path, step: &str, until_a: bool) -> Result<Vec<bluebox::Exchange>, Box<dyn std::error::Error>> {
    let mut xs = bluebox::replay::Replay::from_trace(path, step)?.attach_script()?.exchanges;
    if until_a {
        if let Some(i) = xs.iter().position(|x| x.payload.starts_with(&[0x03, 0x21, b'A'])) {
            xs.truncate(i + 1);
        }
    }
    Ok(xs)
}

/// Target health: chip id and STM0 uptime (TIM6:TIM0 at 100 MHz), every 200 ms.
#[cfg(windows)]
fn watch_target(probe: &mut bluebox::Probe<bluebox::usb::UsbLink>, secs: f64) -> Result<(), Box<dyn std::error::Error>> {
    let t0 = std::time::Instant::now();
    while t0.elapsed().as_secs_f64() < secs {
        let id = probe.read_memory(0xF003_6140, 4)?;
        let lo = probe.read_memory(0xF000_1010, 4)?;
        let hi = probe.read_memory(0xF000_1028, 4)?;
        let up = (u32::from_le_bytes(hi.data[..4].try_into().unwrap()) as f64 * 4294967296.0
            + u32::from_le_bytes(lo.data[..4].try_into().unwrap()) as f64)
            / 1e8;
        // cpu0 DBGSR: a core's CSFR space, reached only once the cores are set up.
        let dbg = probe.read_memory(0xF881_FD00, 4)?;
        println!(
            "  t={:5.2}s chipid {} ok={}  uptime {:9.3}s ok={}  cpu0 DBGSR {} ok={}",
            t0.elapsed().as_secs_f64(),
            hex(&id.data, 4),
            id.all_ok(),
            up,
            lo.all_ok() && hi.all_ok(),
            hex(&dbg.data, 4),
            dbg.all_ok()
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    Ok(())
}

/// `//path body0/len` for a service message, `cmd X` for a low-level command.
#[cfg(windows)]
fn label(p: &[u8]) -> String {
    match bluebox::message::decode(p) {
        Some((k, path, body)) => format!("{path} k{k:02x} {}/{}", hex(&body[..body.len().min(4)], 4), body.len()),
        None if p.starts_with(&[0x03, 0x21]) && p.len() > 2 => format!("cmd {}", p[2] as char),
        None => hex(p, 8),
    }
}

/// Opens the probe again, retrying for a few seconds in case it is still settling.
#[cfg(windows)]
fn reopen(serial: Option<&str>) -> Result<bluebox::usb::UsbLink, Box<dyn std::error::Error>> {
    let t0 = std::time::Instant::now();
    loop {
        match bluebox::usb::UsbLink::open(serial) {
            Ok(l) => return Ok(l),
            Err(e) if t0.elapsed().as_secs() < 5 => {
                let _ = e;
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(windows)]
fn link_timeout(p: &mut bluebox::Probe<bluebox::usb::UsbLink>, ms: u32) {
    let _ = p.link_mut().set_timeout(ms);
}

#[cfg(windows)]
fn show(label: &str, addr: u32, len: usize, r: Result<bluebox::MemReply, Box<dyn std::error::Error>>) {
    match r {
        Ok(m) => {
            let bad = m.failed.iter().filter(|f| **f).count();
            let note = if bad > 0 { format!("  ({bad} of {len} bytes failed)") } else { String::new() };
            println!("  {label:<14} 0x{addr:08x}+{len}: {}{note}", hex(&m.data, 32));
        }
        Err(e) => println!("  {label:<14} 0x{addr:08x}+{len}: {e}"),
    }
}

#[cfg(windows)]
fn parse_u32(s: &str) -> Result<u32, String> {
    let r = match s.strip_prefix("0x") {
        Some(h) => u32::from_str_radix(h, 16),
        None => s.parse(),
    };
    r.map_err(|e| format!("{s}: {e}"))
}

#[cfg(windows)]
fn ms(t: std::time::Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

/// Hex, with printable ASCII runs shown as text.
#[cfg(windows)]
fn dump(b: &[u8], max: usize) -> String {
    let b = &b[..b.len().min(max)];
    let text: String = b.iter().map(|&c| if (0x20..0x7f).contains(&c) { c as char } else { '.' }).collect();
    format!("{}
             |{text}|", hex(b, max))
}

#[cfg(windows)]
fn hex(b: &[u8], max: usize) -> String {
    let mut s: String = b.iter().take(max).map(|x| format!("{x:02x}")).collect();
    if b.len() > max {
        s.push('…');
    }
    s
}
