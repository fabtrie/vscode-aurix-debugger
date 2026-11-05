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
    let mut fpga_status = false;
    let mut bringup = true;
    let mut map: Vec<(u32, usize)> = Vec::new();
    let mut watch: Option<f64> = None;
    let mut script: Option<std::path::PathBuf> = None;
    let mut skip: Vec<(usize, usize)> = Vec::new();
    let mut list = false;
    let mut builtin_attach = false;
    let mut until_a = false;
    let mut reset_halt = false;
    let mut speed_test = false;
    let mut chipid_loop: usize = 0;
    let mut raw_after: Vec<u32> = Vec::new();
    let mut provoke: Option<f64> = None;
    let mut reattach_from = 0usize;
    let mut sweep: Vec<u32> = Vec::new();
    let mut batch_test: Vec<String> = Vec::new();
    let mut clock: Option<u32> = None;
    let mut flag_sets: Vec<[u8; 3]> = Vec::new();
    let mut block_sets: Vec<(usize, u8)> = Vec::new();
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
            // --fpga-status: ask for the debug FPGA's status (after the attach's first messages)
            "--fpga-status" => fpga_status = true,
            "--no-bringup" => bringup = false,
            "--watch" => watch = Some(val()?.parse()?),
            "--script" => script = Some(std::path::PathBuf::from(val()?)),
            "--list" => list = true,
            "--attach" => builtin_attach = true,
            "--until-a" => until_a = true,
            "--raw" => raw_after.push(parse_u32(&val()?)?),
            "--provoke-reset" => provoke = Some(val()?.parse()?),
            "--reattach-from" => reattach_from = val()?.parse()?,
            // --batch "r:ADDR:LEN,w:ADDR:HEX,..." (repeatable): one multi-access message
            "--batch" => batch_test.push(val()?),
            "--clock" => clock = Some(val()?.parse()?),
            // --block-set OFF=VAL: one byte of every access block (block = body without the count)
            "--block-set" => {
                let v = val()?;
                let (o, b) = v.split_once('=').ok_or("--block-set OFF=VAL")?;
                block_sets.push((usize::from_str_radix(o.trim_start_matches("0x"), 16)?, u8::from_str_radix(b.trim_start_matches("0x"), 16)?));
            }
            // --flags 000100: access flags (block offset 0x74) for the --batch messages; repeatable
            "--flags" => {
                let v = val()?;
                let b: Vec<u8> = (0..6).step_by(2).map(|i| u8::from_str_radix(&v[i..i + 2], 16).unwrap()).collect();
                flag_sets.push([b[0], b[1], b[2]]);
            }
            "--speed-sweep" => {
                for v in val()?.split(',') {
                    sweep.push(v.parse()?);
                }
            }
            "--reset-halt" => reset_halt = true,
            "--speed-test" => speed_test = true,
            "--chipid-loop" => chipid_loop = val()?.parse()?,
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
        drop(link);
        let ser = serial.clone();
        let (mut p, mut hs) = bluebox::lowlevel::open_ready(
            || reopen(ser.as_deref()).map_err(|e| bluebox::Error::NotFound(e.to_string())),
            std::time::Duration::from_secs(20),
            |m| println!("{m}"),
        )?;
        println!(
            "handshake in {:.0} ms: kernel running {} (I answer {}), board ids {:?}",
            ms(t),
            hs.kernel_running,
            hex(&hs.ident, 7),
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
        let model = p.model();
        println!("model {}", model.name());
        let kernel_file = || -> Result<bluebox::Kernel, Box<dyn std::error::Error>> {
            let path = match &kernel_path {
                Some(p) => p.clone(),
                None => bluebox::kernel::find_kernel(None, model)
                    .map_err(|e| format!("a kernel is needed: {e}; install it or pass --kernel"))?,
            };
            println!("kernel file {}", path.display());
            Ok(bluebox::Kernel::from_file(&path)?)
        };
        let load = |p: &mut Probe<UsbLink>, state: bluebox::KernelState| -> Result<(), Box<dyn std::error::Error>> {
            if state == bluebox::KernelState::Ours {
                return Ok(());
            }
            let k = kernel_file()?;
            let total = k.len();
            let t = Instant::now();
            p.ensure_kernel(state, Some(&k), |n| eprint!("\r  kernel {n}/{total}"))?;
            eprintln!();
            println!("kernel ({state:?} before) loaded ({total} bytes) and started in {:.0} ms; session set", ms(t));
            Ok(())
        };
        let state = p.kernel_state(&hs)?;
        println!("kernel: {state:?}");
        load(&mut p, state)?;
        if fpga_status && model == bluebox::Model::Ic5700 {
            let builtin = bluebox::attach::Script::tc37x();
            let (head, _) = bluebox::attach::split_at_fpga_status(&builtin);
            p.attach(&head.ok_or("the built-in attach has no FPGA status query")?)?;
            let st = p.fpga_status()?;
            let wanted = bluebox::fpga::WANTED_9_21_356;
            println!(
                "debug FPGA: type 0x{:08x}, size 0x{:x}, MD5 {}, 9.21.356's: {}",
                st.kind,
                st.size,
                st.md5_hex(),
                st.holds(&wanted)
            );
        }
        if builtin_attach {
            let t = Instant::now();
            // The iC5000 needs its debug FPGA image from the winIDEA install.
            let image = match model {
                bluebox::Model::Ic5000 => {
                    let dir = bluebox::kernel::find_kernel(None, model)?;
                    let dir = dir.parent().ok_or("kernel path")?;
                    Some(bluebox::fpga::ic5000::image_from(dir)?)
                }
                bluebox::Model::Ic5700 => None,
            };
            let c = p.connect_target(&bluebox::attach::Script::builtin(model), true, image.as_deref())?;
            for n in &c.notes {
                println!("note: {n}");
            }
            println!(
                "built-in attach ({:?}): {} exchanges in {:.0} ms, {} replies of another length",
                c.setup,
                c.sent,
                ms(t),
                c.other_len
            );
            for d in &c.other_desc {
                println!("  other length at {d}");
            }
            if reset_halt {
                let t = Instant::now();
                let st = p.reset_halt()?;
                println!("reset: cpu0 DBGSR 0x{:08x}, PC 0x{:08x}, halted {} ({:.0} ms)", st.dbgsr, st.pc, st.halted(), ms(t));
            }
            if chipid_loop > 0 && then.is_empty() {
                // SCU_CHIPID every 20 ms: when does the connection drop?
                let t = Instant::now();
                let mut last = None;
                for _ in 0..chipid_loop {
                    let ok = p.read_memory(0xF003_6140, 4)?.all_ok();
                    if last != Some(ok) {
                        println!("  {:6.0} ms: SCU_CHIPID {}", ms(t), if ok { "reads" } else { "fails" });
                        last = Some(ok);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
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
            if chipid_loop > 0 && !then.is_empty() {
                let t = Instant::now();
                let mut last = None;
                for _ in 0..chipid_loop {
                    let ok = p.read_memory(0xF003_6140, 4)?.all_ok();
                    if last != Some(ok) {
                        println!("  {:6.0} ms: SCU_CHIPID {}", ms(t), if ok { "reads" } else { "fails" });
                        last = Some(ok);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
            if let Some(hz) = clock {
                p.set_clock_for(c.setup, hz)?;
                println!("DAP clock {hz} Hz");
            }
            if speed_test {
                // 256 KiB of program flash in 8 KiB reads: the DAP clock shows in the rate.
                let t = Instant::now();
                let mut ok = true;
                let mut sum = 0xcbf2_9ce4_8422_2325u64;
                for i in 0..32u32 {
                    let m = p.read_memory(0x8000_0000 + i * 8192, 8192)?;
                    ok &= m.all_ok();
                    sum = m.data.iter().fold(sum, |h, b| (h ^ *b as u64).wrapping_mul(0x0000_0100_0000_01b3));
                }
                let s = t.elapsed().as_secs_f64();
                println!("speed test: 256 KiB in {:.0} ms, {:.0} KB/s, all ok {ok}, FNV {sum:016x}", s * 1000.0, 256.0 * 1.024 / s);
                // 500 single-word reads: their DAP time shows the clock better.
                let t = Instant::now();
                let mut ok = true;
                for i in 0..500u32 {
                    ok &= p.read_memory(0x7000_0000 + (i % 64) * 4, 4)?.all_ok();
                }
                println!("speed test: 500 word reads in {:.0} ms, all ok {ok}", ms(t));
                // A batch of 64 word reads in one message, 20 times.
                let ops: Vec<bluebox::Access> = (0..64u32).map(|i| bluebox::Access::Read { addr: 0x7000_0000 + i * 4, len: 4 }).collect();
                let t = Instant::now();
                for _ in 0..20 {
                    p.access(&ops)?;
                }
                println!("speed test: 20 batches of 64 word reads in {:.0} ms", ms(t));
            }
            let flag_list: Vec<Option<[u8; 3]>> = if flag_sets.is_empty() { vec![None] } else { flag_sets.iter().map(|f| Some(*f)).collect() };
            for flags in &flag_list {
            if let Some(f) = flags {
                println!("flags {}", hex(f, 3));
            }
            for spec in &batch_test {
                // One //2///16/ message with several access blocks: count, the blocks of
                // the single-access encoding, the 12-byte trailer (as winIDEA's 12-write message).
                let mut blocks = Vec::new();
                let mut n = 0u32;
                for item in spec.split(',') {
                    let f: Vec<&str> = item.split(':').collect();
                    let addr = parse_u32(f[1])?;
                    let one = match f[0] {
                        "r" => bluebox::memory::request(addr, f[2].parse()?, None),
                        "w" => {
                            let d: Vec<u8> = (0..f[2].len()).step_by(2).map(|i| u8::from_str_radix(&f[2][i..i + 2], 16).unwrap()).collect();
                            bluebox::memory::request(addr, d.len(), Some(&d))
                        }
                        _ => return Err(format!("bad batch item {item}").into()),
                    };
                    let body = &one[20..];
                    let mut blk = body[4..body.len() - 12].to_vec();
                    if let Some(f) = flags {
                        blk[0x74..0x77].copy_from_slice(f);
                    }
                    for &(o, b) in &block_sets {
                        blk[o] = b;
                    }
                    blocks.extend_from_slice(&blk);
                    n += 1;
                }
                let mut body = n.to_le_bytes().to_vec();
                body.extend_from_slice(&blocks);
                body.extend_from_slice(&[0u8; 12]);
                let msg = bluebox::message::encode(1, "//2///16/", &body);
                let t = Instant::now();
                let r = p.transact(&msg)?;
                println!("batch [{spec}]: {n} accesses, message {} bytes, reply {} bytes in {:.2} ms", msg.len(), r.len(), ms(t));
                // Results: count u32 at 0x1f; each: u32 0, u32 B, bitmap[B], u32 N, data[N], 24 bytes.
                let u32at = |o: usize| r.get(o..o + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()));
                let count = u32at(0x1f).unwrap_or(0);
                let total = u32at(0).unwrap_or(0) as usize + 16;
                println!("  header {}  count {count}, length field {total}", hex(&r[16..0x23.min(r.len())], 19));
                let mut o = 0x23;
                for k in 0..count {
                    let (Some(z), Some(nb)) = (u32at(o), u32at(o + 4)) else { println!("  result {k}: cut off at 0x{o:x}"); break };
                    let nb = nb as usize;
                    let bm = r.get(o + 8..o + 8 + nb).unwrap_or(&[]);
                    let n = u32at(o + 8 + nb).unwrap_or(0) as usize;
                    let d = r.get(o + 12 + nb..o + 12 + nb + n).unwrap_or(&[]);
                    let tail = r.get(o + 12 + nb + n..o + 36 + nb + n).unwrap_or(&[]);
                    println!(
                        "  result {k} @0x{o:03x}: lead {z:#x}, bitmap {} ({} bytes), data {} bytes {}, tail nonzero {}",
                        hex(bm, 8), nb, n, hex(d, 16), tail.iter().any(|b| *b != 0)
                    );
                    o += 36 + nb + n;
                }
                println!("  parsed to 0x{o:x} of {} (length field {total})", r.len());
                if count == 0 {
                    println!("  reply: {}", hex(&r, 48));
                }
            }
            }
            if !sweep.is_empty() {
                // //7//1// <Hz u32 LE>: the clock the attach sets (10 MHz there).
                let set = |p: &mut bluebox::Probe<bluebox::usb::UsbLink>, hz: u32| -> Result<Vec<u8>, Box<dyn std::error::Error>> {
                    let x = bluebox::Exchange {
                        payload: bluebox::message::encode(1, "//7//1//", &hz.to_le_bytes()),
                        fetch: bluebox::Fetch::new(0x0e, 0xf000, 2),
                        reads: vec![62480],
                        expect: Vec::new(),
                        pause_ms: 0,
                    };
                    Ok(p.exchange(&x)?.remove(0))
                };
                let timed = |p: &mut bluebox::Probe<bluebox::usb::UsbLink>| -> Result<(f64, Vec<u8>, usize), Box<dyn std::error::Error>> {
                    let t = Instant::now();
                    // Application code: programmed throughout, so every byte must read back.
                    let m = p.read_memory(0x8012_0000, 65536)?;
                    let el = ms(t);
                    let bad = m.failed.iter().filter(|f| **f).count();
                    Ok((el, m.data, bad))
                };
                let (base_ms, base, base_bad) = timed(&mut p)?;
                println!("baseline (10 MHz): 64 KiB in {base_ms:.1} ms, {base_bad} failed bytes");
                for &hz in &sweep {
                    let r = set(&mut p, hz)?;
                    let id = p.read_memory(0xF003_6140, 4)?;
                    if !id.all_ok() {
                        println!("{:>9} Hz: reply {} -> chip id read FAILED; stopping", hz, hex(&r[16.min(r.len())..], 16));
                        break;
                    }
                    let (el, data, bad) = timed(&mut p)?;
                    let stop = bad > 0 || data != base;
                    println!(
                        "{:>9} Hz: reply {}  64 KiB in {el:7.1} ms ({:.2} MB/s), {bad} failed bytes, data {}",
                        hz,
                        hex(&r[16.min(r.len())..], 16),
                        65536.0 / el / 1000.0,
                        if data == base { "same" } else { "DIFFERENT" }
                    );
                    if stop {
                        println!("stopping the sweep");
                        break;
                    }
                }
                let r = set(&mut p, 10_000_000)?;
                println!("back to 10 MHz: reply {}", hex(&r[16.min(r.len())..], 16));
            }
            if let Some(secs) = provoke {
                // This application restarts the chip when it resumes after a halt of a
                // few seconds: an external reset to test recovery with.
                const DBGSR0: u32 = 0xF881_FD00;
                const CBS_TLCHS: u32 = 0xF000_049C;
                p.write_memory(DBGSR0, &0x6u32.to_le_bytes())?;
                println!("cpu0 halt requested; waiting {secs} s");
                std::thread::sleep(std::time::Duration::from_secs_f64(secs));
                p.write_memory(DBGSR0, &0x4u32.to_le_bytes())?;
                p.write_memory(CBS_TLCHS, &0x2u32.to_le_bytes())?;
                println!("resumed");
                watch_target(&mut p, 1.0)?;
                let t = Instant::now();
                match p.core_state() {
                    Ok(st) => println!("reconnect (//2///38/ 01) in {:.1} ms: DBGSR 0x{:08x} PC 0x{:08x}", ms(t), st.dbgsr, st.pc),
                    Err(e) => println!("reconnect failed: {e}"),
                }
                watch_target(&mut p, 0.3)?;
                let t = Instant::now();
                let mut sc = bluebox::attach::Script::tc37x();
                sc.exchanges.drain(..reattach_from.min(sc.exchanges.len()));
                let rep = p.attach(&sc)?;
                println!(
                    "attach again from #{reattach_from} (same session): {} exchanges in {:.0} ms, {} replies of another length",
                    rep.sent,
                    ms(t),
                    rep.other_len.len()
                );
            }
            // Raw replies to 4-byte reads, for telling failure kinds apart.
            for &a in &raw_after {
                let r = p.transact(&bluebox::memory::request(a, 4, None))?;
                println!("  raw read 0x{a:08x}: {}", hex(&r, 80));
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
