//! The DMU of the simulator, as far as the UCBs need it: the 48 UCBs, the host command sequences
//! on them (erase, DFLASH page mode and Write Page, Disable/Resume Protection, Clear Status,
//! Reset to Read, PFLASH "verify erased"), the access protection of CONFIRMED UCBs, the
//! installation of `DMU_HF_CONFIRMx`/`PROCON*` at a reset, and an injected power cut after a
//! number of erase/program commands (the UCBs then keep what was done so far).

use crate::ucb::{self, get32, put32, Content, State, CONFIRMED, COUNT, PW, SIZE, UCBS, UNLOCKED};

const CMD_LO: u32 = 0xAF00_0000;
const CMD_HI: u32 = 0xAF10_0000;
const HF_STATUS: u32 = 0xF804_0010;
const HF_ERRSR: u32 = 0xF804_0034;
pub const SIM_CHIP_ID: u32 = 0x1020_7083;

const PROER: u32 = 1 << 2;
const PVER: u32 = 1 << 3;
const SQER: u32 = 1 << 1;
const EVER: u32 = 1 << 4;

#[derive(Clone)]
pub struct SimDmu {
    pub ucb: Vec<Content>,
    /// Installed at the last reset.
    pub confirm: [u32; 3],
    pub protect: u32,
    pub errsr: u32,
    /// Installed `PROCON*` registers (address, value).
    pub procon: std::collections::BTreeMap<u32, u32>,
    page_mode: bool,
    page: Vec<u8>,
    seq_addr: u32,
    seq_count: u32,
    seq_step: u8,
    pw_seq: Vec<u32>,
    /// A wrong password was given: every Disable Protection fails until the next reset.
    pw_blocked: bool,
    /// Erase and program commands executed.
    pub commands: usize,
    /// After this many commands the power fails: further commands do nothing.
    pub cut_after: Option<usize>,
    /// Logical PFLASH sectors (non-cached address) that hold data, for "verify erased".
    pub programmed_sectors: std::collections::BTreeSet<u32>,
    /// A CPU issues the command cycle and Safety ENDINIT is set: PFLASH erase and program fail
    /// with PROER (the debugger's commands are exempt).
    pub cpu_safety_endinit: bool,
    /// Data of the PFLASH pages programmed since (non-cached page address, 32 bytes each); it
    /// reads back at both the cached and the non-cached address, other PFLASH reads as zero.
    pub pflash_pages: std::collections::BTreeMap<u32, Vec<u8>>,
}

impl Default for SimDmu {
    fn default() -> Self {
        Self::new()
    }
}

impl SimDmu {
    /// A TC37x as delivered, with a valid BMHD0 (iLLD default, start 0xA0000000), its UID,
    /// Infineon's UCBs CONFIRMED and the application programmed at the start of PF0.
    pub fn new() -> Self {
        let mut ucb = vec![[0u8; SIZE]; COUNT];
        for d in UCBS {
            let c = ucb::delivery_default(d).unwrap_or_else(|| {
                let mut c = [0u8; SIZE];
                for &off in d.confirm {
                    put32(&mut c, off, CONFIRMED);
                }
                c
            });
            for x in d.indices() {
                ucb[x as usize] = c;
            }
        }
        // HSMCFG is UNLOCKED and all zero on a device without HSM configuration.
        put32(&mut ucb[7], 0x1F0, UNLOCKED);
        put32(&mut ucb[7], 0x1F8, UNLOCKED);
        let mut b = ucb[0];
        put32(&mut b, 0, 0xB359_007E);
        put32(&mut b, 4, 0xA000_0000);
        let crc = ucb::bmhd_crc(0xB359_007E, 0xA000_0000);
        put32(&mut b, 8, crc);
        put32(&mut b, 12, !crc);
        ucb[0] = b;
        ucb[8] = b;
        for (i, w) in [0x0A1B_3C4Du32, 0x0017_0277, 0x0000_A61F, 0x2C07_00E1].into_iter().enumerate() {
            put32(&mut ucb[5], 4 * i as u16, w);
        }
        let mut s = SimDmu {
            ucb,
            confirm: [0; 3],
            protect: 0,
            errsr: 0,
            procon: Default::default(),
            page_mode: false,
            page: Vec::new(),
            seq_addr: 0,
            seq_count: 0,
            seq_step: 0,
            pw_seq: Vec::new(),
            pw_blocked: false,
            commands: 0,
            cut_after: None,
            programmed_sectors: [0xA000_0000].into(),
            cpu_safety_endinit: false,
            pflash_pages: Default::default(),
        };
        s.install();
        s
    }

    fn content_state(&self, d: &ucb::UcbDef, x: u8) -> State {
        match d.confirm.first() {
            Some(&off) => State::of_code(get32(&self.ucb[x as usize], off)),
            None => State::Unknown,
        }
    }

    fn field(s: State) -> u32 {
        match s {
            State::Unlocked => 1,
            State::Confirmed => 2,
            State::Errored => 3,
            State::Unknown => 0,
        }
    }

    /// The source of the installation of a pair: ORIG unless it is ERRORED.
    fn source(&self, d: &ucb::UcbDef) -> Option<u8> {
        let o = self.content_state(d, d.orig);
        if o != State::Errored {
            return Some(d.orig);
        }
        d.copy.filter(|&c| self.content_state(d, c) != State::Errored)
    }

    /// What the startup does: confirmation states and protection installed, protection enabled.
    pub fn install(&mut self) {
        self.confirm = [0; 3];
        for d in UCBS {
            if let Some((r, ob, cb)) = d.conf_bits {
                self.confirm[r] |= Self::field(self.content_state(d, d.orig)) << ob;
                if let (Some(cb), Some(c)) = (cb, d.copy) {
                    self.confirm[r] |= Self::field(self.content_state(d, c)) << cb;
                }
            }
        }
        self.procon.clear();
        let reg = |s: &Self, name: &str, off: u16| {
            let d = &UCBS[ucb::find(name).unwrap()];
            s.source(d).map_or(0, |x| get32(&s.ucb[x as usize], off))
        };
        let v = [
            (ucb::HF_PROCONDBG, reg(self, "DBG", 0)),
            (ucb::HF_PROCONPF, reg(self, "PFLASH", 0xC0)),
            (ucb::HF_PROCONUSR, reg(self, "DFLASH", 0)),
            (ucb::HF_PROCONDF, reg(self, "DFLASH", 4)),
            (ucb::HF_PROCONRAM, reg(self, "DFLASH", 8)),
            (ucb::HF_PROCONTP, reg(self, "OTP0", 0x1E8)),
            (ucb::SP_PROCONHSM, reg(self, "HSM", 0)),
            (ucb::SP_PROCONHSMCFG, reg(self, "HSMCOTP0", 0x18) | reg(self, "HSMCOTP1", 0x18)),
            (ucb::SP_PROCONHSMCBS, reg(self, "HSMCOTP0", 4) | reg(self, "HSMCOTP1", 4)),
        ];
        self.procon.extend(v);
        self.protect = 0;
        self.pw_blocked = false;
        self.errsr = 0;
        self.page_mode = false;
        self.seq_step = 0;
    }

    /// Word of a programmed PFLASH page at `addr` (cached or non-cached address), if any.
    pub fn pflash_word(&self, addr: u32) -> Option<u32> {
        if !matches!(addr >> 28, 0x8 | 0xA) || addr & 0x0FFF_FFFF >= CMD_LO & 0x0FFF_FFFF {
            return None;
        }
        let a = (addr & 0x0FFF_FFFF & !3) | 0xA000_0000;
        let page = self.pflash_pages.get(&(a & !31))?;
        let off = (a & 31) as usize;
        Some(u32::from_le_bytes(page[off..off + 4].try_into().unwrap()))
    }

    pub fn claims(addr: u32) -> bool {
        (CMD_LO..CMD_HI).contains(&addr)
            || (ucb::BASE as u64..ucb::END).contains(&(addr as u64))
            || matches!(addr, HF_STATUS | HF_ERRSR | ucb::HF_PROTECT | ucb::SCU_CHIPID)
            || ucb::HF_CONFIRM.contains(&addr)
            || ucb::INSTALLED.contains(&addr)
    }

    fn pair_state(&self, d: &ucb::UcbDef) -> State {
        match d.conf_bits {
            Some((r, ob, cb)) => State::pair(State::of_field(self.confirm[r] >> ob), cb.map(|cb| State::of_field(self.confirm[r] >> cb))),
            None => State::Unknown,
        }
    }

    fn disabled(&self, d: &ucb::UcbDef) -> bool {
        d.prodis.is_some_and(|b| self.protect >> b & 1 != 0)
    }

    /// Whether a read of `addr` in the UCB area fails (UM 6.5.4.3).
    fn read_blocked(&self, addr: u32) -> bool {
        let idx = ((addr - ucb::BASE) / SIZE as u32) as u8;
        let off = ((addr - ucb::BASE) % SIZE as u32) as u16;
        let Some(di) = ucb::def_of_index(idx) else { return false };
        let d = &UCBS[di];
        let st = self.pair_state(d);
        let locked = (st == State::Confirmed && !self.disabled(d)) || st == State::Errored;
        match d.name {
            "PFLASH" | "DFLASH" | "ECPRIO" | "DBG" => locked,
            "BMHD0" | "SWAP" => locked && (PW..PW + 32).contains(&off),
            "HSMCFG" => State::of_field(self.confirm[0] >> 14) != State::Unlocked,
            _ => false,
        }
    }

    /// Whether the DMU accepts erasing or programming UCB `idx` (UM 6.5.4.3).
    fn write_allowed(&self, idx: u8, erase: bool) -> bool {
        let Some(di) = ucb::def_of_index(idx) else { return false };
        let d = &UCBS[di];
        if matches!(d.access, ucb::Access::ReadOnly(_)) {
            return false;
        }
        let st = self.pair_state(d);
        if st == State::Errored {
            return false;
        }
        let any = |s: State| matches!(s, State::Confirmed | State::Errored);
        match d.class {
            ucb::Class::Otp => {
                let proinotp = (0..8).any(|y| any(State::of_field(self.confirm[2] >> (2 * y))));
                // UM 6.5.4.3.15: erasable until PROINOTP, programmable while the set's ORIG is UNLOCKED.
                let y = (idx as u32 - 32) % 8;
                return if erase { !proinotp } else { State::of_field(self.confirm[2] >> (2 * y)) == State::Unlocked };
            }
            _ if d.name.starts_with("HSMCOTP") => {
                let s0 = self.pair_state(&UCBS[ucb::find("HSMCOTP0").unwrap()]);
                let s1 = self.pair_state(&UCBS[ucb::find("HSMCOTP1").unwrap()]);
                if erase && (any(s0) || any(s1)) {
                    return false;
                }
                return if d.name == "HSMCOTP0" { !any(s0) && s1 != State::Errored } else { s0 == State::Unlocked && !any(s1) };
            }
            _ if d.name == "HSMCFG" => return State::of_field(self.confirm[0] >> 14) == State::Unlocked,
            _ => {}
        }
        st != State::Confirmed || self.disabled(d)
    }

    /// The installed password of the UCB a Disable Protection UC code selects.
    fn password(&self, uc: u32) -> Option<(u8, [u32; 8])> {
        let name = match uc {
            0x00 => "BMHD0",
            0x10 | 0x20 => "PFLASH",
            0x11 => "DFLASH",
            0x12 => "DBG",
            0x16 => "ECPRIO",
            0x17 => "SWAP",
            _ => return None,
        };
        let d = &UCBS[ucb::find(name).unwrap()];
        let x = self.source(d)?;
        let mut pw = [0u32; 8];
        for (i, w) in pw.iter_mut().enumerate() {
            *w = get32(&self.ucb[x as usize], PW + 4 * i as u16);
        }
        Some((d.prodis.unwrap(), pw))
    }

    fn cut(&self) -> bool {
        self.cut_after.is_some_and(|n| self.commands >= n)
    }

    /// A register or UCB word; `None` where the DMU answers with a bus error.
    pub fn read32(&self, addr: u32) -> Option<u32> {
        Some(match addr {
            HF_STATUS => 0,
            HF_ERRSR => self.errsr,
            ucb::HF_PROTECT => self.protect,
            ucb::SCU_CHIPID => SIM_CHIP_ID,
            a if ucb::HF_CONFIRM.contains(&a) => self.confirm[ucb::HF_CONFIRM.iter().position(|&x| x == a).unwrap()],
            a if (ucb::BASE as u64..ucb::END).contains(&(a as u64)) => {
                if self.read_blocked(a) {
                    return None;
                }
                let idx = ((a - ucb::BASE) / SIZE as u32) as usize;
                get32(&self.ucb[idx], ((a - ucb::BASE) % SIZE as u32) as u16 & !3)
            }
            a if (CMD_LO..CMD_HI).contains(&a) => 0,
            a => self.procon.get(&a).copied().unwrap_or(0),
        })
    }

    /// A command cycle; `false` for a write outside the command area (a bus error).
    pub fn write32(&mut self, addr: u32, v: u32) -> bool {
        if !(CMD_LO..CMD_HI).contains(&addr) {
            return false;
        }
        let off = addr & 0xFFFF;
        match off {
            0x5554 => match v & 0xFF {
                0xFA => self.errsr = 0,
                0xF0 => {
                    self.errsr &= !(SQER | PROER);
                    self.page_mode = false;
                    self.seq_step = 0;
                    self.pw_seq.clear();
                }
                0xF5 => self.protect = 0,
                0x5D | 0x50 => {
                    self.page_mode = true;
                    self.page.clear();
                }
                _ => self.errsr |= SQER,
            },
            0x55F0 | 0x55F4 => {
                if self.page_mode {
                    self.page.extend_from_slice(&v.to_le_bytes());
                } else {
                    self.errsr |= SQER;
                }
            }
            0x553C => {
                self.pw_seq.push(v);
                if self.pw_seq.len() == 9 {
                    let uc = self.pw_seq[0];
                    let given: Vec<u32> = self.pw_seq[1..].to_vec();
                    self.pw_seq.clear();
                    match self.password(uc) {
                        Some((bit, pw)) if !self.pw_blocked && given == pw => self.protect |= 1 << bit,
                        _ => {
                            self.errsr |= PROER;
                            self.pw_blocked = true;
                        }
                    }
                }
            }
            0xAA50 => {
                self.seq_addr = v;
                self.seq_step = 1;
            }
            0xAA58 if self.seq_step == 1 => {
                self.seq_count = v;
                self.seq_step = 2;
            }
            0xAAA8 if self.seq_step == 2 => {
                self.seq_step = if matches!(v & 0xFF, 0x80 | 0xA0) { 3 } else { 0 };
            }
            0xAAA8 if self.seq_step == 3 => {
                self.seq_step = 0;
                self.command(v & 0xFF);
            }
            _ => self.errsr |= SQER,
        }
        true
    }

    fn command(&mut self, cmd: u32) {
        let a = self.seq_addr;
        let in_ucb = (ucb::BASE as u64..ucb::END).contains(&(a as u64));
        match cmd {
            // Verify Erased Page (PFLASH): EVER if programmed.
            0x56 => {
                if !a.is_multiple_of(32) {
                    self.errsr |= SQER;
                } else if self.pflash_pages.contains_key(&((a & 0x0FFF_FFFF) | 0xA000_0000)) {
                    self.errsr |= EVER;
                }
            }
            // Verify Erased Logical Sector Range (PFLASH): EVER if programmed.
            0x5F => {
                let sector = (a & 0x0FFF_FFFF & !0x3FFF) | 0xA000_0000;
                if (0..self.seq_count.max(1)).any(|k| self.programmed_sectors.contains(&(sector + k * 0x4000))) {
                    self.errsr |= EVER;
                }
            }
            // Erase Logical Sector Range.
            0x50 if in_ucb => {
                let idx = ((a - ucb::BASE) / SIZE as u32) as u8;
                if self.seq_count != 1 || !(a - ucb::BASE).is_multiple_of(SIZE as u32) {
                    self.errsr |= SQER;
                } else if !self.write_allowed(idx, true) {
                    self.errsr |= PROER;
                } else if !self.cut() {
                    self.ucb[idx as usize] = [0; SIZE];
                    self.commands += 1;
                }
            }
            // Write Page (DFLASH page: 8 bytes).
            0xAA if in_ucb => {
                let idx = ((a - ucb::BASE) / SIZE as u32) as u8;
                let off = ((a - ucb::BASE) % SIZE as u32) as usize;
                let page = std::mem::take(&mut self.page);
                self.page_mode = false;
                if page.len() != 8 || !off.is_multiple_of(8) {
                    self.errsr |= SQER;
                } else if !self.write_allowed(idx, false) {
                    self.errsr |= PROER;
                } else if !self.cut() {
                    let cur = &mut self.ucb[idx as usize][off..off + 8];
                    // Programming only sets bits; a page whose bits would have to clear fails.
                    if cur.iter().zip(&page).any(|(c, n)| c & !n != 0) {
                        self.errsr |= PVER;
                    } else {
                        cur.copy_from_slice(&page);
                    }
                    self.commands += 1;
                }
            }
            // PFLASH erase and program from a CPU with Safety ENDINIT set.
            0x50 | 0xAA | 0xA6 if (a >> 28) == 0xA && a < 0xAF00_0000 && self.cpu_safety_endinit => {
                self.page.clear();
                self.page_mode = false;
                self.errsr |= PROER;
            }
            // PFLASH erase and program: only which sectors hold data is modelled.
            0x50 if (a >> 28) == 0xA && a < 0xAF00_0000 => {
                for k in 0..self.seq_count.max(1) {
                    let sector = ((a & !0x3FFF) | 0xA000_0000) + k * 0x4000;
                    self.programmed_sectors.remove(&sector);
                    self.pflash_pages.retain(|&p, _| !(sector..sector + 0x4000).contains(&p));
                }
            }
            0xAA | 0xA6 if (a >> 28) == 0xA && a < 0xAF00_0000 => {
                let page = std::mem::take(&mut self.page);
                self.page_mode = false;
                let n = if cmd == 0xA6 { 256 } else { 32 };
                let pa = (a & !31) | 0xA000_0000;
                if page.len() != n || !a.is_multiple_of(n as u32) {
                    self.errsr |= SQER;
                } else if (0..n as u32 / 32).any(|k| self.pflash_pages.contains_key(&(pa + 32 * k))) {
                    // Programming over programmed data: the chip reports a program verify error.
                    self.errsr |= PVER;
                } else {
                    self.programmed_sectors.insert((a & !0x3FFF) | 0xA000_0000);
                    for (k, p) in page.chunks(32).enumerate() {
                        self.pflash_pages.insert(((a & !31) | 0xA000_0000) + 32 * k as u32, p.to_vec());
                    }
                }
            }
            _ => self.errsr |= SQER,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_state_installs() {
        let d = SimDmu::new();
        let dbg = &UCBS[ucb::find("DBG").unwrap()];
        assert_eq!(d.pair_state(dbg), State::Unlocked);
        assert_eq!(d.pair_state(&UCBS[ucb::find("SSW").unwrap()]), State::Confirmed);
        assert!(ucb::bmhd_valid(&d.ucb[0]));
    }
}
