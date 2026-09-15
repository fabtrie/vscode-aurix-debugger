//! `adapter driver sim`: a small TC37x model for offline tests.
//!
//! Models OSTATE/OEC, CPU_ID, DBGSR halt/resume/SUSP, a running PC and PC
//! triggers (a resume with an armed trigger halts immediately at it).
//! Every 32-bit write is recorded in [`SimAdapter::writes`].

use std::any::Any;
use std::collections::HashMap;

use tas::{Batch, BatchOp, OpResult, PL0_ERR_NO_ERROR};

use crate::adapter::{Adapter, AdapterSettings, DeviceInfo};
use crate::arch::{OEC, OSTATE};
use crate::error::Result;

const NUM_CORES: usize = 3;

pub struct SimAdapter {
    mem: HashMap<u32, u32>,
    pub writes: Vec<(u32, u32)>,
    pub halted: [bool; NUM_CORES],
    pub susp: [bool; NUM_CORES],
    /// SYSCON.BHALT: core not started yet.
    pub bhalt: [bool; NUM_CORES],
    pub evtsrc: [u32; NUM_CORES],
    pub pc: [u32; NUM_CORES],
    pub ostate: u32,
    pub pending_resets: u64,
    pub device_id: u32,
    initialized: bool,
    srst: bool,
}

impl Default for SimAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl SimAdapter {
    pub fn new() -> Self {
        SimAdapter {
            mem: HashMap::new(),
            writes: Vec::new(),
            halted: [false; NUM_CORES],
            susp: [false; NUM_CORES],
            bhalt: [false; NUM_CORES],
            evtsrc: [0; NUM_CORES],
            pc: [0x8000_1000, 0x8010_0000, 0x8020_0000],
            ostate: 0,
            pending_resets: 0,
            device_id: 0x1020_7083,
            initialized: false,
            srst: false,
        }
    }

    /// Simulates an external (watchdog) reset: OCDS disabled, cores running.
    pub fn external_reset(&mut self) {
        self.ostate = 0;
        self.halted = [false; NUM_CORES];
        self.pending_resets += 1;
    }

    fn core_of(addr: u32) -> Option<(usize, u32)> {
        (0..NUM_CORES).find_map(|i| {
            let base = 0xF881_0000 + 0x2_0000 * i as u32;
            (addr >= base && addr < base + 0x1_0000).then(|| (i, addr - base))
        })
    }

    fn read32(&mut self, addr: u32) -> u32 {
        if addr == OSTATE {
            return self.ostate;
        }
        if let Some((i, off)) = Self::core_of(addr) {
            match off {
                0xFE18 => return 0x00C0_C021,
                0xFD00 => {
                    return (self.halted[i] as u32) << 1
                        | 1
                        | (self.susp[i] as u32) << 4
                        | if self.halted[i] { (self.evtsrc[i] & 0x1F) << 8 } else { 0 }
                }
                0xFE08 => {
                    if !self.halted[i] {
                        self.pc[i] = self.pc[i].wrapping_add(2);
                    }
                    return self.pc[i];
                }
                0xFE14 => return (self.bhalt[i] as u32) << 24,
                _ => {}
            }
        }
        *self.mem.get(&addr).unwrap_or(&0)
    }

    fn write32(&mut self, addr: u32, v: u32) {
        self.writes.push((addr, v));
        if addr == OEC {
            if self.ostate & 1 == 0 {
                self.ostate |= 1;
            }
            return;
        }
        if let Some((i, off)) = Self::core_of(addr) {
            match off {
                0xFD00 => {
                    self.susp[i] = v & 0x10 != 0;
                    match (v >> 1) & 3 {
                        3 => {
                            self.halted[i] = true;
                            self.evtsrc[i] = 0;
                        }
                        2 => self.resume_core(i),
                        _ => {}
                    }
                    return;
                }
                0xFE08 => {
                    self.pc[i] = v;
                    return;
                }
                _ => {}
            }
        }
        self.mem.insert(addr, v);
    }

    fn resume_core(&mut self, i: usize) {
        self.halted[i] = false;
        let base = 0xF881_0000 + 0x2_0000 * i as u32;
        for n in 0..8u32 {
            let evt = *self.mem.get(&(base + 0xF000 + 8 * n)).unwrap_or(&0);
            if evt & (1 << 12) != 0 && evt & 7 != 0 {
                let adr = *self.mem.get(&(base + 0xF004 + 8 * n)).unwrap_or(&0);
                self.halted[i] = true;
                self.evtsrc[i] = 16 + n;
                self.pc[i] = if evt & 8 != 0 { adr } else { adr.wrapping_add(2) };
                break;
            }
        }
    }
}

impl Adapter for SimAdapter {
    fn driver_name(&self) -> &'static str {
        "sim"
    }

    fn init(&mut self, _s: &AdapterSettings) -> Result<DeviceInfo> {
        self.initialized = true;
        Ok(DeviceInfo { device_id: self.device_id, description: "simulated TC37x".into() })
    }

    fn initialized(&self) -> bool {
        self.initialized
    }

    fn execute(&mut self, batch: &Batch) -> Result<Vec<OpResult>> {
        let mut out = Vec::with_capacity(batch.len());
        for op in batch.iter() {
            match op {
                BatchOp::Read { addr, len, addr_map } => {
                    let mut data = vec![0u8; len];
                    if addr_map == 0 {
                        // One register read per word, so side effects happen once.
                        let mut k = 0;
                        while k < len {
                            let a = addr as u32 + k as u32;
                            let base = a & !3;
                            let w = self.read32(base).to_le_bytes();
                            let start = (a - base) as usize;
                            let n = (4 - start).min(len - k);
                            data[k..k + n].copy_from_slice(&w[start..start + n]);
                            k += n;
                        }
                    }
                    out.push(OpResult { err: PL0_ERR_NO_ERROR, bytes_ok: len, data });
                }
                BatchOp::Write { addr, data, addr_map } => {
                    if addr_map == 0 {
                        for (k, chunk) in data.chunks(4).enumerate() {
                            let mut w = [0u8; 4];
                            w[..chunk.len()].copy_from_slice(chunk);
                            self.write32(addr as u32 + 4 * k as u32, u32::from_le_bytes(w));
                        }
                    }
                    out.push(OpResult { err: PL0_ERR_NO_ERROR, bytes_ok: data.len(), data: Vec::new() });
                }
            }
        }
        Ok(out)
    }

    fn assert_srst(&mut self) -> Result<()> {
        self.srst = true;
        Ok(())
    }

    fn deassert_srst(&mut self, halt_after_reset: bool) -> Result<()> {
        if self.srst {
            self.srst = false;
            self.ostate = 0;
            self.halted = [halt_after_reset, false, false];
            self.pc = [0x800A_8000, 0x8010_0000, 0x8020_0000];
        }
        Ok(())
    }

    fn take_resets(&mut self) -> u64 {
        std::mem::take(&mut self.pending_resets)
    }

    fn set_speed_khz(&mut self, _khz: u32) -> Result<()> {
        Ok(())
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}
