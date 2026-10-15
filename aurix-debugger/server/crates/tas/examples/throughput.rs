//! Raw read throughput of the TAS transport, with nothing of aurix-dbg in the way.
//!
//! This is the floor the sampler builds on: no core thread, no scheduler, no change detection, no
//! decoding, no JSON, no WebSocket - just `TasClient::execute` in a loop. Comparing it with what
//! the DAQ endpoint delivers says how much of the cost is ours and how much is the probe.
//!
//! `tas_server.exe` cannot be taken out of the picture: it owns the USB connection to the
//! miniWiggler and this crate is a client of its socket protocol, not a USB driver. So "raw" here
//! means raw over the same TCP socket the debugger uses.
//!
//! Usage: cargo run -p tas --release --example throughput [host]
//!
//! Reads only, and only from CPU0 DSPR globals RAM, so it disturbs nothing. Stop `aurix-dbg`
//! first: the probe takes one session at a time.

use std::time::{Duration, Instant};

use tas::{Batch, ConnectMode, TasClient, DEFAULT_PORT};

/// TAS address map 15 is the access hardware itself, not the target.
const AM15: u8 = 15;
/// Access-hardware clock, in Hz. `tc37x_ocds_init.cfg` sets 30 MHz, so the raw numbers below are
/// comparable with what the debugger gets.
const AM15_ACC_HW_FREQUENCY: u64 = 0x000C_0100;
const SPEED_KHZ: u32 = 30_000;

/// CPU0 DSPR globals: mapped, readable, and not the application's stack.
const BASE: u64 = 0x7000_B400;
const OSTATE: u64 = 0xF000_0480;
const OEC: u64 = 0xF000_0478;
/// Long enough to average out USB scheduling, short enough to keep the run quick.
const SECS: f64 = 2.0;

/// Sets the access-hardware clock, exactly as `adapter speed` does.
fn set_speed(c: &mut TasClient, khz: u32) -> Result<(), Box<dyn std::error::Error>> {
    let mut b = Batch::new();
    b.write_am(AM15_ACC_HW_FREQUENCY, &khz.saturating_mul(1000).to_le_bytes(), AM15);
    let r = c.execute(&b)?;
    if !r[0].ok() {
        return Err(format!("cannot set {khz} kHz: {}", tas::pl_err_name(r[0].err)).into());
    }
    Ok(())
}

/// Runs `b` back to back for `SECS` and returns (batches/s, bytes/s). Bytes counted are the ones
/// the probe reported as actually read, so a partial read cannot inflate the rate.
fn bench(c: &mut TasClient, b: &Batch) -> (f64, f64) {
    // One pass first, so a first-touch cost does not land in the measurement.
    let _ = c.execute(b);
    let t0 = Instant::now();
    let (mut n, mut bytes) = (0u64, 0u64);
    while t0.elapsed() < Duration::from_secs_f64(SECS) {
        let r = c.execute(b).expect("batch failed");
        bytes += r.iter().map(|x| x.bytes_ok as u64).sum::<u64>();
        n += 1;
    }
    let dt = t0.elapsed().as_secs_f64();
    (n as f64 / dt, bytes as f64 / dt)
}

fn scattered(n: usize, stride: u64, len: usize) -> Batch {
    let mut b = Batch::new();
    for i in 0..n {
        b.read(BASE + i as u64 * stride, len);
    }
    b
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let host = std::env::args().nth(1).unwrap_or_else(|| "localhost".into());
    let (mut c, info) = TasClient::connect(&host, DEFAULT_PORT, "tas throughput")?;
    println!("server: {} v{}.{}", info.name, info.v_major, info.v_minor);
    let targets = c.get_targets()?;
    let t = targets.first().ok_or("no target")?.clone();
    let con = c.session_start(&t.identifier, "tas-throughput", "")?;
    println!(
        "session: {} read(s) per packet, {} B request, {} B response",
        con.pl0_max_num_rw, con.max_pl2rq_pkt_size, con.max_pl2rsp_pkt_size
    );
    c.device_connect(ConnectMode::HotAttach)?;
    set_speed(&mut c, SPEED_KHZ)?;
    println!("access hardware clock: {SPEED_KHZ} kHz (same as tc37x_ocds_init.cfg)");

    // Same as the debugger's init: enable OCDS if it is off, so the reads get through. Nothing
    // is reset and nothing is halted.
    if c.read32(OSTATE)? & 1 == 0 {
        let mut b = Batch::new();
        for v in [0xA1u32, 0x5E, 0xA1, 0x5E] {
            b.write32(OEC, v);
        }
        c.execute(&b)?;
        println!("OCDS enabled");
    }

    println!("\n1. round trip: one batch of ONE 4-byte read");
    let (r, _) = bench(&mut c, &scattered(1, 4, 4));
    let rt = 1e6 / r;
    println!("   {r:.0} batches/s -> {rt:.0} us per round trip\n");

    println!("2. scattered 4-byte reads in ONE batch (stride 2 KB, never coalescable)");
    println!("   {:>6} {:>12} {:>12} {:>14} {:>12}", "reads", "batches/s", "reads/s", "us per batch", "us per read");
    for n in [1usize, 2, 4, 8, 16, 32, 64, 128] {
        let (r, _) = bench(&mut c, &scattered(n, 2048, 4));
        let us = 1e6 / r;
        println!("   {:>6} {:>12.0} {:>12.0} {:>14.0} {:>12.1}", n, r, r * n as f64, us, (us - rt) / (n as f64 - 1.0).max(1.0));
    }

    println!("\n3. one contiguous read of N bytes");
    println!("   {:>8} {:>12} {:>14} {:>12}", "bytes", "batches/s", "us per batch", "MB/s");
    for len in [4usize, 64, 256, 1024, 4096, 16384, 65536] {
        let mut b = Batch::new();
        b.read(BASE, len);
        let (r, bps) = bench(&mut c, &b);
        println!("   {:>8} {:>12.0} {:>14.0} {:>12.2}", len, r, 1e6 / r, bps / 1e6);
    }

    println!("\n4. several 1 KB reads in one batch (the client keeps 8 packets in flight)");
    println!("   {:>6} {:>8} {:>12} {:>14} {:>12}", "reads", "bytes", "batches/s", "us per batch", "MB/s");
    for n in [1usize, 2, 4, 8, 16, 32] {
        let b = scattered(n, 1024, 1024);
        let (r, bps) = bench(&mut c, &b);
        println!("   {:>6} {:>8} {:>12.0} {:>14.0} {:>12.2}", n, n * 1024, r, 1e6 / r, bps / 1e6);
    }

    println!("\n5. what the access-hardware clock is worth");
    println!("   {:>10} {:>14} {:>10} {:>16}", "clock", "1 KB read", "MB/s", "16 words, us");
    for khz in [1_000u32, 5_000, 10_000, 20_000, 30_000] {
        if set_speed(&mut c, khz).is_err() {
            println!("   {khz:>7} kHz  not accepted");
            continue;
        }
        let mut b = Batch::new();
        b.read(BASE, 1024);
        let (r, bps) = bench(&mut c, &b);
        let (r16, _) = bench(&mut c, &scattered(16, 2048, 4));
        println!("   {:>7} kHz {:>11.0} us {:>10.2} {:>16.0}", khz, 1e6 / r, bps / 1e6, 1e6 / r16);
    }
    set_speed(&mut c, SPEED_KHZ)?;

    println!("\nreads only, CPU0 DSPR globals, hot attach: nothing was reset, halted or written");
    Ok(())
}
