//! Flash banks (`flash bank`) and the AURIX TC3xx PFLASH/DFLASH driver (`tc3x_eflash`,
//! also accepted as `aurix_eflash`). DFLASH banks support erase and erase check only.
//!
//! Programming runs the DMU command sequences over the debug interface (no RAM
//! loader). `write_image` is incremental per 16 KB sector:
//! - an erased sector is programmed directly; erased state is checked with the DMU
//!   "verify erased" command, because reading erased PFLASH causes ECC bus errors,
//! - a sector whose image pages already hold the image data is skipped,
//! - any other sector is erased and programmed.
//!
//! All cores are halted first: they execute from the flash being changed.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use tas::Batch;

use crate::adapter::{check, Io};
use crate::error::{other, usage, Error, Result};
use crate::image::Image;
use crate::target::TargetState;
use crate::{log_info, log_warn, ops, Cx};

#[derive(Clone, Debug)]
pub struct FlashBank {
    pub name: String,
    pub driver: String,
    pub base: u32,
    pub size: u32,
    pub chip_width: u32,
    pub bus_width: u32,
    pub target: String,
    pub extra: Vec<String>,
}

impl FlashBank {
    fn contains(&self, start: u32, end: u64) -> bool {
        start >= self.base && end <= self.base as u64 + self.size as u64
    }
}

pub const DRIVERS: &[&str] = &["tc3x_eflash", "aurix_eflash"];

const SCU_CHIPID: u32 = 0xF003_6140;
const CMD: u32 = 0xAF00_0000;
const KEY: u32 = CMD + 0x5554;
const LOAD_PAGE: u32 = CMD + 0x55F0;
const SEQ_ADDR: u32 = CMD + 0xAA50;
const SEQ_COUNT: u32 = CMD + 0xAA58;
const SEQ_CMD: u32 = CMD + 0xAAA8;
const DMU_HF_STATUS: u32 = 0xF804_0010;
const DMU_HF_ERRSR: u32 = 0xF804_0034;

const KEY_CLEAR_STATUS: u32 = 0xFA;
const KEY_RESET_TO_READ: u32 = 0xF0;
const KEY_PAGE_MODE_PFLASH: u32 = 0x50;

const ERR_OPER: u32 = 1 << 0;
const ERR_SQER: u32 = 1 << 1;
const ERR_PROER: u32 = 1 << 2;
const ERR_PVER: u32 = 1 << 3;
const ERR_EVER: u32 = 1 << 4;
const ERR_MASK: u32 = 0x7F;

pub const SECTOR_SIZE: u32 = 16 * 1024;
const PAGE_SIZE: u32 = 32;
const BURST_SIZE: usize = 256;
/// Logical sectors per erase command (512 KB).
const MAX_ERASE_SECTORS: u32 = 32;
const ERASE_TIMEOUT: Duration = Duration::from_secs(10);
const PROGRAM_TIMEOUT: Duration = Duration::from_secs(2);
/// Status reads after a program command before the result is read. A 256-byte burst was seen
/// to end within 3 reads (TC377, miniWiggler at 5 MHz); if not, the result is polled.
const PROGRAM_SETTLE_READS: usize = 3;
/// Bytes of a programmed page that the image does not define.
const PAD: u8 = 0;
/// Largest block compared per read in verify_image.
const VERIFY_CHUNK: usize = 0x10000;

/// A PFLASH bank of the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pflash {
    pub base: u32,
    pub size: u32,
    /// Physical sector size; erase commands cannot cross it.
    pub phys_sector: u32,
    /// Busy bit of the bank in DMU_HF_STATUS.
    pub busy_bit: u32,
    /// Logical sector size (the erase unit).
    pub sector_size: u32,
    /// Logical sectors one erase command may cover.
    pub max_erase_sectors: u32,
    pub dflash: bool,
}

const MB: u32 = 1024 * 1024;

const fn pf(base: u32, size: u32, busy_bit: u32) -> Pflash {
    Pflash { base, size, phys_sector: MB, busy_bit, sector_size: SECTOR_SIZE, max_erase_sectors: MAX_ERASE_SECTORS, dflash: false }
}

/// DFLASH0 (EEPROM emulation, NVM data) of all TC3xx variants: 4 KB logical sectors, up to 256 KB
/// per erase command, at most 1 MB (the `flash bank` size sets the actual size).
const DF0: Pflash = Pflash { base: 0xAF00_0000, size: MB, phys_sector: MB, busy_bit: 0, sector_size: 4 * 1024, max_erase_sectors: 64, dflash: true };
/// DFLASH1 (HSM data) of all TC3xx variants: 128 KB, 4 KB logical sectors. Erasing it from the
/// debugger only works if the HSM does not protect it.
const DF1: Pflash = Pflash { base: 0xAFC0_0000, size: 128 * 1024, phys_sector: 128 * 1024, busy_bit: 1, ..DF0 };

const TC36X: &[Pflash] = &[pf(0x8000_0000, 2 * MB, 2), pf(0x8030_0000, 2 * MB, 3)];
const TC37X: &[Pflash] = &[pf(0x8000_0000, 3 * MB, 2), pf(0x8030_0000, 3 * MB, 3)];
const TC38X: &[Pflash] = &[pf(0x8000_0000, 3 * MB, 2), pf(0x8030_0000, 3 * MB, 3), pf(0x8060_0000, 3 * MB, 4), pf(0x8090_0000, MB, 5)];
const TC39X: &[Pflash] = &[
    pf(0x8000_0000, 3 * MB, 2),
    pf(0x8030_0000, 3 * MB, 3),
    pf(0x8060_0000, 3 * MB, 4),
    pf(0x8090_0000, 3 * MB, 5),
    pf(0x80C0_0000, 3 * MB, 6),
    pf(0x80F0_0000, MB, 7),
];
const TC3EX: &[Pflash] = &[pf(0x8000_0000, 3 * MB, 2), pf(0x8030_0000, 3 * MB, 3), pf(0x8060_0000, 3 * MB, 4), pf(0x8090_0000, 3 * MB, 5)];

/// PFLASH banks per TC3xx variant (SCU_CHIPID.CHID).
fn pflash_banks(variant: u32) -> &'static [Pflash] {
    match variant {
        0x6 => TC36X,
        0x7 => TC37X,
        0x8 => TC38X,
        0x9 => TC39X,
        0xE => TC3EX,
        _ => &[],
    }
}

/// Non-cached address (segment 0xA) of a flash address.
fn seg_a(addr: u32) -> u32 {
    (addr & 0x0FFF_FFFF) | 0xA000_0000
}

fn is_flash_address(addr: u32) -> bool {
    matches!(addr >> 28, 0x8 | 0xA)
}

fn err_text(e: u32) -> String {
    const NAMES: &[(u32, &str)] = &[
        (ERR_OPER, "operation error"),
        (ERR_SQER, "command sequence error"),
        (ERR_PROER, "protection error"),
        (ERR_PVER, "program verify error"),
        (ERR_EVER, "erase verify error"),
        (1 << 5, "SRI bus address ECC error"),
        (1 << 6, "original error"),
    ];
    NAMES.iter().filter(|(b, _)| e & b != 0).map(|(_, n)| *n).collect::<Vec<_>>().join(", ")
}

/// Identifies the PFLASH bank a `flash bank` definition refers to.
pub fn probe(io: &mut Io, bank: &FlashBank) -> Result<Pflash> {
    if !DRIVERS.contains(&bank.driver.as_str()) {
        return Err(usage(format!("flash driver '{}' not supported", bank.driver)));
    }
    let chipid = io.read32(SCU_CHIPID)?;
    if chipid & 0xC0 != 0x80 {
        return Err(other(format!("SCU_CHIPID 0x{chipid:08x} is not an AURIX TC3xx device")));
    }
    let variant = (chipid >> 12) & 0xF;
    let banks = pflash_banks(variant);
    banks
        .iter()
        .chain(if banks.is_empty() { &[][..] } else { &[DF0, DF1][..] })
        // The DFLASH size depends on the device (256 KB on the TC377 board): up to 1 MB is accepted.
        .find(|p| {
            let size_ok = if p.dflash { bank.size > 0 && bank.size <= p.size && bank.size % p.sector_size == 0 } else { p.size == bank.size };
            p.base & 0x0FFF_FFFF == bank.base & 0x0FFF_FFFF && size_ok
        })
        .map(|p| Pflash { base: bank.base, size: bank.size, ..*p })
        .ok_or_else(|| {
            other(format!(
                "{}: no TC3{variant:X}x PFLASH or DFLASH bank at 0x{:08x} with size 0x{:x} (UCB banks are not supported)",
                bank.name, bank.base, bank.size
            ))
        })
}

/// DMU command sequences on one PFLASH bank.
pub struct Driver<'a, 'b> {
    io: &'a mut Io<'b>,
    pub bank: Pflash,
}

impl<'a, 'b> Driver<'a, 'b> {
    pub fn new(io: &'a mut Io<'b>, bank: Pflash) -> Self {
        Driver { io, bank }
    }

    fn status(&mut self) -> Result<(u32, u32)> {
        let v = self.io.read32_many(&[DMU_HF_STATUS, DMU_HF_ERRSR])?;
        Ok((v[0], v[1]))
    }

    fn key(&mut self, k: u32) -> Result<()> {
        self.io.write32(KEY, k)
    }

    fn begin(&mut self) -> Result<()> {
        let (st, _) = self.status()?;
        if st & (1 << self.bank.busy_bit) != 0 {
            return Err(other("flash is busy with another operation"));
        }
        self.key(KEY_CLEAR_STATUS)
    }

    /// Waits for the end of the running operation; returns its error flags.
    fn wait(&mut self, timeout: Duration) -> Result<u32> {
        let t0 = Instant::now();
        loop {
            let (st, err) = self.status()?;
            if st & (1 << self.bank.busy_bit) == 0 {
                return Ok(err & ERR_MASK);
            }
            if t0.elapsed() > timeout {
                let _ = self.key(KEY_RESET_TO_READ);
                return Err(Error::Timeout("flash operation timed out".into()));
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn fail(&mut self, what: String, err: u32) -> Error {
        let _ = self.key(if err & (ERR_SQER | ERR_PROER) != 0 { KEY_RESET_TO_READ } else { KEY_CLEAR_STATUS });
        other(format!("flash {what} failed: {}", err_text(err)))
    }

    fn sequence(&mut self, addr: u32, count: u32, cmd: u32) -> Result<()> {
        self.io.write32_seq(&[(SEQ_ADDR, seg_a(addr)), (SEQ_COUNT, count), (SEQ_CMD, 0x80), (SEQ_CMD, cmd)])
    }

    /// Sectors from `addr` that one command may cover.
    fn chunk(&self, addr: u32, remaining: u32) -> u32 {
        let off = (addr & 0x0FFF_FFFF) - (self.bank.base & 0x0FFF_FFFF);
        let to_phys = (self.bank.phys_sector - off % self.bank.phys_sector) / self.bank.sector_size;
        remaining.min(self.bank.max_erase_sectors).min(to_phys.max(1))
    }

    /// Erases `sectors` logical sectors from `addr`.
    pub fn erase(&mut self, mut addr: u32, mut sectors: u32) -> Result<()> {
        self.begin()?;
        while sectors > 0 {
            let n = self.chunk(addr, sectors);
            self.sequence(addr, n, 0x50)?;
            let err = self.wait(ERASE_TIMEOUT)?;
            if err != 0 {
                return Err(self.fail(format!("erase at 0x{addr:08x}"), err));
            }
            addr += n * self.bank.sector_size;
            sectors -= n;
        }
        Ok(())
    }

    /// Whether all sectors in the range are erased (DMU "verify erased"; no flash read).
    pub fn is_erased(&mut self, mut addr: u32, mut sectors: u32) -> Result<bool> {
        self.begin()?;
        while sectors > 0 {
            let n = self.chunk(addr, sectors);
            self.sequence(addr, n, 0x5F)?;
            let err = self.wait(ERASE_TIMEOUT)?;
            if err & !ERR_EVER != 0 {
                return Err(self.fail(format!("erase check at 0x{addr:08x}"), err));
            }
            if err & ERR_EVER != 0 {
                self.key(KEY_CLEAR_STATUS)?;
                return Ok(false);
            }
            addr += n * self.bank.sector_size;
            sectors -= n;
        }
        Ok(true)
    }

    /// Programs erased flash with page-aligned `data` (a multiple of 32 bytes). Each command is
    /// one TAS transaction that also reads the result, so polling is only needed if the flash is
    /// still busy by then. Every TAS operation costs 0.1..0.2 ms at 5 MHz, so fewer operations
    /// is what makes programming faster.
    pub fn program(&mut self, addr: u32, data: &[u8]) -> Result<()> {
        if addr % PAGE_SIZE != 0 || data.len() % PAGE_SIZE as usize != 0 {
            return Err(usage("flash program: data is not page aligned"));
        }
        self.begin()?;
        let mut off = 0usize;
        while off < data.len() {
            let a = addr + off as u32;
            // A burst must start on a 256-byte boundary of the flash, not of the run (SQER otherwise).
            let n = if a % BURST_SIZE as u32 == 0 && data.len() - off >= BURST_SIZE { BURST_SIZE } else { PAGE_SIZE as usize };
            let mut b = Batch::new();
            b.write32(KEY as u64, KEY_PAGE_MODE_PFLASH);
            // One block write per page loads both assembly registers (0x55F0, 0x55F4).
            for w in data[off..off + n].chunks(8) {
                b.write(LOAD_PAGE as u64, w);
            }
            b.write32(SEQ_ADDR as u64, seg_a(a));
            b.write32(SEQ_COUNT as u64, 0);
            b.write32(SEQ_CMD as u64, 0xA0);
            b.write32(SEQ_CMD as u64, if n == BURST_SIZE { 0xA6 } else { 0xAA });
            for _ in 0..PROGRAM_SETTLE_READS {
                b.read32(DMU_HF_STATUS as u64);
            }
            b.read32(DMU_HF_STATUS as u64);
            b.read32(DMU_HF_ERRSR as u64);
            let res = self.io.batch(&b)?;
            for r in &res {
                check(r, a as u64, true)?;
            }
            let (st, err) = (res[res.len() - 2].u32(), res[res.len() - 1].u32() & ERR_MASK);
            let err = if st & (1 << self.bank.busy_bit) != 0 { self.wait(PROGRAM_TIMEOUT)? } else { err };
            if err != 0 {
                return Err(self.fail(format!("program at 0x{a:08x}"), err));
            }
            off += n;
        }
        Ok(())
    }
}

// --- Image operations ------------------------------------------------------------------------

/// Flash bank by index or name.
pub fn find_bank(cx: &Cx, spec: &str) -> Result<FlashBank> {
    cx.with(|c| {
        spec.parse::<usize>()
            .ok()
            .and_then(|i| c.flash_banks.get(i))
            .or_else(|| c.flash_banks.iter().find(|b| b.name == spec))
            .cloned()
            .ok_or_else(|| usage(format!("flash bank '{spec}' not found")))
    })
}

/// Halts all examined cores.
pub fn halt_all(cx: &Cx) -> Result<()> {
    let n = cx.with(|c| c.targets.len());
    let examined = |t: usize| cx.with(|c| c.targets[t].core.examined);
    for t in 0..n {
        if examined(t) && ops::state(cx, t).0 != TargetState::Halted {
            ops::halt(cx, t)?;
        }
    }
    for t in 0..n {
        if examined(t) {
            ops::wait_state(cx, t, TargetState::Halted, 2000)?;
        }
    }
    Ok(())
}

pub fn probe_bank(cx: &Cx, bank: &FlashBank) -> Result<Pflash> {
    cx.with(|c| probe(&mut c.io()?, bank))
}

/// Pages (32 bytes) of the image inside `[lo, hi)`, undefined bytes padded.
fn image_pages(img: &Image, lo: u32, hi: u64) -> BTreeMap<u32, [u8; PAGE_SIZE as usize]> {
    let mut pages = BTreeMap::new();
    for s in &img.sections {
        let start = (s.addr as u64).max(lo as u64);
        let end = s.end().min(hi);
        let mut a = start;
        while a < end {
            let page = a as u32 & !(PAGE_SIZE - 1);
            let n = ((page as u64 + PAGE_SIZE as u64).min(end) - a) as usize;
            let (to, from) = ((a - page as u64) as usize, (a - s.addr as u64) as usize);
            pages.entry(page).or_insert([PAD; PAGE_SIZE as usize])[to..to + n].copy_from_slice(&s.data[from..from + n]);
            a += n as u64;
        }
    }
    pages
}

/// Runs of consecutive pages inside one sector as (address, bytes).
fn sector_runs(pages: &BTreeMap<u32, [u8; PAGE_SIZE as usize]>, sector: u32) -> Vec<(u32, Vec<u8>)> {
    let mut runs: Vec<(u32, Vec<u8>)> = Vec::new();
    for (&page, data) in pages.range(sector..sector.saturating_add(SECTOR_SIZE)) {
        match runs.last_mut() {
            Some((a, d)) if *a + d.len() as u32 == page => d.extend_from_slice(data),
            _ => runs.push((page, data.to_vec())),
        }
    }
    runs
}

#[derive(Debug, Default, Clone)]
pub struct WriteStats {
    pub sectors: usize,
    pub unchanged: usize,
    pub erased: usize,
    pub bytes: usize,
    /// Bytes of UCB data in the image that were not written.
    pub ucb_skipped: usize,
    pub elapsed: Duration,
    /// Addresses of the sectors that were programmed.
    pub programmed: Vec<u32>,
}

impl std::fmt::Display for WriteStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} sectors: {} unchanged, {} programmed ({} erased first), {} bytes written in {:.1}s",
            self.sectors,
            self.unchanged,
            self.sectors - self.unchanged,
            self.erased,
            self.bytes,
            self.elapsed.as_secs_f64()
        )?;
        if self.ucb_skipped > 0 {
            write!(f, ", {} bytes of UCB data skipped", self.ucb_skipped)?;
        }
        Ok(())
    }
}

/// TC3xx UCB area (0xAF400000, 24 KB).
const UCB_START: u32 = 0xAF40_0000;
const UCB_END: u64 = 0xAF40_6000;

/// The image without its UCB data, and the number of bytes removed. UCBs are never written:
/// a wrong UCB can lock the device permanently. Without `unlock` the UCB data is skipped with a
/// warning, so a post-build HEX with UCB records still flashes; with `unlock` it is an error,
/// because UCB flashing is not implemented.
pub fn skip_ucbs(img: &Image, unlock: bool, what: &str) -> Result<(Image, usize)> {
    let (rest, n) = img.without(UCB_START, UCB_END);
    if n > 0 {
        if unlock {
            return Err(other(format!("image has {n} bytes of UCB data (0x{UCB_START:08x}..0x{UCB_END:08x}): UCB flashing is not supported")));
        }
        log_warn!("{what}: skipping {n} bytes of UCB data (0x{UCB_START:08x}..0x{UCB_END:08x}), UCB flashing is not unlocked");
    }
    Ok((rest, n))
}

/// Programs the image into the flash banks (`flash write_image`): erased sectors are
/// programmed, other touched sectors are erased first (an error without `erase`).
pub fn write_image(cx: &Cx, img: &Image, erase: bool) -> Result<WriteStats> {
    write_image_with(cx, img, erase, false, false)
}

/// Like [`write_image`]; with `incremental`, programmed sectors whose image pages already
/// hold the image data are skipped. That reads the image pages, which is only safe when
/// they are programmed: reading erased PFLASH raises an ECC error that the application's
/// SMU configuration may turn into a reset (seen on the TC377 board). `unlock`: see [`skip_ucbs`].
pub fn write_image_with(cx: &Cx, img: &Image, erase: bool, incremental: bool, unlock: bool) -> Result<WriteStats> {
    let (img, ucb_skipped) = skip_ucbs(img, unlock, "flash write_image")?;
    let img = &img;
    let banks = cx.with(|c| c.flash_banks.clone());
    for s in &img.sections {
        if !banks.iter().any(|b| b.contains(s.addr, s.end())) {
            return Err(other(format!("no flash bank for image data at 0x{:08x} ({} bytes)", s.addr, s.data.len())));
        }
    }
    halt_all(cx)?;
    let t0 = Instant::now();
    let mut stats = WriteStats { ucb_skipped, ..Default::default() };
    for bank in &banks {
        let hi = bank.base as u64 + bank.size as u64;
        if !img.touches(bank.base, hi) {
            continue;
        }
        let layout = probe_bank(cx, bank)?;
        if layout.dflash {
            return Err(other(format!("{}: writing DFLASH is not supported", bank.name)));
        }
        let pages = image_pages(img, bank.base, hi);
        let sectors: BTreeSet<u32> = pages.keys().map(|p| p - (p - bank.base) % SECTOR_SIZE).collect();
        log_info!("{}: checking {} sectors for {} bytes of image data", bank.name, sectors.len(), pages.len() * PAGE_SIZE as usize);
        for (i, &sector) in sectors.iter().enumerate() {
            let runs = sector_runs(&pages, sector);
            let (unchanged, erased, bytes) = cx.with(|c| -> Result<(bool, bool, usize)> {
                let mut io = c.io()?;
                let mut f = Driver::new(&mut io, layout);
                let blank = f.is_erased(sector, 1)?;
                if !blank {
                    if incremental {
                        // Assumes the image pages are programmed (see write_image_with).
                        let same = runs.iter().all(|(a, d)| f.io.read(seg_a(*a), d.len()).is_ok_and(|r| r == *d));
                        if same {
                            return Ok((true, false, 0));
                        }
                    }
                    if !erase {
                        return Err(other(format!("sector 0x{sector:08x} is not erased (use 'flash write_image erase')")));
                    }
                    f.erase(sector, 1)?;
                }
                let mut bytes = 0;
                for (a, d) in &runs {
                    f.program(*a, d)?;
                    bytes += d.len();
                }
                Ok((false, !blank, bytes))
            })?;
            stats.sectors += 1;
            stats.unchanged += unchanged as usize;
            stats.erased += erased as usize;
            stats.bytes += bytes;
            if !unchanged {
                stats.programmed.push(sector);
            }
            if (i + 1) % 16 == 0 || i + 1 == sectors.len() {
                log_info!("{}: {}/{} sectors done", bank.name, i + 1, sectors.len());
            }
        }
    }
    stats.elapsed = t0.elapsed();
    Ok(stats)
}

/// Compares the image with target memory (`verify_image`). Returns the number
/// of bytes compared, or the first difference as an error. UCB data is not compared,
/// since write_image does not write it.
pub fn verify_image(cx: &Cx, img: &Image) -> Result<usize> {
    let (img, _) = skip_ucbs(img, false, "verify_image")?;
    let img = &img;
    let mut total = 0;
    let mut diffs = 0usize;
    let mut first: Option<(u32, u8, u8)> = None;
    for s in &img.sections {
        for (k, chunk) in s.data.chunks(VERIFY_CHUNK).enumerate() {
            let addr = s.addr + (k * VERIFY_CHUNK) as u32;
            let read_at = if is_flash_address(addr) { seg_a(addr) } else { addr };
            let got = ops::read_memory(cx, read_at, chunk.len())
                .map_err(|e| other(format!("verify failed: cannot read 0x{addr:08x} ({} bytes): {e}", chunk.len())))?;
            for (i, (want, have)) in chunk.iter().zip(&got).enumerate() {
                if want != have {
                    diffs += 1;
                    first.get_or_insert((addr + i as u32, *want, *have));
                }
            }
            total += chunk.len();
        }
    }
    match first {
        None => Ok(total),
        Some((a, want, have)) => Err(other(format!(
            "verify failed: {diffs} of {total} bytes differ, first at 0x{a:08x} (image 0x{want:02x}, target 0x{have:02x})"
        ))),
    }
}

/// Sector erase check of a whole bank: (sector address, erased).
pub fn erase_check(cx: &Cx, bank: &FlashBank) -> Result<Vec<(u32, bool)>> {
    let layout = probe_bank(cx, bank)?;
    halt_all(cx)?;
    cx.with(|c| {
        let mut io = c.io()?;
        let mut f = Driver::new(&mut io, layout);
        let n = layout.size / layout.sector_size;
        (0..n).map(|i| bank.base + i * layout.sector_size).map(|a| f.is_erased(a, 1).map(|e| (a, e))).collect()
    })
}

/// Erases sectors `first..=last` of a bank.
pub fn erase_sectors(cx: &Cx, bank: &FlashBank, first: u32, last: u32) -> Result<()> {
    let layout = probe_bank(cx, bank)?;
    let count = layout.size / layout.sector_size;
    if first > last || last >= count {
        return Err(usage(format!("invalid sector range {first}..{last} (bank has {count} sectors)")));
    }
    halt_all(cx)?;
    cx.with(|c| {
        let mut io = c.io()?;
        Driver::new(&mut io, layout).erase(bank.base + first * layout.sector_size, last - first + 1)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::Section;

    #[test]
    fn pages_and_runs() {
        let img = Image::from_sections(vec![
            Section { addr: 0x8012_0404, data: vec![1; 40] },
            Section { addr: 0x8012_4000, data: vec![2; 3] },
        ]);
        let pages = image_pages(&img, 0x8000_0000, 0x8030_0000);
        assert_eq!(pages.keys().copied().collect::<Vec<_>>(), vec![0x8012_0400, 0x8012_0420, 0x8012_4000]);
        assert_eq!(&pages[&0x8012_0400][..6], &[PAD, PAD, PAD, PAD, 1, 1]);
        let runs = sector_runs(&pages, 0x8012_0000);
        assert_eq!(runs.len(), 1);
        assert_eq!((runs[0].0, runs[0].1.len()), (0x8012_0400, 64));
        assert_eq!(sector_runs(&pages, 0x8012_4000)[0].1[..4], [2, 2, 2, PAD]);
    }

    #[test]
    fn layout_and_chunks() {
        let b = pflash_banks(7)[1];
        assert_eq!((b.base, b.busy_bit), (0x8030_0000, 3));
        let mut sim = crate::sim::SimAdapter::new();
        let mut io = Io::new(&mut sim);
        let d = Driver::new(&mut io, b);
        // 1 MB physical sectors: at most 32 logical sectors, never across 0x80400000.
        assert_eq!(d.chunk(0x8030_0000, 100), 32);
        assert_eq!(d.chunk(0x803F_C000, 100), 1);
        assert_eq!(seg_a(0x8012_0400), 0xA012_0400);
        // DFLASH0: 4 KB sectors, at most 64 per command.
        let df = Driver::new(d.io, DF0);
        assert_eq!((df.chunk(0xAF00_0000, 300), df.chunk(0xAF0F_F000, 300)), (64, 1));
        assert_eq!(err_text(ERR_PROER | ERR_PVER), "protection error, program verify error");
    }
}
