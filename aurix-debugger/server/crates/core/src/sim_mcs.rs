//! GTM MCS model of the simulated TC37x: GTM_CLC, the channel and module registers of MCS0..4
//! and their program RAM.
//!
//! Enabled channels execute a few instructions whenever a register of their module is read (a
//! poll): MOVL, ADDL, SUBL, MOV, MRD, MWR, JMP, JBS, JBC, CALL, RET and WURM, which waits until
//! one of its mask bits is set in STRG. Other valid instructions just advance the PC; an invalid
//! one disables the channel with ERR set, as the MCS does.

use isa::mcs;

use crate::gtm_mcs::{arp_range, ccm_base, module_ram, module_regs, ARP_DIS_PROT, ARP_WPROT_AEI, CTRL_EN, GTM_CLC, DEFAULT_RAM_SIZE};

pub const MODULES: usize = 5;

#[derive(Clone, Debug, Default)]
pub struct SimChannel {
    pub en: bool,
    pub err: bool,
    pub pc: u32,
    pub r: [u32; 8],
    /// Waiting in a WURM instruction.
    pub waiting: bool,
    /// EN was cleared while waiting: the channel stops after the wait.
    pub disable_pending: bool,
    pub z: bool,
}

pub struct SimModule {
    pub ram: Vec<u32>,
    /// CCMi_ARPz_CTRL of the module's cluster.
    pub arp: [u32; 10],
    /// CCMi_PROT.CLS_PROT: ARP configuration locked.
    pub cls_prot: bool,
    pub ch: [SimChannel; 8],
    pub strg: u32,
}

impl SimModule {
    fn new() -> Self {
        let mut ch: [SimChannel; 8] = Default::default();
        for (x, c) in ch.iter_mut().enumerate() {
            c.pc = 4 * x as u32;
        }
        SimModule { ram: vec![0; (DEFAULT_RAM_SIZE / 4) as usize], arp: [0; 10], cls_prot: false, ch, strg: 0 }
    }
}

pub struct SimGtm {
    pub enabled: bool,
    pub modules: Vec<SimModule>,
    /// Instructions executed per enabled channel on each register read of its module.
    pub steps_per_read: u32,
}

impl Default for SimGtm {
    fn default() -> Self {
        Self::new()
    }
}

enum Loc {
    Clc,
    Ccm { m: usize, off: u32 },
    Reg { m: usize, off: u32 },
    Ram { m: usize, idx: usize },
}

impl SimGtm {
    pub fn new() -> Self {
        SimGtm { enabled: false, modules: (0..MODULES).map(|_| SimModule::new()).collect(), steps_per_read: 1 }
    }

    /// Loads a program image into module `m` (as the application does).
    pub fn load(&mut self, m: usize, words: &[u32]) {
        self.modules[m].ram[..words.len()].copy_from_slice(words);
    }

    fn locate(addr: u32) -> Option<Loc> {
        if addr == GTM_CLC {
            return Some(Loc::Clc);
        }
        (0..MODULES).find_map(|m| {
            let regs = module_regs(m as u32);
            let ram = module_ram(m as u32);
            let ccm = ccm_base(m as u32);
            if (ccm..ccm + 0x200).contains(&addr) {
                Some(Loc::Ccm { m, off: addr - ccm })
            } else if (regs..regs + 0x400).contains(&addr) {
                Some(Loc::Reg { m, off: addr - regs })
            } else if (ram..ram + DEFAULT_RAM_SIZE).contains(&addr) {
                Some(Loc::Ram { m, idx: ((addr - ram) / 4) as usize })
            } else {
                None
            }
        })
    }

    pub fn claims(addr: u32) -> bool {
        Self::locate(addr).is_some()
    }

    pub fn read(&mut self, addr: u32) -> Option<u32> {
        Some(match Self::locate(addr)? {
            Loc::Clc => {
                if self.enabled {
                    0
                } else {
                    2
                }
            }
            Loc::Ram { m, idx } => self.modules[m].ram[idx],
            Loc::Ccm { m, off } => match off {
                0x1FC => self.modules[m].cls_prot as u32,
                o if o < 80 && o % 8 == 0 => self.modules[m].arp[(o / 8) as usize],
                _ => 0,
            },
            Loc::Reg { m, off } => {
                self.run(m);
                let md = &self.modules[m];
                let (x, o) = ((off / 0x80) as usize, off % 0x80);
                let c = &md.ch[x];
                match (x, o) {
                    (0, 0x2C) => md.strg,
                    (_, 0..=0x1C) => c.r[(o / 4) as usize],
                    (_, 0x20) => (c.en as u32) | (c.err as u32) << 2 | (c.z as u32) << 5,
                    (_, 0x40) => c.pc,
                    _ => 0,
                }
            }
        })
    }

    /// Writes a GTM register or MCS RAM word: None for other addresses, Some(false) for a write
    /// refused by an address range protector.
    pub fn write(&mut self, addr: u32, v: u32) -> Option<bool> {
        let loc = Self::locate(addr)?;
        match loc {
            Loc::Clc => self.enabled = v & 1 == 0,
            Loc::Ram { m, idx } => {
                let md = &mut self.modules[m];
                let a = idx as u32 * 4;
                let blocked = md.arp.iter().any(|&c| {
                    let (s, e) = arp_range(c);
                    c & ARP_WPROT_AEI != 0 && c & ARP_DIS_PROT == 0 && (s..e).contains(&a)
                });
                if blocked {
                    return Some(false);
                }
                md.ram[idx] = v;
            }
            Loc::Ccm { m, off } => match off {
                0x1FC => self.modules[m].cls_prot = v & 1 != 0,
                o if o < 80 && o % 8 == 0 && !self.modules[m].cls_prot => self.modules[m].arp[(o / 8) as usize] = v,
                _ => {}
            },
            Loc::Reg { m, off } => {
                let md = &mut self.modules[m];
                let (x, o) = ((off / 0x80) as usize, off % 0x80);
                match (x, o) {
                    (0, 0x2C) => md.strg |= v,
                    (0, 0x28) => md.strg &= !v,
                    (_, 0..=0x1C) if !md.ch[x].en => md.ch[x].r[(o / 4) as usize] = v & 0xFF_FFFF,
                    (_, 0x20) => {
                        let c = &mut md.ch[x];
                        if v & CTRL_EN != 0 {
                            c.en = true;
                            c.err = false;
                            c.disable_pending = false;
                        } else if c.waiting {
                            c.disable_pending = true;
                        } else {
                            c.en = false;
                        }
                    }
                    (_, 0x40) if !md.ch[x].en => md.ch[x].pc = v & 0xFFFC,
                    _ => {}
                }
            }
        }
        Some(true)
    }

    fn run(&mut self, m: usize) {
        for _ in 0..self.steps_per_read {
            for x in 0..8 {
                if self.modules[m].ch[x].en {
                    self.exec(m, x);
                }
            }
        }
    }

    fn exec(&mut self, m: usize, x: usize) {
        let md = &mut self.modules[m];
        let mut c = md.ch[x].clone();
        let pc = c.pc;
        let word = md.ram.get((pc / 4) as usize).copied().unwrap_or(0);
        let Some(op) = mcs::decode(word) else {
            c.en = false;
            c.err = true;
            md.ch[x] = c;
            return;
        };
        let a = mcs::operands(op, word);
        let strg = md.strg;
        let get = |c: &SimChannel, i: i64| -> u32 {
            match i {
                0..=7 => c.r[i as usize],
                8 => (c.en as u32) | (c.z as u32) << 5,
                11 => strg,
                _ => 0,
            }
        };
        let set = |c: &mut SimChannel, i: i64, v: u32| {
            if (0..=7).contains(&i) {
                c.r[i as usize] = v & 0xFF_FFFF;
            } else if i == 8 {
                c.en = v & 1 != 0;
            }
            c.z = v & 0xFF_FFFF == 0;
        };
        let mut next = pc + 4;
        match op.name {
            "MOVL" => set(&mut c, a[0], a[1] as u32),
            "ADDL" => {
                let v = get(&c, a[0]).wrapping_add(a[1] as u32);
                set(&mut c, a[0], v);
            }
            "SUBL" => {
                let v = get(&c, a[0]).wrapping_sub(a[1] as u32);
                set(&mut c, a[0], v);
            }
            "MOV" => {
                let v = get(&c, a[1]);
                set(&mut c, a[0], v);
            }
            "MRD" => {
                let v = md.ram[(a[1] / 4) as usize];
                set(&mut c, a[0], v);
            }
            "MWR" => md.ram[(a[1] / 4) as usize] = get(&c, a[0]),
            "JMP" => next = a[0] as u32,
            "JBS" | "JBC" => {
                let bit = get(&c, a[0]) >> a[1] & 1 != 0;
                if bit == (op.name == "JBS") {
                    next = a[2] as u32;
                }
            }
            "CALL" => {
                let sp = c.r[7].wrapping_add(4) & 0x7FFC;
                md.ram[(sp / 4) as usize] = pc + 4;
                c.r[7] = sp;
                next = a[0] as u32;
            }
            "RET" => {
                let sp = c.r[7] & 0x7FFC;
                next = md.ram[(sp / 4) as usize] & 0x7FFC;
                c.r[7] = sp.wrapping_sub(4);
            }
            "WURM" => {
                let mask = a[2] as u32;
                if strg & mask == 0 {
                    c.waiting = true;
                    md.ch[x] = c;
                    return;
                }
                md.strg &= !mask;
                c.waiting = false;
                if c.disable_pending {
                    c.disable_pending = false;
                    c.en = false;
                }
            }
            _ => {}
        }
        c.pc = next & 0xFFFC;
        md.ch[x] = c;
    }
}
