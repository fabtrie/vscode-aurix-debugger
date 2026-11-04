//! Hardware prototype: talks to a BlueBox directly, with winIDEA closed.
//!
//! ```text
//! cargo run -p bluebox --example probe -- --replay <trace.jsonl> [--step "connect: begin_attach"]
//!     [--serial 01ABC] [--toggle 0|1] [--rewrite 0x70000000]
//!     [--kernel C:/winIDEA/iC6kKrnl.s] [--restart-kernel] [--no-bringup]
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
            "--map" => {
                let addr = parse_u32(&val()?)?;
                let len = parse_u32(&val()?)? as usize;
                map.push((addr, len));
            }
            _ => return Err(format!("unknown argument {a}").into()),
        }
    }

    let paths = usb::device_paths()?;
    println!("probes: {paths:?}");
    let t = Instant::now();
    let mut link = UsbLink::open(serial.as_deref())?;
    println!("opened {} in {:.1} ms", link.path(), ms(t));

    if bringup {
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
        // Target health after the attach: chip id and STM0 uptime (TIM6:TIM0 at 100 MHz).
        let mut probe = Probe::new(link, toggle.unwrap_or(false));
        let t0 = Instant::now();
        while t0.elapsed().as_secs_f64() < secs {
            let id = probe.read_memory(0xF003_6140, 4)?;
            let lo = probe.read_memory(0xF000_1010, 4)?;
            let hi = probe.read_memory(0xF000_1028, 4)?;
            let up = (u32::from_le_bytes(hi.data[..4].try_into().unwrap()) as f64 * 4294967296.0
                + u32::from_le_bytes(lo.data[..4].try_into().unwrap()) as f64)
                / 1e8;
            println!(
                "  t={:5.2}s chipid {} ok={}  uptime {:9.3}s ok={}",
                t0.elapsed().as_secs_f64(),
                hex(&id.data, 4),
                id.all_ok(),
                up,
                lo.all_ok() && hi.all_ok()
            );
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
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
