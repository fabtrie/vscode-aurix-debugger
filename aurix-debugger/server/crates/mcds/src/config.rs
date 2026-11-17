//! Trace configuration -> the register writes that set it up.
//!
//! The write sequence is winIDEA's (same registers, same order); only the values that a setting
//! changes differ. Every setting maps onto something observed on hardware:
//!
//! | Setting | Registers |
//! |---|---|
//! | data trace of a range | DTU comparator 0 (X, len-1), event with `dtu_ea_trig_0`, `dtu_wadr` + `dtu_wdat` while it |
//! | program trace | `ptu_enable` always, or while an event with `ptu_trig_0` (PTU comparator 0) |
//! | stop when full | `trace_done` always, linear buffer |
//! | stop on halt | ring buffer, no `trace_done`: the debugger stops it when it sees the halt, after a
//! |   | [`Plan::flush`] (measured: a `trace_done` stop leaves the last trace RAM word, the last few
//! |   | hundred nanoseconds of program flow, inside the unit for good) |
//! | stop manually | `trace_done` never, ring buffer |
//! | stop at a function | `trace_done` on an event with `ptu_trig_1` (comparator 1), ring buffer |
//! | time stamps | ticks: `tick_enable` always; messages: `tsu_rel_en` always, `tsu_rel_sync` on the data
//! |   | comparator event (data trace: one stamp per write) or on `tsu_tc_trig` while the core is not
//! |   | suspended (program trace) |
//!
//! `tsu_tc_trig` fires every few TSU units: with nothing else to trace (a halted core, a data
//! trace between writes) its stamps fill the 8 KB in about a millisecond, so it is never left
//! ungated.

use crate::regs::{self, Action, Input, When};

/// An address range: `len` bytes from `start` (`len` >= 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddrRange {
    pub start: u32,
    pub len: u32,
}

impl AddrRange {
    pub fn new(start: u32, len: u32) -> Result<Self, String> {
        if len == 0 {
            return Err("empty address range".into());
        }
        if start.checked_add(len - 1).is_none() {
            return Err(format!("range {start:#x} + {len:#x} passes the end of the address space"));
        }
        Ok(Self { start, len })
    }
    pub fn end(&self) -> u32 {
        self.start + (self.len - 1)
    }
    pub fn contains(&self, a: u32) -> bool {
        a >= self.start && a <= self.end()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// Every write into the range (DTU).
    Data { range: AddrRange },
    /// Program flow (PTU), optionally only while the PC is inside `filter`.
    Program { filter: Option<AddrRange> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The recorder stops when the 8 KB are full (first 8 KB after the start).
    BufferFull,
    /// Ring buffer, stopped in hardware when the core halts (last 8 KB before the halt).
    Halt,
    /// Ring buffer, stopped by the host (last 8 KB before the stop; streaming data trace).
    Manual,
    /// Ring buffer, stopped when the PC enters the range (PTU comparator 1).
    Function(AddrRange),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timestamps {
    /// A tick message per time step: exact durations, but fills the buffer in milliseconds.
    Ticks,
    /// TSU time stamps only along with other messages.
    Messages,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceConfig {
    /// Observed core (POB). Only CPU0 is known.
    pub core: u8,
    pub kind: Kind,
    pub stop: Stop,
    pub timestamps: Timestamps,
}

/// The setup for a [`TraceConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// Register writes (32-bit), in order, starting with the recorder reset.
    pub writes: Vec<(u32, u32)>,
    /// Value for [`regs::CTRL`] that starts the recording (written after `writes` and after the
    /// trace RAM was cleared).
    pub start: u32,
    /// Ring buffer (stopped by an event or the host) rather than linear (stops when full).
    pub ring: bool,
    /// Writes that push the last, partly filled trace RAM word out of the unit (it writes the RAM
    /// in 32-byte words): a few forced time stamps (or a burst of ticks). Made before a host stop;
    /// without it the last messages (a program trace's last few hundred nanoseconds, a data
    /// trace's last writes) never reach the trace RAM.
    pub flush: Vec<(u32, u32)>,
    pub resources: Resources,
}

/// Rising edges forced on the time stamp event by [`Plan::flush`]: one stamp each, measured: 16
/// moved the write pointer by three words.
pub const FLUSH_EDGES: usize = 16;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resources {
    pub ptu_comparators: u8,
    pub dtu_comparators: u8,
    /// Events used, with the input selected in each.
    pub events: Vec<(u8, &'static str)>,
}

/// Events the allocator prefers for an input (verified on hardware first).
fn preferred_events(input: Input) -> &'static [u8] {
    match input {
        Input::PtuTrig0 => &[0, 2, 5, 7, 8, 9, 10, 11, 12, 13],
        Input::PtuTrig1 => &[1, 3, 7, 8, 9, 10, 11, 12, 13],
        Input::DtuEaTrig0 => &[7, 8, 9, 10, 11, 12, 13],
        Input::DtuEaTrig1 => &[7, 8, 9, 10, 11, 12, 13],
        Input::DtuAccTrig0 => &[8, 9, 10, 13],
        Input::DcuHalt => &[4, 1, 5],
        Input::DcuSus => &[3, 2],
        Input::BreakIn => &[14, 15, 0, 1, 2, 3, 4, 6, 7, 8, 10, 11, 12, 13],
        Input::TsuTcTrig => &[0, 1, 2, 3, 14, 15],
    }
}

#[derive(Default)]
struct Events {
    /// Events in use with their terms.
    used: Vec<(u8, Vec<Term>)>,
}

/// One input of an event: block, input, required true (or false).
type Term = (u8, Input, bool);

impl Events {
    /// An event with no input, never true (EVT value 0): forced true by the debugger only.
    fn alloc_never(&mut self) -> Result<u8, String> {
        for e in [15u8, 14, 13, 12, 11, 10, 9, 8, 6, 5, 4, 3, 2, 1, 0] {
            if !self.used.iter().any(|u| u.0 == e) {
                self.used.push((e, Vec::new()));
                return Ok(e);
            }
        }
        Err("no MCX event left for the flush".into())
    }

    fn alloc(&mut self, input: Input) -> Result<u8, String> {
        self.alloc_all(&[(input, true)])
    }

    /// An event whose inputs all hold: `(input, true)` requires the input, `(input, false)` its
    /// absence. The events the first input prefers are tried first.
    fn alloc_all(&mut self, terms: &[(Input, bool)]) -> Result<u8, String> {
        let preferred = preferred_events(terms[0].0);
        let mut order: Vec<u8> = preferred.to_vec();
        order.extend((0..regs::EVT_COUNT as u8).filter(|e| !preferred.contains(e)));
        for e in order {
            if self.used.iter().any(|u| u.0 == e) {
                continue;
            }
            let blocks: Option<Vec<Term>> = terms.iter().map(|&(i, on)| regs::block_of(e, i).map(|b| (b, i, on))).collect();
            if let Some(blocks) = blocks {
                self.used.push((e, blocks));
                return Ok(e);
            }
        }
        let names: Vec<String> = terms.iter().map(|(i, on)| format!("{}{}", if *on { "" } else { "not " }, i.name())).collect();
        Err(format!("no MCX event left with {}", names.join(" and ")))
    }
}

impl TraceConfig {
    pub fn plan(&self) -> Result<Plan, String> {
        if self.core != 0 {
            return Err(format!(
                "only CPU0 can be traced so far (the observation block's core selection is known for CPU0 only), not CPU{}",
                self.core
            ));
        }
        let mut ev = Events::default();
        let mut act = [When::Never; regs::ACT_COUNT];
        let mut dtu = [(0u32, regs::DTU_CMP_UNUSED); 2];
        let mut ptu = [(0u32, regs::PTU_CMP_UNUSED); 2];
        let mut res = Resources::default();

        let mut data_event = None;
        match &self.kind {
            Kind::Data { range } => {
                let e = ev.alloc(Input::DtuEaTrig0)?;
                data_event = Some(e);
                dtu[0] = (range.start, range.len - 1);
                res.dtu_comparators = 1;
                act[Action::DtuWadr.index()] = When::While(e);
                act[Action::DtuWdat.index()] = When::While(e);
            }
            Kind::Program { filter } => match filter {
                None => act[Action::PtuEnable.index()] = When::Always,
                Some(r) => {
                    let e = ev.alloc(Input::PtuTrig0)?;
                    ptu[0] = (r.start, r.len - 1);
                    res.ptu_comparators += 1;
                    act[Action::PtuEnable.index()] = When::While(e);
                }
            },
        }

        let ring = match self.stop {
            Stop::BufferFull => {
                act[Action::TraceDone.index()] = When::Always;
                false
            }
            // Stopped by the debugger (see the module documentation).
            Stop::Halt => true,
            Stop::Manual => true,
            Stop::Function(r) => {
                let e = ev.alloc(Input::PtuTrig1)?;
                ptu[1] = (r.start, r.len - 1);
                res.ptu_comparators += 1;
                act[Action::TraceDone.index()] = When::On(e);
                true
            }
        };

        let mut flush: Vec<(u32, u32)> = Vec::new();
        let mut stamp_event = None;
        let start = match self.timestamps {
            Timestamps::Ticks => {
                if matches!(self.kind, Kind::Program { .. }) {
                    // Ticks only while the core runs, else they overwrite the ring during a halt.
                    // A burst of ticks does not flush (idle ticks pack into 12-bit multi-tick
                    // messages): forced time stamps on an event that is otherwise never true do.
                    let e = ev.alloc_all(&[(Input::DcuSus, false)])?;
                    act[Action::TickEnable.index()] = When::While(e);
                    let f = ev.alloc_never()?;
                    act[Action::TsuRelEn.index()] = When::Always;
                    act[Action::TsuRelSync.index()] = When::On(f);
                    stamp_event = Some(f);
                } else {
                    act[Action::TickEnable.index()] = When::Always;
                }
                regs::CTRL_START_TICK
            }
            Timestamps::Messages => {
                act[Action::TsuRelEn.index()] = When::Always;
                let sync = match (&self.kind, data_event) {
                    // A stamp per write: the data comparator, qualified so that most reads of the
                    // range do not stamp (the unqualified comparator stamps every access).
                    (Kind::Data { .. }, Some(_)) => ev.alloc_all(&[(Input::DtuEaTrig0, true), (Input::DtuAccTrig0, true)])?,
                    // Periodic stamps while the core runs; none while it is halted.
                    _ => ev.alloc_all(&[(Input::TsuTcTrig, true), (Input::DcuSus, false)])?,
                };
                act[Action::TsuRelSync.index()] = When::On(sync);
                stamp_event = Some(sync);
                regs::CTRL_START_TSU
            }
        };

        let mut evt = [0u32; regs::EVT_COUNT];
        for (e, terms) in &ev.used {
            if terms.is_empty() {
                // alloc_never: no input selected would be "always"; 0 (every block both true and
                // false) is never.
                evt[*e as usize] = 0;
                res.events.push((*e, "flush"));
                continue;
            }
            let t: Vec<(u8, bool)> = terms.iter().map(|&(b, _, on)| (b, on)).collect();
            evt[*e as usize] = regs::event_value_of(&t);
            let names: Vec<&'static str> = terms.iter().map(|&(_, i, on)| if on { i.name() } else { not_name(i) }).collect();
            res.events.push((*e, if names.len() == 1 { names[0] } else { combo_name(&names) }));
        }
        res.events.sort();
        if let Some(e) = stamp_event {
            // The stamp event forced true and back: one rising edge, one time stamp each.
            let a = regs::EVT + 4 * e as u32;
            for _ in 0..FLUSH_EDGES {
                flush.push((a, 0xFFFF_FFFF));
                flush.push((a, 0));
            }
            flush.push((a, evt[e as usize]));
        }

        let mut w: Vec<(u32, u32)> = Vec::with_capacity(130);
        w.push((regs::CTRL, regs::CTRL_CONFIG));
        for (i, a) in act.iter().enumerate() {
            w.push((regs::ACT + 4 * i as u32, a.value()));
        }
        for (i, v) in evt.iter().enumerate() {
            w.push((regs::EVT + 4 * i as u32, *v));
        }
        for a in (regs::ZERO_BLOCK_A00.start + 4..regs::ZERO_BLOCK_A00.end).step_by(0x10) {
            w.push((a, 0));
        }
        for a in (regs::ZERO_BLOCK_A00.start..regs::ZERO_BLOCK_A00.end).step_by(0x10) {
            w.push((a, 0));
        }
        w.push((regs::PTU_CMP[0], ptu[0].0));
        w.push((regs::PTU_CMP[1], ptu[1].0));
        w.push((regs::PTU_CMP[0] + 4, ptu[0].1));
        w.push((regs::PTU_CMP[1] + 4, ptu[1].1));
        w.push((regs::DTU_CMP[0], dtu[0].0));
        w.push((regs::DTU_CMP[1], dtu[1].0));
        w.push((regs::DTU_CMP[0] + 4, dtu[0].1));
        w.push((regs::DTU_CMP[1] + 4, dtu[1].1));
        w.extend_from_slice(&regs::CONSTANTS[..8]);
        w.push((regs::PTU_CMP[0], ptu[0].0));
        w.push((regs::PTU_CMP[1], ptu[1].0));
        w.extend_from_slice(&regs::CONSTANTS[8..]);
        w.push((regs::FIFO_BOTTOM, 0));
        w.push((regs::FIFO_WRAP, if ring { regs::TRAM_SIZE - 1 } else { 0 }));
        w.push((regs::FIFO_END, regs::TRAM_SIZE - 1));
        w.push(regs::REG_404);
        Ok(Plan { writes: w, start, ring, flush, resources: res })
    }
}

/// Writes that switch the MCDS on after a reset, given the current CLC and 0x010 values
/// (winIDEA's first session): clock on, 0x010 |= 0x8000 (0x00A0 -> 0x80A0, reads back 0x8020A0),
/// Cerberus 0x47C = 0x300.
pub fn init_writes(clc: u32, cfg10: u32) -> Vec<(u32, u32)> {
    let mut w = Vec::new();
    if clc != 0 {
        w.push((regs::CLC, 0));
    }
    // Written 0x80A0, it reads back 0x8020A0: bit 15 shows up as bit 23.
    if cfg10 & 0x0080_8000 == 0 {
        w.push((regs::CFG10, cfg10 | 0x8000));
    }
    w.push((regs::CBS_47C, regs::CBS_47C_VALUE));
    w
}

/// Writes that stop a recording: winIDEA's pulse on three action slots, then the stop value
/// (`done`: the recorder had finished by itself).
pub fn stop_writes(done: bool) -> Vec<(u32, u32)> {
    let pulse = regs::STOP_PULSE.iter().map(|&a| (a, 0x60)).chain(regs::STOP_PULSE.iter().map(|&a| (a, 0)));
    if done {
        let mut w: Vec<(u32, u32)> = pulse.collect();
        w.push((regs::CTRL, regs::CTRL_STOP_DONE));
        w
    } else {
        // Stop first: on a running recorder the pulse (tsu_rel_sync always) floods the trace RAM
        // with time stamps within microseconds (measured). After the stop it writes nothing.
        let mut w = vec![(regs::CTRL, regs::CTRL_STOP_HOST)];
        w.extend(pulse);
        w
    }
}

/// Display names of inverted inputs (`Resources::events` holds `&'static str`).
fn not_name(i: Input) -> &'static str {
    match i {
        Input::PtuTrig0 => "not ptu_trig_0",
        Input::PtuTrig1 => "not ptu_trig_1",
        Input::DtuEaTrig0 => "not dtu_ea_trig_0",
        Input::DtuEaTrig1 => "not dtu_ea_trig_1",
        Input::DtuAccTrig0 => "not dtu_acc_trig_0",
        Input::DcuHalt => "not dcu_halt",
        Input::DcuSus => "not dcu_sus",
        Input::BreakIn => "not break_in",
        Input::TsuTcTrig => "not tsu_tc_trig",
    }
}

fn combo_name(names: &[&str]) -> &'static str {
    match names {
        ["tsu_tc_trig", "not dcu_sus"] => "tsu_tc_trig and not dcu_sus",
        ["dtu_ea_trig_0", "dtu_acc_trig_0"] => "dtu_ea_trig_0 and dtu_acc_trig_0",
        _ => "combination",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// winIDEA's data trace session on a TC377 (DAP capture `dap-mcds-winidea-data1`), without the
    /// trace RAM clearing and the start.
    const WINIDEA_DATA1: &str = "fb718210=00008000 fb718880=0 fb718884=0 fb718888=0 fb71888c=0 fb718890=0 fb718894=0 fb718898=0 fb71889c=0 fb7188a0=0 fb7188a4=0 fb7188a8=0 fb7188ac=0 fb7188b0=0 fb7188b4=0 fb7188b8=0 fb7188bc=0 fb7188c0=0 fb7188c4=0 fb7188c8=0 fb7188cc=0 fb7188d0=0 fb7188d4=60 fb7188d8=60 fb7188dc=0 fb7188e0=0 fb7188e4=0 fb7188e8=0 fb7188ec=c7 fb7188f0=c7 fb7188f4=0 fb7188f8=0 fb7188fc=0 fb718900=0 fb718904=0 fb718908=0 fb71890c=0 fb718910=0 fb718914=0 fb718918=0 fb71891c=0 fb718920=0 fb718800=0 fb718804=0 fb718808=0 fb71880c=0 fb718810=0 fb718814=0 fb718818=0 fb71881c=ffffffef fb718820=0 fb718824=0 fb718828=0 fb71882c=0 fb718830=0 fb718834=0 fb718838=0 fb71883c=0 fb718840=0 fb718844=0 fb718848=0 fb71884c=0 fb718850=0 fb718854=0 fb718858=0 fb71885c=0 fb718a04=0 fb718a14=0 fb718a24=0 fb718a34=0 fb718a44=0 fb718a54=0 fb718a64=0 fb718a74=0 fb718a00=0 fb718a10=0 fb718a20=0 fb718a30=0 fb718a40=0 fb718a50=0 fb718a60=0 fb718a70=0 fb71b000=0 fb71b010=0 fb71b004=fffffffe fb71b014=fffffffe fb71a400=7000b52c fb71a410=0 fb71a404=3 fb71a414=ffffffff fb71a480=0 fb71a4a0=0 fb71a490=ffffffff fb71a4b0=ffffffff fb71a488=0 fb71a4a8=0 fb71a49c=0 fb71a4bc=0 fb71b000=0 fb71b010=0 fb71a00c=0 fb71a500=0 fb71a518=0 fb71a508=3001 fb71a520=3001 fb71a504=0 fb71a51c=0 fb718020=08008000 fb718014=81 fb718204=0 fb718208=0 fb71820c=1fff fb718404=1";

    fn parse(s: &str) -> Vec<(u32, u32)> {
        s.split_whitespace()
            .map(|kv| {
                let (a, v) = kv.split_once('=').unwrap();
                (u32::from_str_radix(a, 16).unwrap(), u32::from_str_radix(v, 16).unwrap())
            })
            .collect()
    }

    fn get(p: &Plan, a: u32) -> u32 {
        p.writes.iter().rev().find(|w| w.0 == a).unwrap().1
    }

    #[test]
    fn data_trace_matches_winidea() {
        let cfg = TraceConfig {
            core: 0,
            kind: Kind::Data { range: AddrRange::new(0x7000_B52C, 4).unwrap() },
            stop: Stop::BufferFull,
            timestamps: Timestamps::Ticks,
        };
        let p = cfg.plan().unwrap();
        assert_eq!(p.writes, parse(WINIDEA_DATA1));
        assert_eq!(p.start, 0x6003);
        assert!(!p.ring);
    }

    #[test]
    fn program_filter_matches_winidea() {
        // The working function filter (capture dap-mcds-winidea-filter): EVT0 = 0xFFFFFFBF,
        // ptu_enable = 0xC0, PTU comparator 0 = function start / size - 1.
        let cfg = TraceConfig {
            core: 0,
            kind: Kind::Program { filter: Some(AddrRange::new(0x8019_D3B0, 0x4EE).unwrap()) },
            stop: Stop::BufferFull,
            timestamps: Timestamps::Ticks,
        };
        let p = cfg.plan().unwrap();
        assert_eq!(get(&p, 0xFB71_8800), 0xFFFF_FFBF);
        assert_eq!(get(&p, 0xFB71_88FC), 0xC0);
        assert_eq!(get(&p, 0xFB71_B000), 0x8019_D3B0);
        assert_eq!(get(&p, 0xFB71_B004), 0x4ED);
        assert_eq!(get(&p, 0xFB71_88D8), 0x60);
        // winIDEA: tick_enable always (0x60). Here ticks only while the core is not suspended
        // (else a halt fills the buffer with them): while EVT3 = not dcu_sus.
        assert_eq!(get(&p, 0xFB71_88D4), 0xC3);
        assert_eq!(get(&p, 0xFB71_880C), 0xFFFF_7FFF);
        assert_eq!(get(&p, 0xFB71_8208), 0);
    }

    #[test]
    fn stop_on_halt() {
        // winIDEA (capture dap-mcds-winidea-stophalt, run S2) used trace_done = 0x44 on EVT4 =
        // dcu_halt. Measured with this debugger on a TC377: dcu_halt misses a halt over trigger
        // line 1 (debugger halt, another core's breakpoint); dcu_sus (EVT3 block 7) sees all.
        // Then: a trace_done stop leaves the last trace RAM word in the unit, so the debugger
        // stops it after a flush; ticks only while the core is not suspended.
        let cfg = TraceConfig { core: 0, kind: Kind::Program { filter: None }, stop: Stop::Halt, timestamps: Timestamps::Ticks };
        let p = cfg.plan().unwrap();
        assert_eq!(get(&p, 0xFB71_88D8), 0);
        assert_eq!(get(&p, 0xFB71_88D4), 0xC3);
        assert_eq!(get(&p, 0xFB71_880C), 0xFFFF_7FFF);
        // Flush: forced stamps on EVT15, which is otherwise never true.
        assert_eq!(get(&p, 0xFB71_8884), 0x4F);
        assert_eq!(get(&p, 0xFB71_883C), 0);
        assert_eq!(p.flush[0], (0xFB71_883C, 0xFFFF_FFFF));
        assert_eq!(p.flush.last(), Some(&(0xFB71_883C, 0)));
        assert_eq!(get(&p, 0xFB71_88FC), 0x60);
        assert_eq!(get(&p, 0xFB71_8208), 0x1FFF);
        assert!(p.ring);
    }

    #[test]
    fn tsu_time_stamps_gated() {
        // winIDEA's program trace wizard: tsu_rel_en always, tsu_rel_sync on tsu_tc_trig. Here
        // the trigger only counts while the core is not suspended (EVT2: tsu_tc_trig and not
        // dcu_sus), else its stamps fill the trace RAM within a millisecond of a halt.
        let cfg = TraceConfig { core: 0, kind: Kind::Program { filter: None }, stop: Stop::BufferFull, timestamps: Timestamps::Messages };
        let p = cfg.plan().unwrap();
        assert_eq!(get(&p, 0xFB71_8880), 0x60);
        assert_eq!(get(&p, 0xFB71_8884), 0x42);
        assert_eq!(get(&p, 0xFB71_8808), 0xFFEF_7FFF);
        assert_eq!(p.start, 0x6002);
        assert_eq!(p.flush.len(), 2 * FLUSH_EDGES + 1);
        assert_eq!(p.flush[0], (0xFB71_8808, 0xFFFF_FFFF));
        assert_eq!(p.flush.last(), Some(&(0xFB71_8808, 0xFFEF_7FFF)));
    }

    #[test]
    fn data_trace_stamps_each_write() {
        // Measured: one stamp per write (tsu_rel_sync on the comparator event) keeps the trace RAM
        // for the writes; on tsu_tc_trig it laps every few milliseconds with nothing but stamps.
        let cfg = TraceConfig { core: 0, kind: Kind::Data { range: AddrRange::new(0x7000_B52C, 4).unwrap() }, stop: Stop::Manual, timestamps: Timestamps::Messages };
        let p = cfg.plan().unwrap();
        assert_eq!(get(&p, 0xFB71_8884), 0x48);
        assert_eq!(get(&p, 0xFB71_8820), 0xFFFF_EFEF);
        assert_eq!(get(&p, 0xFB71_88EC), 0xC7);
        assert_eq!(p.resources.events, vec![(7, "dtu_ea_trig_0"), (8, "dtu_ea_trig_0 and dtu_acc_trig_0")]);
    }

    #[test]
    fn host_stop_comes_first() {
        assert_eq!(stop_writes(false)[0], (regs::CTRL, regs::CTRL_STOP_HOST));
        assert_eq!(stop_writes(true).last(), Some(&(regs::CTRL, regs::CTRL_STOP_DONE)));
    }

    #[test]
    fn filter_and_tsu_use_different_events() {
        let cfg = TraceConfig {
            core: 0,
            kind: Kind::Program { filter: Some(AddrRange::new(0x8000_0000, 0x100).unwrap()) },
            stop: Stop::Halt,
            timestamps: Timestamps::Messages,
        };
        let p = cfg.plan().unwrap();
        assert_eq!(p.resources.events, vec![(0, "ptu_trig_0"), (2, "tsu_tc_trig and not dcu_sus")]);
        assert_eq!(get(&p, 0xFB71_8884), 0x42);
        assert_eq!(get(&p, 0xFB71_88D8), 0);
    }

    #[test]
    fn other_cores_are_refused() {
        let cfg = TraceConfig { core: 1, kind: Kind::Program { filter: None }, stop: Stop::Halt, timestamps: Timestamps::Ticks };
        assert!(cfg.plan().unwrap_err().contains("CPU0"));
    }

    #[test]
    fn stop_sequence() {
        assert_eq!(
            stop_writes(true),
            vec![(0xFB71_8880, 0x60), (0xFB71_8884, 0x60), (0xFB71_8904, 0x60), (0xFB71_8880, 0), (0xFB71_8884, 0), (0xFB71_8904, 0), (0xFB71_8210, 0xA003)]
        );
        assert_eq!(init_writes(3, 0xA0), vec![(0xFB71_8000, 0), (0xFB71_8010, 0x80A0), (0xF000_047C, 0x300)]);
        assert_eq!(init_writes(0, 0x8020A0), vec![(0xF000_047C, 0x300)]);
    }
}
