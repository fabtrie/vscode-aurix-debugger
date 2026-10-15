//! M1 hardware check: connect, list targets, read CPU_ID/PC/DBGSR of all
//! three TC37x cores, halt/resume CPU0 and verify the PC stops and moves.
//!
//! Usage: cargo run -p tas --example tas_probe [host]

use std::time::{Duration, Instant};
use tas::{Batch, ConnectMode, TasClient, DEFAULT_PORT};

const CSFR: [u64; 3] = [0xF881_0000, 0xF883_0000, 0xF885_0000];
const PC: u64 = 0xFE08;
const DBGSR: u64 = 0xFD00;
const CPU_ID: u64 = 0xFE18;
const OSTATE: u64 = 0xF000_0480;
const OEC: u64 = 0xF000_0478;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let host = std::env::args().nth(1).unwrap_or_else(|| "localhost".into());
    let (mut c, info) = TasClient::connect(&host, DEFAULT_PORT, "aurix-dbg tas_probe")?;
    println!("server: {} v{}.{} ({})", info.name, info.v_major, info.v_minor, info.date);

    let targets = c.get_targets()?;
    for t in &targets {
        println!("target: {} device_type=0x{:08X} clients={}", t.identifier, t.device_type, t.num_client);
    }
    let t = targets.first().ok_or("no target")?;
    let con = c.session_start(&t.identifier, "aurix-dbg", "")?;
    println!(
        "session: max_rq={} max_rsp={} max_rw={} am_mask=0x{:04X} dev=0x{:08X}",
        con.max_pl2rq_pkt_size, con.max_pl2rsp_pkt_size, con.pl0_max_num_rw, con.pl0_addr_map_mask, con.device_type
    );
    let dt = c.device_connect(ConnectMode::HotAttach)?;
    println!("device_connect(hot attach): device_type=0x{dt:08X}");

    let ostate = c.read32(OSTATE)?;
    println!("OSTATE=0x{ostate:08X}");
    if ostate & 1 == 0 {
        let mut b = Batch::new();
        for v in [0xA1, 0x5E, 0xA1, 0x5E] {
            b.write32(OEC, v);
        }
        c.execute(&b)?;
        println!("OCDS enabled -> OSTATE=0x{:08X}", c.read32(OSTATE)?);
    }

    // One batch for all cores.
    let mut b = Batch::new();
    let hs: Vec<_> = CSFR.iter().map(|&base| (b.read32(base + CPU_ID), b.read32(base + PC), b.read32(base + DBGSR))).collect();
    let r = c.execute(&b)?;
    for (i, (id, pc, dbg)) in hs.iter().enumerate() {
        println!(
            "cpu{i}: CPU_ID=0x{:08X} PC=0x{:08X} DBGSR=0x{:08X} ({})",
            r[id.0].u32(),
            r[pc.0].u32(),
            r[dbg.0].u32(),
            if r[dbg.0].u32() & 2 != 0 { "halted" } else { "running" }
        );
    }

    // Block read timing: 64 KB of programmed PFLASH (.text of the firmware ELF).
    // Erased PFLASH pages raise a bus error on TC3xx, so avoid the BMHD area.
    let t0 = Instant::now();
    let data = c.read(0x8021_7CC0, 64 * 1024)?;
    let dt = t0.elapsed();
    println!("read 64 KiB PFLASH in {:?} ({:.0} KiB/s), first word 0x{:08X}", dt, 64.0 / dt.as_secs_f64(), u32::from_le_bytes(data[..4].try_into()?));
    // Partial read reporting: the block containing the erased area.
    let mut b = Batch::new();
    let h = b.read(0x8000_0000, 256);
    let pr = c.execute(&b)?;
    println!("read 0x80000000+256: err=0x{:02X} ({}), bytes_ok={}", pr[h.0].err, tas::pl_err_name(pr[h.0].err), pr[h.0].bytes_ok);

    let sample = |c: &mut TasClient| -> Result<Vec<u32>, tas::Error> {
        (0..5).map(|_| c.read32(CSFR[0] + PC)).collect()
    };
    let pcs = sample(&mut c)?;
    println!("cpu0 PC samples: {:08X?}", pcs);

    // Halt/resume only if the core was running; leave the target as found.
    let was_running = r[hs[0].2 .0].u32() & 2 == 0;
    if was_running {
        c.write32(CSFR[0] + DBGSR, 0x6)?;
        let pcs = sample(&mut c)?;
        let halted = c.read32(CSFR[0] + DBGSR)? & 2 != 0;
        println!("halt: DBGSR halted={halted} PC samples {:08X?} stable={}", pcs, pcs.windows(2).all(|w| w[0] == w[1]));
        std::thread::sleep(Duration::from_millis(200));
        c.write32(CSFR[0] + DBGSR, 0x4)?;
        let pcs = sample(&mut c)?;
        println!("resume: PC samples {:08X?} moving={}", pcs, pcs.windows(2).any(|w| w[0] != w[1]));
    } else {
        println!("cpu0 was halted - not touching run state");
    }
    println!("resets seen: {}", c.take_resets());
    Ok(())
}
