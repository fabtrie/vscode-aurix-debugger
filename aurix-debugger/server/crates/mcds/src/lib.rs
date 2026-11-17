//! AURIX miniMCDS trace (TC37x production device): register setup and decoders. No I/O.
//!
//! - [`regs`]: addresses, action slots, the MCX event input table.
//! - [`config`]: a [`config::TraceConfig`] -> the register writes that set it up.
//! - [`frame`]: trace RAM paragraphs -> messages.
//! - [`decode`]: messages -> data trace writes, program trace events, time.
//! - [`flow`]: program trace events -> executed instructions (needs the code).
//! - [`buffer`]: paragraph order of a stopped recording, incremental reading of a running one.
//! - [`encode`]: builds trace RAM contents (tests, simulated target).
//!
//! Everything was learned from winIDEA's traffic with a TC377 and checked against winIDEA's
//! decoded output; see the repository's trace notes for the evidence and the open points.

pub mod buffer;
pub mod config;
pub mod decode;
pub mod encode;
pub mod flow;
pub mod frame;
pub mod regs;

pub use config::{AddrRange, Kind, Plan, Stop, Timestamps, TraceConfig};
pub use decode::{Access, DataEvent, Decoder, Event, PtuEvent};
pub use flow::{Code, Flow, GapReason, Insn, Step};

/// Converts time units to nanoseconds.
pub fn units_to_ns(units: u64) -> u64 {
    units * regs::TIME_UNIT_PS / 1000
}

/// Decodes a stopped recording's data trace (all paragraphs in order).
pub fn decode_data(paragraphs: &[(usize, &[u8])]) -> (Vec<Event>, decode::Stats) {
    let mut d = Decoder::new();
    let mut out = Vec::new();
    for (_, p) in paragraphs {
        for (_, m) in frame::messages(p) {
            if let Some(e) = d.feed(m) {
                out.push(e);
            }
        }
    }
    let stats = d.stats.clone();
    (out, stats)
}

/// Decodes a stopped recording's program trace into executed instructions.
pub fn decode_program(paragraphs: &[(usize, &[u8])], code: &mut dyn Code) -> (Vec<Step>, flow::FlowStats, decode::Stats) {
    let (steps, fs, ds, _) = decode_program_to(paragraphs, code, None);
    (steps, fs, ds)
}

/// [`decode_program`] of a recording that ended with the core halted at `halt_pc`: the straight
/// code from the last message to the halt is filled in (see [`Flow::run_to`]). The last value
/// says whether the flow reaches the halt PC.
pub fn decode_program_to(
    paragraphs: &[(usize, &[u8])],
    code: &mut dyn Code,
    halt_pc: Option<u32>,
) -> (Vec<Step>, flow::FlowStats, decode::Stats, bool) {
    let mut d = Decoder::new();
    let mut f = Flow::new();
    let mut out = Vec::new();
    for (_, p) in paragraphs {
        for (_, m) in frame::messages(p) {
            match d.feed(m) {
                Some(Event::Ptu { time, ev }) => f.feed(ev, time, code, &mut out),
                Some(Event::Lost { time, .. }) | Some(Event::Undecodable { time }) => f.gap(&mut out, time),
                _ => {}
            }
        }
    }
    let reached = match halt_pc {
        Some(pc) => f.run_to(pc, d.now(), code, &mut out),
        None => false,
    };
    let fs = f.stats;
    let ds = d.stats.clone();
    (out, fs, ds, reached)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::{dtw, paragraphs, Rec};
    use crate::frame::{CORE_DTU0, CORE_TSU};

    #[test]
    fn data_roundtrip_with_time() {
        let mut recs = vec![Rec::Unit { core: CORE_TSU, tt: 0, data: 1000, bits: 32 }];
        for i in 0..50u32 {
            let (tt, data, bits) = dtw(0x100 + i, 4, 0x7000_B52C);
            recs.push(Rec::Tick(3));
            recs.push(Rec::Unit { core: CORE_DTU0, tt, data, bits });
        }
        let mut raw = paragraphs(&recs, 0x400, 8);
        raw.resize(0x2000, 0);
        let pars = buffer::ordered(&raw, 0x1FE0, false);
        let (ev, stats) = decode_data(&pars);
        let writes: Vec<_> = ev.iter().filter_map(|e| if let Event::Data(d) = e { Some(*d) } else { None }).collect();
        assert_eq!(writes.len(), 50);
        assert_eq!(writes[0].addr, Some(0x7000_B52C));
        assert_eq!(writes[49].value, Some(0x100 + 49));
        assert_eq!(writes[0].time, Some(1003));
        assert_eq!(stats.undecodable, 0);
        assert_eq!(units_to_ns(150), 1000);
    }

    /// Local recordings (not in the repository): set MCDS_CAPTURES to their folder.
    fn captures() -> Option<std::path::PathBuf> {
        std::env::var_os("MCDS_CAPTURES").map(std::path::PathBuf::from)
    }

    /// A data trace recorded with winIDEA, when present locally.
    #[test]
    fn local_captures_if_present() {
        let Some(dir) = captures() else { return };
        let Ok(raw) = std::fs::read(dir.join("old-data-isystem_trace.raw")) else { return };
        let tram = &raw[60..];
        let pars = buffer::ordered(tram, 0x1FE0, false);
        let (ev, stats) = decode_data(&pars);
        let writes = ev.iter().filter(|e| matches!(e, Event::Data(_))).count();
        assert_eq!(writes, 327, "winIDEA decoded 327 writes from this buffer");
        assert_eq!(stats.undecodable, 0);
        if let Event::Data(d) = ev.iter().find(|e| matches!(e, Event::Data(_))).unwrap() {
            assert_eq!(d.value, Some(0x0010_000D));
        }
    }

    /// The program trace pair recorded with winIDEA (15,600 instructions in winIDEA's export),
    /// decoded with Capstone against the matching ELF. Local only: set MCDS_TEST_ELF.
    #[cfg(feature = "capstone")]
    #[test]
    fn local_program_capture_if_present() {
        use object::{Object, ObjectSegment};
        let Some(dir) = captures() else { return };
        let (Ok(raw), Ok(elf)) = (std::fs::read(dir.join("prog2-matching-elf.raw")), std::env::var("MCDS_TEST_ELF")) else { return };
        let data = std::fs::read(elf).unwrap();
        let obj = object::File::parse(&*data).unwrap();
        let segs: Vec<(u64, Vec<u8>)> = obj.segments().filter_map(|s| s.data().ok().filter(|d| !d.is_empty()).map(|d| (s.address(), d.to_vec()))).collect();
        let mut code = flow::CapstoneCode::new(|a: u32, n: usize| {
            segs.iter().find(|(b, d)| (a as u64) >= *b && (a as u64) < b + d.len() as u64).map(|(b, d)| {
                let o = (a as u64 - b) as usize;
                d[o..(o + n).min(d.len())].to_vec()
            })
        })
        .unwrap();
        let pars = buffer::ordered(&raw[60..], 0x1FE0, false);
        let (steps, fs, ds) = decode_program(&pars, &mut code);
        let n = steps.iter().filter(|s| matches!(s, Step::Insn { .. })).count();
        assert_eq!(n, 15_600, "{fs:?} {ds:?}");
        assert_eq!(fs.inconsistent, 0);
    }
}
