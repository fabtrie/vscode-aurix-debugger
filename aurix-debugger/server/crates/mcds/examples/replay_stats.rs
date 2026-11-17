//! Runs the register replay over a decoded program trace (the JSON of `trace_read` /
//! `aurix/trace read`) with the code of an ELF, without halt registers or memory, and prints
//! what it could rebuild and which instructions it does not model:
//!
//!     cargo run -p mcds --features capstone --example replay_stats -- <trace.json> <elf>

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&a[1]).expect("read the JSON")).expect("JSON");
    let data = std::fs::read(&a[2]).expect("read the ELF");
    use object::{Object, ObjectSegment};
    let obj = object::File::parse(&*data).unwrap();
    let segs: Vec<(u64, Vec<u8>)> = obj.segments().filter_map(|s| s.data().ok().filter(|d| !d.is_empty()).map(|d| (s.address(), d.to_vec()))).collect();
    let mut code = mcds::flow::CapstoneCode::new(|a: u32, n: usize| {
        segs.iter().find(|(b, d)| (a as u64) >= *b && (a as u64) < b + d.len() as u64).map(|(b, d)| {
            let o = (a as u64 - b) as usize;
            d[o..(o + n).min(d.len())].to_vec()
        })
    })
    .unwrap();
    let steps: Vec<mcds::Step> = v["Steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| match s.as_array() {
            Some(row) => mcds::Step::Insn { addr: row[0].as_u64().unwrap() as u32, time: None },
            None => mcds::Step::Gap { time: None, reason: mcds::GapReason::Lost },
        })
        .collect();
    let t = std::time::Instant::now();
    let r = mcds::replay::Replay::build(&steps, &mut code, None, &mut |_, _| None, 0);
    let ms = t.elapsed().as_millis();
    let n = r.rows.len();
    println!("{} steps -> {n} rows ({} entries) in {ms} ms, {} conflicts", steps.len(), r.stats.entries, r.stats.conflicts);
    for at in [0, n / 4, n / 2, 3 * n / 4, n - 1] {
        println!("  known after row {at}: {:.0} %", 100.0 * r.known_share(Some(at)));
    }
    if std::env::var_os("SHOW_ENTRIES").is_some() {
        let mut shown = 0;
        for (i, row) in r.rows.iter().enumerate() {
            if let mcds::replay::Row::Entry { vector, .. } = row {
                if let Some(mcds::replay::Row::Insn { addr, d, .. }) = r.rows.get(i - 1) {
                    println!("  entry after {addr:08x} {} {} -> {vector:08x}", d.mnemonic, d.ops);
                    shown += 1;
                }
            }
            if shown > 25 {
                break;
            }
        }
    }
    let total: u32 = r.stats.unmodelled.iter().map(|x| x.1).sum();
    println!("unmodelled: {total} executed ({:.2} %): {:?}", 100.0 * total as f64 / n as f64, r.stats.unmodelled);
}
