//! Lists the messages and decoded events of a trace RAM dump (`mcds dump`, winIDEA RawData).
//!
//!     cargo run -p mcds --example mcds_dump -- <file> [write_ptr] [ring] [max_per_paragraph]

use mcds::frame::{self, Msg};
use mcds::Decoder;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let raw = std::fs::read(&a[1]).expect("read");
    let hlen = u32::from_le_bytes(raw[12..16].try_into().unwrap()) as usize;
    let tram = &raw[hlen..];
    let wp = a.get(2).map(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).unwrap());
    let ring = a.get(3).is_some_and(|s| s == "ring");
    let max: usize = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(12);
    let pars: Vec<(usize, &[u8])> = match wp {
        Some(wp) => mcds::buffer::ordered(tram, wp, ring),
        None => (0..tram.len() / 1024).map(|i| (i, &tram[i * 1024..(i + 1) * 1024])).collect(),
    };
    let mut dec = Decoder::new();
    for (i, p) in pars {
        let msgs = frame::messages(p);
        let mut kinds = std::collections::BTreeMap::<String, usize>::new();
        for (_, m) in &msgs {
            let k = match m {
                Msg::Unit { core, tt, bits, .. } => format!("c{core}/tt{tt}/{bits}"),
                other => format!("{other:?}").split([' ', '(', '{']).next().unwrap().to_string(),
            };
            *kinds.entry(k).or_default() += 1;
        }
        println!("paragraph {i}: {kinds:?}");
        let mut shown = 0;
        for (at, m) in msgs {
            let e = dec.feed(m);
            if shown < max {
                match m {
                    Msg::Unit { core, tt, data, bits } => println!("  {at:5} c{core} tt{tt} {bits:3} {data:#x} -> {e:?}"),
                    _ => println!("  {at:5} {m:?} -> {e:?}"),
                }
                shown += 1;
            }
        }
        dec.gap();
    }
}
