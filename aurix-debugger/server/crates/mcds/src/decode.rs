//! Unit messages -> events: data trace (DTU), program trace (PTU) and time (TSU, ticks).
//!
//! Odd trace types are compressed: XOR with the last value of the same engine. The engines are
//! kept across paragraphs (the PTU starts paragraphs compressed; the DTU usually does not, which
//! is harmless). After a gap call [`Decoder::gap`]: compressed messages are dropped until an
//! uncompressed one restores the base.
//!
//! DTU (core IDs 9 and 11), fields LSB first after core and trace type:
//! - TT0/1 DTWD `dsize:2 data` (data of a write whose address came in a DTA)
//! - TT2/3 `sub:2`: 0 DTA `addr`, 1 DTWA `addr`, 2 DTRD `dsize:2 data`, 3 DTRA `addr`
//! - TT4/5 DTW, TT6/7 DTR: `len1:4 dsize:2 data[len1] addr`
//!
//! PTU (core ID 12): TT0/1 address, TT2/3 byte count, TT4/5 `flags:4 count` trace off; the
//! count engine is shared by TT2..TT5.
//!
//! TSU (core ID 14): TT0/1 time stamp (units of [`crate::regs::TIME_UNIT_PS`]).

use crate::frame::{Msg, CORE_DTU0, CORE_DTU1, CORE_PTU, CORE_TSU};

/// Bits of `data` per LENGTH1 code.
const LEN1: [u16; 16] = [0, 4, 8, 12, 16, 20, 24, 28, 32, 36, 40, 44, 64, 0, 0, 0];
const DSIZE: [u8; 4] = [1, 2, 4, 4];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Write,
    Read,
}

/// A data trace event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataEvent {
    /// Target time in units (ticks or TSU), `None` before the first time reference.
    pub time: Option<u64>,
    pub access: Access,
    /// `None` for a data message whose address message was lost.
    pub addr: Option<u32>,
    /// `None` for an address-only message (data tracing off).
    pub value: Option<u64>,
    /// Access size in bytes.
    pub size: u8,
    /// DTU unit (0 or 1).
    pub unit: u8,
}

/// A program trace message, decompressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtuEvent {
    /// The next instruction executed (branch target or sync point).
    Addr(u32),
    /// Bytes executed since the last discontinuity, ending with the instruction that left the
    /// sequential flow.
    Count(u32),
    /// Trace off after `count` more bytes.
    Off { count: u32, flags: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Data(DataEvent),
    Ptu { time: Option<u64>, ev: PtuEvent },
    /// Messages lost (MCDS error message).
    Lost { time: Option<u64>, unit: u8, count: u32 },
    /// A compressed message without base, an unknown message type or a damaged paragraph.
    Undecodable { time: Option<u64> },
}

#[derive(Debug, Default, Clone, Copy)]
struct Engine {
    addr: Option<u128>,
    data: Option<u128>,
}

#[derive(Debug, Default, Clone)]
pub struct Decoder {
    dtu: [Engine; 2],
    pending_addr: [Option<u32>; 2],
    ptu_addr: Option<u128>,
    ptu_cnt: Option<u128>,
    tsu: Option<u128>,
    /// Unwrapped time of the last TSU message.
    tsu_time: Option<u64>,
    tsu_epoch: u64,
    /// Ticks since the last TSU message (or since the start without one).
    ticks: u64,
    /// The decoder has seen any tick.
    ticking: bool,
    pub stats: Stats,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stats {
    pub messages: u64,
    pub undecodable: u64,
    pub lost: u64,
    pub gaps: u64,
}

fn xor(comp: bool, v: u128, base: Option<u128>) -> Option<u128> {
    if comp {
        base.map(|b| v ^ b)
    } else {
        Some(v)
    }
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Data was lost before the next messages: forget the compression bases.
    pub fn gap(&mut self) {
        self.dtu = Default::default();
        self.pending_addr = [None; 2];
        self.ptu_addr = None;
        self.ptu_cnt = None;
        self.tsu = None;
        self.stats.gaps += 1;
    }

    /// Current target time in units: the last TSU stamp plus ticks since, or ticks alone.
    pub fn now(&self) -> Option<u64> {
        match self.tsu_time {
            Some(t) => Some(t + self.ticks),
            None if self.ticking => Some(self.ticks),
            None => None,
        }
    }

    /// Feeds one message; returns the event it produces, if any.
    pub fn feed(&mut self, m: Msg) -> Option<Event> {
        match m {
            Msg::Tick(n) => {
                self.ticks += n as u64;
                self.ticking = true;
                None
            }
            Msg::Error { core, count } => {
                self.stats.lost += count as u64;
                let unit = if core == CORE_DTU1 { 1 } else { 0 };
                Some(Event::Lost { time: self.now(), unit, count: count as u32 })
            }
            Msg::Skip | Msg::End => None,
            Msg::Bad { .. } => {
                self.stats.undecodable += 1;
                Some(Event::Undecodable { time: self.now() })
            }
            Msg::Unit { core, tt, data, bits } => {
                self.stats.messages += 1;
                let r = match core {
                    CORE_DTU0 | CORE_DTU1 => self.dtu(if core == CORE_DTU1 { 1 } else { 0 }, tt, data, bits),
                    CORE_PTU => self.ptu(tt, data),
                    CORE_TSU => {
                        self.tsu_msg(tt, data);
                        Ok(None)
                    }
                    _ => Err(()),
                };
                match r {
                    Ok(e) => e,
                    Err(()) => {
                        self.stats.undecodable += 1;
                        Some(Event::Undecodable { time: self.now() })
                    }
                }
            }
        }
    }

    fn tsu_msg(&mut self, tt: u8, data: u128) {
        if tt > 1 {
            return; // TSA (absolute time base): not used
        }
        let Some(v) = xor(tt & 1 == 1, data, self.tsu) else {
            self.stats.undecodable += 1;
            return;
        };
        self.tsu = Some(v);
        let v = (v & 0xFFFF_FFFF) as u64;
        let mut t = self.tsu_epoch + v;
        if let Some(prev) = self.tsu_time {
            if t < prev && prev - t > 1 << 31 {
                self.tsu_epoch += 1 << 32;
                t += 1 << 32;
            }
        }
        self.tsu_time = Some(t);
        self.ticks = 0;
    }

    fn ptu(&mut self, tt: u8, data: u128) -> Result<Option<Event>, ()> {
        let comp = tt & 1 == 1;
        let ev = match tt {
            0 | 1 => {
                let a = xor(comp, data, self.ptu_addr).ok_or(())?;
                self.ptu_addr = Some(a);
                PtuEvent::Addr(a as u32)
            }
            2 | 3 => {
                let c = xor(comp, data, self.ptu_cnt).ok_or(())?;
                self.ptu_cnt = Some(c);
                PtuEvent::Count(c as u32)
            }
            4 | 5 => {
                let c = xor(comp, data >> 4, self.ptu_cnt).ok_or(())?;
                self.ptu_cnt = Some(c);
                PtuEvent::Off { count: c as u32, flags: (data & 0xF) as u8 }
            }
            _ => return Err(()),
        };
        Ok(Some(Event::Ptu { time: self.now(), ev }))
    }

    fn dtu(&mut self, unit: usize, tt: u8, data: u128, bits: u16) -> Result<Option<Event>, ()> {
        let comp = tt & 1 == 1;
        let time = self.now();
        let e = &mut self.dtu[unit];
        let mask = |dsize: usize| -> u128 { (1u128 << (8 * DSIZE[dsize] as u32)) - 1 };
        match tt {
            0 | 1 => {
                if bits < 2 {
                    return Err(());
                }
                let dsize = (data & 3) as usize;
                let v = xor(comp, data >> 2, e.data).ok_or(())? & mask(dsize);
                e.data = Some(v);
                let addr = self.pending_addr[unit].take();
                Ok(Some(Event::Data(DataEvent { time, access: Access::Write, addr, value: Some(v as u64), size: DSIZE[dsize], unit: unit as u8 })))
            }
            2 | 3 => {
                if bits < 2 {
                    return Err(());
                }
                let sub = data & 3;
                if sub == 2 {
                    let dsize = ((data >> 2) & 3) as usize;
                    let v = xor(comp, data >> 4, e.data).ok_or(())? & mask(dsize);
                    e.data = Some(v);
                    let addr = self.pending_addr[unit].take();
                    return Ok(Some(Event::Data(DataEvent { time, access: Access::Read, addr, value: Some(v as u64), size: DSIZE[dsize], unit: unit as u8 })));
                }
                let full = xor(comp, data >> 2, e.addr).ok_or(())?;
                e.addr = Some(full);
                let a = full as u32;
                if sub == 0 {
                    // An address announcing the following data message: no event yet.
                    self.pending_addr[unit] = Some(a);
                    Ok(None)
                } else {
                    let access = if sub == 1 { Access::Write } else { Access::Read };
                    Ok(Some(Event::Data(DataEvent { time, access, addr: Some(a), value: None, size: 0, unit: unit as u8 })))
                }
            }
            _ => {
                if bits < 6 {
                    return Err(());
                }
                let n1 = LEN1[(data & 0xF) as usize].min(bits - 6);
                let dsize = ((data >> 4) & 3) as usize;
                let raw = (data >> 6) & ((1u128 << n1) - 1);
                let v = xor(comp, raw, e.data).ok_or(())? & mask(dsize);
                let full = xor(comp, data >> (6 + n1), e.addr).ok_or(())?;
                e.data = Some(v);
                e.addr = Some(full);
                let access = if tt < 6 { Access::Write } else { Access::Read };
                Ok(Some(Event::Data(DataEvent { time, access, addr: Some(full as u32), value: Some(v as u64), size: DSIZE[dsize], unit: unit as u8 })))
            }
        }
    }
}
