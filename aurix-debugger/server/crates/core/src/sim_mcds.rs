//! Simulated miniMCDS: registers, trace RAM and a recorder that behaves like the chip's in the
//! ways the debugger relies on.
//!
//! - CLC reads 3 after a reset; every other MCDS register refuses access until it is 0.
//! - `CTRL = 0x8000` resets the recorder, `0x600x` starts it, `0xA003`/`0x8003` stop it.
//! - Data trace (`dtu_wadr` active): the debugger's own stores into the DTU comparator range are
//!   recorded as write messages with TSU time stamps, so streaming can be tested end to end.
//! - Program trace (`ptu_enable` active): a synthetic flow is written at start - all 8 KB and
//!   done when the buffer stops when full, one and a half laps of the ring otherwise. A ring
//!   stopped by `dcu_sus` (or `dcu_halt`) finishes when the observed core ([`regs::POB_SEL`],
//!   CPU0 until written) halts.

use std::collections::HashMap;
use std::time::Instant;

use mcds::encode::ParagraphWriter;
use mcds::frame::{CORE_DTU0, CORE_PTU, CORE_TSU};
use mcds::regs::{self, Action};

const MCDS_RANGE: std::ops::Range<u32> = 0xFB71_8000..0xFB71_C000;

pub struct SimMcds {
    clc: u32,
    regs: HashMap<u32, u32>,
    tram: Vec<u8>,
    ctrl_low: u32,
    running: bool,
    done: bool,
    fifonow: u32,
    ring: bool,
    stop_on_halt: bool,
    data: Option<(u32, u32)>,
    writer: ParagraphWriter,
    par: usize,
    t0: Instant,
    /// Stores recorded, for tests.
    pub recorded: u64,
}

impl Default for SimMcds {
    fn default() -> Self {
        Self::new()
    }
}

impl SimMcds {
    pub fn new() -> Self {
        Self {
            clc: 3,
            regs: HashMap::new(),
            tram: vec![0; regs::TRAM_SIZE as usize],
            ctrl_low: 0,
            running: false,
            done: false,
            fifonow: 0,
            ring: false,
            stop_on_halt: false,
            data: None,
            writer: ParagraphWriter::new(regs::PARAGRAPH as usize),
            par: 0,
            t0: Instant::now(),
            recorded: 0,
        }
    }

    pub fn claims(addr: u32) -> bool {
        MCDS_RANGE.contains(&addr) || (regs::TRAM..regs::TRAM + regs::TRAM_SIZE).contains(&addr)
    }

    fn is_tram(addr: u32) -> bool {
        (regs::TRAM..regs::TRAM + regs::TRAM_SIZE).contains(&addr)
    }

    /// A device reset: the MCDS is off and forgets everything.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// `Err` = the access fails (clock off).
    #[allow(clippy::result_unit_err)]
    pub fn read(&mut self, addr: u32) -> Result<u32, ()> {
        if Self::is_tram(addr) {
            let o = (addr - regs::TRAM) as usize & !3;
            return Ok(u32::from_le_bytes(self.tram[o..o + 4].try_into().unwrap()));
        }
        if addr == regs::CLC {
            return Ok(self.clc);
        }
        if self.clc != 0 {
            return Err(());
        }
        Ok(match addr {
            regs::FIFONOW => self.fifonow,
            regs::CTRL => {
                if self.running {
                    self.ctrl_low & 0x2
                } else if self.done {
                    regs::CTRL_DONE | 0x3
                } else {
                    0x3
                }
            }
            regs::CFG10 => *self.regs.get(&addr).unwrap_or(&0xA0),
            _ => *self.regs.get(&addr).unwrap_or(&0),
        })
    }

    #[allow(clippy::result_unit_err)]
    pub fn write(&mut self, addr: u32, v: u32) -> Result<(), ()> {
        if Self::is_tram(addr) {
            let o = (addr - regs::TRAM) as usize & !3;
            self.tram[o..o + 4].copy_from_slice(&v.to_le_bytes());
            return Ok(());
        }
        if addr == regs::CLC {
            self.clc = v;
            return Ok(());
        }
        if self.clc != 0 {
            return Err(());
        }
        match addr {
            regs::CFG10 => {
                // Bit 15 written reads back as bit 23 (as on the chip).
                let r = if v & 0x8000 != 0 { (v & !0x8000) | 0x0080_0000 | 0x2000 } else { v };
                self.regs.insert(addr, r);
            }
            regs::CTRL => match v {
                regs::CTRL_CONFIG => {
                    self.running = false;
                    self.done = false;
                    self.fifonow = 0;
                }
                v if v & 0xF000 == 0x6000 => self.start(v),
                _ => self.running = false,
            },
            _ => {
                self.regs.insert(addr, v);
            }
        }
        Ok(())
    }

    fn act(&self, a: Action) -> u32 {
        *self.regs.get(&a.addr()).unwrap_or(&0)
    }

    fn start(&mut self, v: u32) {
        self.ctrl_low = v & 3;
        self.running = true;
        self.done = false;
        self.fifonow = 0;
        self.par = 0;
        self.writer = ParagraphWriter::new(regs::PARAGRAPH as usize);
        self.t0 = Instant::now();
        self.ring = self.regs.get(&regs::FIFO_WRAP).copied().unwrap_or(0) != 0;
        let done = self.act(Action::TraceDone);
        self.stop_on_halt = done & 0x40 != 0 && done & 0x20 == 0 && {
            let e = (done & 0x1F) as u8;
            let evt = *self.regs.get(&(regs::EVT + 4 * e as u32)).unwrap_or(&0);
            [regs::Input::DcuSus, regs::Input::DcuHalt]
                .iter()
                .any(|&i| regs::block_of(e, i).is_some_and(|b| evt & (1 << (2 * b as u32)) == 0))
        };
        self.data = (self.act(Action::DtuWadr) & 0x40 != 0).then(|| {
            let x = *self.regs.get(&regs::DTU_CMP[0]).unwrap_or(&0);
            let n = *self.regs.get(&(regs::DTU_CMP[0] + 4)).unwrap_or(&0);
            (x, n)
        });
        if self.act(Action::PtuEnable) != 0 {
            let total = if self.ring { regs::TRAM_SIZE as usize * 3 / 2 } else { usize::MAX };
            let mut written = 0usize;
            let mut k = 0u32;
            while self.running && written < total {
                let before = self.par * regs::PARAGRAPH as usize + self.writer.used();
                self.unit(CORE_TSU, 0, self.now() as u128 + k as u128 * 20, 32);
                self.unit(CORE_PTU, 0, 0x8000_0100u128 + (k % 64) as u128 * 0x20, 32);
                self.unit(CORE_PTU, 2, 8, 4);
                k += 1;
                let after = self.par * regs::PARAGRAPH as usize + self.writer.used();
                written += after.saturating_sub(before).max(1);
            }
        }
    }

    fn now(&self) -> u64 {
        // Units of 6.667 ns.
        (self.t0.elapsed().as_nanos() as u64) * 3 / 20
    }

    /// Appends a message at the write pointer (paragraph handling and the ring like the chip).
    fn unit(&mut self, core: u8, tt: u8, data: u128, bits: u16) {
        if !self.running {
            return;
        }
        if !self.writer.fits(bits) {
            let w = std::mem::replace(&mut self.writer, ParagraphWriter::new(regs::PARAGRAPH as usize));
            let bytes = w.finish_skip();
            let o = self.par * regs::PARAGRAPH as usize;
            self.tram[o..o + bytes.len()].copy_from_slice(&bytes);
            if self.par + 1 == regs::PARAGRAPHS && !self.ring {
                self.fifonow = regs::TRAM_SIZE - 0x20;
                self.running = false;
                self.done = true;
                return;
            }
            self.par = (self.par + 1) % regs::PARAGRAPHS;
        }
        self.writer.unit(core, tt, data, bits);
        let o = self.par * regs::PARAGRAPH as usize;
        let used = self.writer.used();
        self.tram[o..o + used].copy_from_slice(&self.writer.bytes()[..used]);
        self.fifonow = (o + used.div_ceil(0x20) * 0x20) as u32 % regs::TRAM_SIZE;
    }

    /// A store by the debugger: recorded when a data trace covers it.
    pub fn on_store(&mut self, addr: u32, value: u32) {
        let Some((x, n)) = self.data else { return };
        if !self.running || addr < x || addr > x.saturating_add(n) {
            return;
        }
        let t = self.now();
        self.unit(CORE_TSU, 0, t as u128, 32);
        let (tt, data, bits) = mcds::encode::dtw(value, 4, addr);
        self.unit(CORE_DTU0, tt, data, bits);
        self.recorded += 1;
    }

    /// The core the observation block follows ([`regs::POB_SEL`]; CPU0 until written).
    pub fn observed_core(&self) -> usize {
        self.regs.get(&regs::POB_SEL).and_then(|&v| regs::pob_core(v)).unwrap_or(0) as usize
    }

    /// The observed core's run state: a ring stopped by `dcu_sus` finishes when it halts.
    pub fn observe(&mut self, observed_halted: bool) {
        if self.running && self.stop_on_halt && observed_halted {
            self.running = false;
            self.done = true;
        }
    }
}
