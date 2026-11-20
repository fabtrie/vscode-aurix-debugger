//! A TriCore interpreter for the simulator, for the RAM flash loader ([`crate::flash_loader`]):
//! only the 32-bit encodings it is assembled to. Anything else stops the core with a fault, so a
//! loader change that needs more shows up in the tests.

/// Word access to the simulated chip.
pub trait Bus {
    fn read32(&mut self, addr: u32) -> u32;
    fn write32(&mut self, addr: u32, v: u32);
}

/// Why [`SimCpu::run`] returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// The instruction budget is used up.
    Budget,
    /// The core loops without storing anything: it waits for the debugger.
    Idle,
    /// An instruction the interpreter does not know, or a misaligned access.
    Fault(String),
}

#[derive(Debug, Clone, Default)]
pub struct SimCpu {
    pub d: [u32; 16],
    pub a: [u32; 16],
    pub pc: u32,
    /// ICR.IE (cleared by DISABLE).
    pub ie: bool,
    pub executed: u64,
    stores: u64,
    /// The last taken backward branch, the store count and data registers then, and how often it
    /// repeated so. A loop that changes a register (a counter) is not waiting.
    spin: (u32, u64, [u32; 16], u32),
}

fn sext(v: u32, bits: u32) -> u32 {
    let s = 32 - bits;
    (((v << s) as i32) >> s) as u32
}

fn shift(v: u32, count: u32) -> u32 {
    let n = sext(count & 0x3F, 6) as i32;
    match n {
        0..=31 => v << n,
        -31..=-1 => v >> -n,
        _ => 0,
    }
}

impl SimCpu {
    /// Runs until the core idles, faults or `budget` instructions have run.
    pub fn run(&mut self, bus: &mut dyn Bus, budget: usize) -> Stop {
        for _ in 0..budget {
            match self.step(bus) {
                Ok(false) => {}
                Ok(true) => return Stop::Idle,
                Err(e) => return Stop::Fault(e),
            }
        }
        Stop::Budget
    }

    fn store(&mut self, bus: &mut dyn Bus, addr: u32, v: u32) -> Result<(), String> {
        if addr & 3 != 0 {
            return Err(format!("misaligned store to 0x{addr:08x} at 0x{:08x}", self.pc));
        }
        self.stores += 1;
        bus.write32(addr, v);
        Ok(())
    }

    fn load(&mut self, bus: &mut dyn Bus, addr: u32) -> Result<u32, String> {
        if addr & 3 != 0 {
            return Err(format!("misaligned load from 0x{addr:08x} at 0x{:08x}", self.pc));
        }
        Ok(bus.read32(addr))
    }

    /// A taken branch to `target`; returns whether the core spins (backward, nothing stored).
    fn branch(&mut self, target: u32) -> bool {
        let from = self.pc;
        self.pc = target;
        if target > from {
            return false;
        }
        if self.spin.0 == from && self.spin.1 == self.stores && self.spin.2 == self.d {
            self.spin.3 += 1;
        } else {
            self.spin = (from, self.stores, self.d, 0);
        }
        self.spin.3 >= 32
    }

    /// One instruction; `Ok(true)`: the core spins.
    fn step(&mut self, bus: &mut dyn Bus) -> Result<bool, String> {
        let pc = self.pc;
        let w = bus.read32(pc);
        let fault = || format!("unknown instruction 0x{w:08x} at 0x{pc:08x}");
        if w & 1 == 0 {
            return Err(fault());
        }
        self.executed += 1;
        let c = ((w >> 28) & 0xF) as usize;
        let s1 = ((w >> 8) & 0xF) as usize;
        let s2 = ((w >> 12) & 0xF) as usize;
        let k16 = (w >> 12) & 0xFFFF;
        let off16 = sext(((w >> 16) & 0x3F) | ((w >> 28) & 0xF) << 6 | ((w >> 22) & 0x3F) << 10, 16);
        let disp15 = sext((w >> 16) & 0x7FFF, 15).wrapping_mul(2);
        let next = pc.wrapping_add(4);
        let mut jump: Option<u32> = None;
        match w & 0xFF {
            0x0D => match (w >> 22) & 0x3F {
                0x0D => self.ie = false,
                0x12 | 0x13 => {} // dsync, isync
                _ => return Err(fault()),
            },
            0x91 => self.a[c] = k16 << 16,
            // MFCR of CCNT: one cycle per instruction.
            0x4D if k16 == 0xFC04 => self.d[c] = self.executed as u32,
            0x7B => self.d[c] = k16 << 16,
            0x3B => self.d[c] = sext(k16, 16),
            0x1B => self.d[c] = self.d[s1].wrapping_add(sext(k16, 16)),
            0xD9 => self.a[s1] = self.a[s2].wrapping_add(off16),
            // LEA with a 10-bit offset (BO format), which the assembler picks for small ones.
            0x49 if (w >> 22) & 0x3F == 0x28 => {
                self.a[s1] = self.a[s2].wrapping_add(sext(((w >> 16) & 0x3F) | ((w >> 28) & 0xF) << 6, 10));
            }
            0x19 => self.d[s1] = self.load(bus, self.a[s2].wrapping_add(off16))?,
            0x59 => {
                let v = self.d[s1];
                self.store(bus, self.a[s2].wrapping_add(off16), v)?;
            }
            op @ (0x09 | 0x89) => {
                let off10 = sext(((w >> 16) & 0x3F) | ((w >> 28) & 0xF) << 6, 10);
                let (words, post) = match (w >> 22) & 0x3F {
                    0x24 => (1, false),
                    0x25 => (2, false),
                    0x04 => (1, true),
                    0x05 => (2, true),
                    _ => return Err(fault()),
                };
                let addr = if post { self.a[s2] } else { self.a[s2].wrapping_add(off10) };
                for i in 0..words {
                    let a = addr.wrapping_add(4 * i as u32);
                    if op == 0x09 {
                        self.d[s1 + i] = self.load(bus, a)?;
                    } else {
                        let v = self.d[s1 + i];
                        self.store(bus, a, v)?;
                    }
                }
                if post {
                    self.a[s2] = self.a[s2].wrapping_add(off10);
                }
            }
            0x0B => {
                let (x, y) = (self.d[s1], self.d[s2]);
                self.d[c] = match (w >> 20) & 0xFF {
                    0x00 => x.wrapping_add(y),
                    0x08 => x.wrapping_sub(y),
                    0x1F => y,
                    _ => return Err(fault()),
                };
            }
            0x0F => {
                let (x, y) = (self.d[s1], self.d[s2]);
                self.d[c] = match (w >> 20) & 0xFF {
                    0x00 => shift(x, y),
                    0x08 => x & y,
                    0x0A => x | y,
                    0x0C => x ^ y,
                    _ => return Err(fault()),
                };
            }
            0x8F => {
                let (x, k9) = (self.d[s1], (w >> 12) & 0x1FF);
                self.d[c] = match (w >> 21) & 0x7F {
                    0x00 => shift(x, k9),
                    0x08 => x & k9,
                    0x0A => x | k9,
                    0x0C => x ^ k9,
                    _ => return Err(fault()),
                };
            }
            0x01 if (w >> 20) & 0xFF == 0x00 => self.a[c] = self.a[s2],
            0x01 if (w >> 20) & 0xFF == 0x63 => self.a[c] = self.d[s2], // mov.a
            0x2D if (w >> 20) & 0xFF == 0x03 => jump = Some(self.a[s1] & !1),
            0x37 if (w >> 21) & 3 == 3 => {
                let (pos, width) = ((w >> 23) & 0x1F, (w >> 16) & 0x1F);
                let mask = if width == 0 { 0 } else { u32::MAX >> (32 - width) };
                self.d[c] = (self.d[s1] >> pos) & mask;
            }
            op @ (0x1D | 0x5D) => {
                let disp24 = ((w >> 16) & 0xFFFF) | ((w >> 8) & 0xFF) << 16;
                if op == 0x5D {
                    self.a[11] = next;
                }
                jump = Some(pc.wrapping_add(sext(disp24, 24).wrapping_mul(2)));
            }
            0xDF => {
                let eq = self.d[s1] == sext((w >> 12) & 0xF, 4);
                if eq != (w >> 31 != 0) {
                    jump = Some(pc.wrapping_add(disp15));
                }
            }
            0x5F => {
                let eq = self.d[s1] == self.d[s2];
                if eq != (w >> 31 != 0) {
                    jump = Some(pc.wrapping_add(disp15));
                }
            }
            0x3F => {
                let lt = if w >> 31 != 0 { self.d[s1] < self.d[s2] } else { (self.d[s1] as i32) < (self.d[s2] as i32) };
                if lt {
                    jump = Some(pc.wrapping_add(disp15));
                }
            }
            0x6F => {
                let n = ((w >> 12) & 0xF) | ((w >> 7) & 1) << 4;
                let set = (self.d[s1] >> n) & 1 != 0;
                if set == (w >> 31 != 0) {
                    jump = Some(pc.wrapping_add(disp15));
                }
            }
            _ => return Err(fault()),
        }
        match jump {
            Some(t) => Ok(self.branch(t)),
            None => {
                self.pc = next;
                Ok(false)
            }
        }
    }
}
