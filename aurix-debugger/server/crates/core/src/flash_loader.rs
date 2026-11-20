//! RAM flash loader: CPU0 runs the PFLASH command sequences from its own RAM while the debugger
//! only moves data, so a probe pays one memory access per 8 KB instead of ~40 per 256 bytes.
//!
//! The loader ([`LOADER`], source in `flash_loader.s`) goes to the start of CPU0's PSPR, its
//! control block and two 8 KB slots to the start of CPU0's DSPR. CPU0 runs it alone: it leaves
//! trigger line 1 (which holds all cores while they are halted) for the time of the run, the other
//! cores stay halted. The loader turns off the CPU0 and safety watchdogs (they run again once the
//! core does, and a reset mid-erase would leave a half-erased sector), so they stay off until the
//! next reset. A CPU's PFLASH erase and program commands also need Safety ENDINIT cleared (the
//! debugger's do not): the loader clears it for their command cycles only, as the iLLD does. Afterwards CPU0 is halted and gets its registers back; the RAM keeps the loader.
//! The flash is changed under the application anyway: a reset follows every flash download.
//!
//! The debugger fills one slot while the loader works on the other: commands run in order, and
//! the debugger only waits when it needs a result (the blank check) or a slot to reuse. A loader
//! that does not start (no CPU0, RAM not writable, watchdogs that cannot be turned off) leaves
//! the programming to the DMU sequences over the debug interface ([`crate::flash::Driver`]).

use std::time::{Duration, Instant};

use crate::arch::{DBGSR, DBGSR_HALTED, DBGSR_HALT_REQ, DBGSR_RESUME, DBGSR_SUSP, ICR, PC, PSW, REGS, SYSCON};
use crate::error::{other, Error, Result};
use crate::flash::{err_text, seg_a, Pflash, ERR_EVER, ERR_MASK};
use crate::target::Core;
use crate::{log_info, log_warn, Cx};

/// The loader, assembled from `flash_loader.s` with Green Hills `astri -nooptimize` (32-bit
/// encodings only, which is what the simulator executes).
#[rustfmt::skip]
pub const LOADER: [u32; 263] = [
    0x0340000D, 0xAF003091, 0x960CAAD9, 0x01C2005D, 0x01E4005D, 0x01EA005D,
    0xAF003091, 0xA628AAD9, 0x01B8005D, 0x01DA005D, 0x01E0005D, 0xFF003091,
    0x3000107B, 0x9614F119, 0xA630F219, 0x0080210F, 0x8010306F, 0x3FFFF31B,
    0xFFF603DF, 0x09044189, 0x09084289, 0x0E05707B, 0x09004089, 0x0000001D,
    0x09044189, 0x09084289, 0x8FC0404D, 0x801F888F, 0x090C4889, 0x04C4407B,
    0x0523101B, 0x09004089, 0xCAF00091, 0xDF804091, 0xEAF01091, 0x9A90EED9,
    0xFAF00091, 0x7570FFD9, 0x70005001, 0x0008005D, 0x70006001, 0x0004005D,
    0xFFF8FF1D, 0x3000B001, 0x8FC0404D, 0x091C7009, 0xFFFE10DF, 0x0A1C4849,
    0x015A005D, 0x09007109, 0x09047209, 0x09087309, 0x090C7609, 0x0A207249,
    0x000FA03B, 0x5554C059, 0x0000003B, 0x003211DF, 0x000A21DF, 0x008031DF,
    0x00CE41DF, 0x0010003B, 0x0136001D, 0x014A005D, 0x0900E289, 0x0908E389,
    0x0008003B, 0x1918E089, 0x0005003B, 0x1918E089, 0x0480000D, 0x0166005D,
    0x8FC0404D, 0x0910D009, 0x0080600F, 0xFFFC00DF, 0x0A104849, 0x0120005D,
    0x0934D009, 0x0107F08F, 0x811200DF, 0x0102001D, 0x010003DF, 0x0005003B,
    0x5554C059, 0x0480000D, 0x0910D009, 0x0080600F, 0xFFFC00DF, 0x5002003B,
    0x700AA03B, 0x410FF28F, 0x800A04DF, 0x4010003B, 0x8006433F, 0x5010003B,
    0x700A603B, 0x41F0500B, 0x01482809, 0x0940F889, 0x4FFF841B, 0xFFFA04DF,
    0x00FC005D, 0x0900E289, 0x0000003B, 0x0908E089, 0x000A003B, 0x1918E089,
    0x1918E789, 0x0480000D, 0x0118005D, 0x8FC0404D, 0x0910D009, 0x0080600F,
    0xFFFC00DF, 0x0A144849, 0x00D2005D, 0x0934D009, 0x0107F08F, 0x80C400DF,
    0x2000520B, 0x3080530B, 0xFFB0FF1D, 0x09107509, 0x7000003B, 0xA000003B,
    0xB002003B, 0x004203DF, 0x0900E289, 0x0000103B, 0x0908E089, 0x0008003B,
    0x1918E089, 0x0005F03B, 0x1918E089, 0x0480000D, 0x8FC0404D, 0x0910D009,
    0x0080600F, 0xFFFC00DF, 0x0A184849, 0x00A0005D, 0x0934D009, 0x0107F08F,
    0x1106F08F, 0x809001DF, 0x000C406F, 0x1000103B, 0x1000A10F, 0x70A0170F,
    0x100FA03B, 0x5554C159, 0xA0001A1B, 0x2000520B, 0x3FFFF31B, 0xFFC8BA5F,
    0x01042789, 0x7000003B, 0xA000003B, 0xFFC0FF1D, 0x09002789, 0x0000003B,
    0x0060001D, 0x005C03DF, 0x09002209, 0x09042409, 0x86302001, 0xA000003B,
    0xB000003B, 0x004604DF, 0x7000003B, 0x5000803B, 0x01048009, 0x70A0070F,
    0xA0000A0B, 0xB000AB0B, 0x5FFFF51B, 0xFFF605DF, 0x802E07DF, 0x0900E289,
    0x0000003B, 0x0908E089, 0x0008003B, 0x1918E089, 0x0005603B, 0x1918E089,
    0x0480000D, 0x0910D009, 0x0080600F, 0xFFFC00DF, 0x0934D009, 0x0107F08F,
    0x1106F08F, 0x803001DF, 0x800A406F, 0x19E3707B, 0x179B911B, 0xB0001B0B,
    0x0006001D, 0x100FA03B, 0x5554C159, 0x2002021B, 0x4FFE041B, 0xFFBCFF1D,
    0x09002A89, 0x09042B89, 0x0A082249, 0x3FFFF31B, 0xFFA6FF1D, 0x0000003B,
    0x09147089, 0x8FC0404D, 0x801F888F, 0x090C4889, 0x0000203B, 0x091C7089,
    0x0030032D, 0x09147089, 0x0000303B, 0x091C7089, 0x0000001D, 0x9FC0404D,
    0x9080890B, 0x901F898F, 0x09008809, 0x8000980B, 0x09008889, 0x00300B2D,
    0x0900AC09, 0xD16E0C37, 0xD183FD8F, 0xD0002D8F, 0xEFFFF07B, 0xE080EC0F,
    0xE0A0DE0F, 0x00061C6F, 0xF1401E8F, 0x0900AF89, 0xF1402E8F, 0x0900AF89,
    0xD100003B, 0x0900AC09, 0x00060C6F, 0xDFFFFD1B, 0xFFFA0DDF, 0x00300B2D,
    0x0904AC09, 0xC1408C8F, 0x0904AC89, 0x00300B2D, 0x0900AC09, 0xD16E0C37,
    0xD183FD8F, 0xD0002D8F, 0xEFFFF07B, 0xE080EC0F, 0xE0A0DE0F, 0xF1401E8F,
    0x0900AF89, 0xF1403E8F, 0x0900AF89, 0x0900AC09, 0x00300B2D,
];

/// Where the loader runs: the start of CPU0's PSPR (global address).
pub const CODE_ADDR: u32 = 0x7010_0000;
/// Where the control block and the two slots go (global addresses), and the slot's data size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub ctrl: u32,
    pub slots: [u32; 2],
    pub data: usize,
}

/// The places tried, in order: CPU0's DSPR (32 KB slots: four 8 KB pieces, which a BlueBox takes
/// in one message), else its PSPR behind the code (24 KB slots, 64 KB in all). An application may
/// protect its DSPR against the debugger, and such writes are dropped without an error (seen on a
/// TC375 Lite Kit), so each place is checked by reading a pattern back.
pub const LAYOUTS: [Layout; 2] = [
    Layout { ctrl: 0x7000_0000, slots: [0x7000_0020, 0x7000_8040], data: 0x8000 },
    Layout { ctrl: CODE_ADDR + 0x800, slots: [CODE_ADDR + 0x840, CODE_ADDR + 0x6860], data: 0x6000 },
];
/// Slot header offsets (the header comes first, the data after it).
const H_RESULT: u32 = 0x14;
const H_STATE: u32 = 0x1C;
const H_DATA: u32 = 0x20;

const STATUS_READY: u32 = 0x4C44_5231;
const STATUS_WDT_FAIL: u32 = 0xE057_0000;

const CMD_PROGRAM: u32 = 1;
const CMD_ERASE: u32 = 2;
const CMD_BLANK: u32 = 3;
const CMD_CHECKSUM: u32 = 4;

const STATE_IDLE: u32 = 0;
const STATE_READY: u32 = 1;
const STATE_DONE: u32 = 2;
const STATE_FAILED: u32 = 3;

/// TL1 suspend targets: CPU0 is bit 0.
use crate::tl1::CBS_TL1ST;
/// Supervisor mode, call depth counting as after a reset.
const LOADER_PSW: u32 = 0x0000_0B80;
const ICR_IE: u32 = 1 << 15;

const START_TIMEOUT: Duration = Duration::from_millis(500);
const HALT_TIMEOUT: Duration = Duration::from_millis(200);
const ERASE_TIMEOUT: Duration = Duration::from_secs(10);
const PROGRAM_TIMEOUT: Duration = Duration::from_secs(2);

/// Registers the loader changes, restored afterwards: d0-d15, a2-a15, PSW, PC, ICR, SYSCON,
/// CCTRL.
fn saved_regs() -> Vec<u16> {
    let gp = (0..16).chain(18..32).map(|i| REGS[i].offset);
    gp.chain([PSW, PC, ICR, SYSCON, CCTRL]).collect()
}

/// SYSCON.PROTEN and TPROTEN: the memory protection an application may have turned on. Its code
/// ranges hardly cover CPU0's PSPR at its global address, so the loader runs without it.
const SYSCON_PROTEN: u32 = 0x6;

/// CPUx_FLASHCON1 (offset from the core's debug base): MASKUECC (bits 17:16) = 01 masks the
/// uncorrectable ECC errors of the core's local PFLASH bank for every master, so a CPU load of
/// erased PFLASH raises no DIE trap (DIETR.IED, which would inhibit it, only hardware can set).
/// The debugger may write it (measured on a TC375); a core without a local bank ignores it.
const FLASHCON1: u32 = 0x1104;
const MASKUECC: u32 = 3 << 16;
const MASKUECC_MASKED: u32 = 1 << 16;

/// CPU clock counter control (CE: count) and the counter; the loader times its waits with CCNT.
const CCTRL: u16 = 0xFC00;
const CCTRL_CE: u32 = 1 << 1;
/// Control block words of the timing (CCNT / 256): the last timestamp, then the waits for erases,
/// programs, blank checks and the debugger.
const T_STAMP: u32 = 0x0C;
const T_WAITS: u32 = 0x10;

/// Where the loader's time went, for the log: its own CCNT waits, calibrated against the host
/// clock, and the host's time writing the slots.
#[derive(Default)]
struct Timing {
    /// Host time and CCNT / 256 when the loader reported ready.
    start: Option<(Instant, u32)>,
    /// Host time spent writing slots, and the bytes written.
    write: Duration,
    bytes: usize,
    /// Host time when the last command was seen done: the loader's last timestamp is from then.
    last_done: Option<Instant>,
}

/// Whether the debugger's writes reach `l`'s RAM: a pattern in the control block, both slot
/// headers and the last word of each slot reads back. Leaves the control block and the slot states
/// zero.
fn writable(io: &mut crate::adapter::Io, l: &Layout) -> bool {
    let places = [(l.ctrl, 0x20), (l.slots[0], 0x20), (l.slots[1], 0x20), (l.slots[0] + H_DATA + l.data as u32 - 4, 4), (l.slots[1] + H_DATA + l.data as u32 - 4, 4)];
    let pattern = |a: u32, n: usize| (0..n as u32).map(|i| (a.wrapping_add(i).wrapping_mul(0x9E37_79B1) >> 24) as u8).collect::<Vec<u8>>();
    let ok = places.iter().all(|&(a, n)| io.write(a, &pattern(a, n)).is_ok()) && places.iter().all(|&(a, n)| io.read(a, n).is_ok_and(|r| r == pattern(a, n)));
    ok && io.write(l.ctrl, &[0; 0x20]).is_ok() && io.write32_seq(&[(l.slots[0] + H_STATE, STATE_IDLE), (l.slots[1] + H_STATE, STATE_IDLE)]).is_ok()
}

/// What a slot was given, for the error message and the timeout.
#[derive(Clone, Copy, Debug)]
struct Job {
    cmd: u32,
    addr: u32,
    timeout: Duration,
}

/// The loader's sums of `data` (32-bit little-endian words): per word w, A += w, B += A.
pub fn checksum(data: &[u8]) -> (u32, u32) {
    let (mut a, mut b) = (0u32, 0u32);
    for w in data.chunks_exact(4) {
        a = a.wrapping_add(u32::from_le_bytes(w.try_into().unwrap()));
        b = b.wrapping_add(a);
    }
    (a, b)
}

impl Job {
    fn what(&self) -> &'static str {
        match self.cmd {
            CMD_PROGRAM => "program",
            CMD_ERASE => "erase",
            CMD_CHECKSUM => "checksum",
            _ => "erase check",
        }
    }
}

/// The running loader on CPU0 (target `t`). [`Loader::stop`] must be called in every case.
pub struct Loader {
    t: usize,
    saved: Vec<(u32, u32)>,
    tl1st: Option<u32>,
    dbgsr: u32,
    next: usize,
    jobs: [Option<Job>; 2],
    pub commands: usize,
    layout: Layout,
    timing: Timing,
    /// FLASHCON1.MASKUECC took on every core: the checksum command may read erased PFLASH.
    checksums: bool,
    /// Log where CPU0 was when it is halted (the loader did not start).
    report_pc: bool,
}

impl Loader {
    /// Starts the loader on CPU0, which must be halted (as after [`crate::flash::halt_all`]).
    /// `Ok(None)`: no loader here (no CPU0, or it did not start): the caller programs through the
    /// DMU directly. The reason is logged.
    pub fn start(cx: &Cx) -> Result<Option<Loader>> {
        let Some((t, bases)) = cx.with(|c| {
            if c.adapter.as_deref_mut().is_some_and(|a| a.run_control().is_some()) {
                return None;
            }
            let t = c.targets.iter().position(|e| matches!(&e.core, Core::Tricore(tc) if tc.coreid == 0 && tc.examined))?;
            let bases: Vec<u32> = c.targets.iter().filter_map(|e| e.core.as_tricore()).filter(|tc| tc.examined).filter_map(|tc| tc.dbgbase).collect();
            Some((t, bases))
        }) else {
            log_info!("flash loader: no examined cpu0 to run it on, programming through the debug interface");
            return Ok(None);
        };
        let r = cx.with(|c| -> Result<Loader> {
            let (mut io, e) = c.target_io(t)?;
            let Core::Tricore(tc) = &mut e.core else { unreachable!() };
            let csfr: Vec<u32> = saved_regs().into_iter().map(|o| tc.csfr(o)).collect();
            let dbgsr_addr = tc.csfr(DBGSR);
            let dbgsr = io.read32(dbgsr_addr)?;
            if dbgsr & DBGSR_HALTED == 0 {
                return Err(other("cpu0 is not halted"));
            }
            let vals = io.read32_many(&csfr)?;
            let mut saved: Vec<(u32, u32)> = csfr.iter().copied().zip(vals).collect();
            let code: Vec<u8> = LOADER.iter().flat_map(|w| w.to_le_bytes()).collect();
            io.write(CODE_ADDR, &code)?;
            if io.read(CODE_ADDR, code.len())? != code {
                return Err(other(format!("the loader did not read back from 0x{CODE_ADDR:08x}")));
            }
            // Status and both slot states cleared, before the core can see them.
            let layout = LAYOUTS
                .iter()
                .copied()
                .find(|l| writable(&mut io, l))
                .ok_or_else(|| other("the debugger's writes do not reach cpu0's DSPR or PSPR (protected by the application?)"))?;
            if layout != LAYOUTS[0] {
                log_info!("flash loader: cpu0's DSPR does not take the debugger's writes, using its PSPR (slots of {} KB)", layout.data / 1024);
            }
            let old = |o: u16| saved.iter().find(|(a, _)| *a == tc.csfr(o)).map_or(0, |s| s.1);
            io.write32_seq(&[
                (tc.csfr(REGS[20].offset), layout.ctrl), // a4
                (tc.csfr(REGS[21].offset), layout.slots[0]), // a5
                (tc.csfr(REGS[22].offset), layout.slots[1]), // a6
                (tc.csfr(PSW), LOADER_PSW),
                (tc.csfr(ICR), old(ICR) & !ICR_IE),
                (tc.csfr(PC), CODE_ADDR),
                (tc.csfr(SYSCON), old(SYSCON) & !SYSCON_PROTEN),
                (tc.csfr(CCTRL), old(CCTRL) | CCTRL_CE),
            ])?;
            // Uncorrectable ECC errors masked on every core's local PFLASH bank, for the checksums.
            let fc: Vec<u32> = bases.iter().map(|b| b + FLASHCON1).collect();
            let fc_old = io.read32_many(&fc)?;
            saved.extend(fc.iter().copied().zip(fc_old.iter().copied()));
            io.write32_seq(&fc.iter().zip(&fc_old).map(|(&a, &v)| (a, (v & !MASKUECC) | MASKUECC_MASKED)).collect::<Vec<_>>())?;
            let checksums = io.read32_many(&fc)?.iter().all(|v| v & MASKUECC == MASKUECC_MASKED);
            let tl1 = io.read32(CBS_TL1ST)?;
            let tl1st = (tl1 & 1 != 0).then_some(tl1);
            if tl1st.is_some() {
                io.write32(CBS_TL1ST, tl1 & !1)?;
            }
            tc.invalidate();
            io.write32(dbgsr_addr, DBGSR_RESUME | (dbgsr & DBGSR_SUSP))?;
            Ok(Loader { t, saved, tl1st, dbgsr, next: 0, jobs: [None; 2], commands: 0, layout, timing: Timing::default(), checksums, report_pc: false })
        });
        let mut l = match r {
            Ok(l) => l,
            Err(e) => {
                log_warn!("flash loader: cannot start it on cpu0 ({e}), programming through the debug interface");
                return Ok(None);
            }
        };
        // The loader reports once the watchdogs are off.
        let t0 = Instant::now();
        let ctrl = l.layout.ctrl;
        let status = loop {
            let s = cx.with(|c| c.io().and_then(|mut io| io.read32_many(&[ctrl, ctrl + 4, ctrl + 8, ctrl + T_STAMP])));
            match s {
                Ok(v) if v[0] != 0 => break Ok(v),
                Ok(_) if t0.elapsed() < START_TIMEOUT => std::thread::sleep(Duration::from_millis(1)),
                Ok(_) => break Err(other("it did not report ready")),
                Err(e) => break Err(e),
            }
        };
        let why = match status {
            Ok(v) if v[0] == STATUS_READY => {
                l.timing.start = Some((Instant::now(), v[3]));
                log_info!("flash loader: running on cpu0 (CPU0 and safety watchdogs off until the next reset)");
                if !l.checksums {
                    log_warn!("flash loader: FLASHCON1.MASKUECC does not stick, incremental writes compare by reading the flash");
                }
                return Ok(Some(l));
            }
            Ok(v) if v[0] == STATUS_WDT_FAIL => {
                format!("the watchdogs did not turn off (WDTCPU0SR 0x{:08x}, WDTSSR 0x{:08x})", v[1], v[2])
            }
            Ok(v) => format!("unexpected status 0x{:08x}", v[0]),
            Err(e) => e.to_string(),
        };
        l.jobs = [None; 2];
        l.report_pc = true;
        if let Err(e) = l.stop(cx) {
            return Err(other(format!("flash loader: {why}, and stopping it failed: {e}")));
        }
        log_warn!("flash loader: {why}; programming through the debug interface");
        Ok(None)
    }

    /// Waits until slot `k` holds no unfinished command; fails on a failed one.
    fn wait_slot(&mut self, cx: &Cx, k: usize) -> Result<u32> {
        let Some(job) = self.jobs[k] else { return Ok(0) };
        let state_addr = self.layout.slots[k] + H_STATE;
        let t0 = Instant::now();
        let mut polls = 0u32;
        loop {
            let v = cx.with(|c| c.io().and_then(|mut io| io.read32_many(&[state_addr, self.layout.slots[k] + H_RESULT])))?;
            match v[0] {
                STATE_DONE => {
                    self.jobs[k] = None;
                    self.timing.last_done = Some(Instant::now());
                    return Ok(v[1]);
                }
                STATE_FAILED => {
                    self.jobs[k] = None;
                    let err = v[1];
                    let text = if err & !ERR_MASK != 0 { format!("loader error 0x{err:x}") } else { err_text(err) };
                    return Err(other(format!("flash {} at 0x{:08x} failed: {text}", job.what(), job.addr)));
                }
                STATE_READY => {}
                s => return Err(other(format!("flash loader: slot {k} in state {s}"))),
            }
            if t0.elapsed() > job.timeout {
                return Err(Error::Timeout(format!("flash {} at 0x{:08x} timed out (loader)", job.what(), job.addr)));
            }
            polls += 1;
            if polls > 4 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }

    /// Puts a command into the next slot (after its previous one has finished).
    fn submit(&mut self, cx: &Cx, job: Job, count: u32, busy_mask: u32, step: u32, data: &[u8]) -> Result<usize> {
        let k = self.next;
        self.wait_slot(cx, k)?;
        let base = self.layout.slots[k];
        let mut header = Vec::with_capacity(0x20);
        for w in [job.cmd, seg_a(job.addr), count, busy_mask, step, 0, 0, STATE_READY] {
            header.extend_from_slice(&w.to_le_bytes());
        }
        // The data first, then the header: its last word, the state, is written last (a block is
        // written in address order).
        let mut blocks: Vec<(u32, &[u8])> = Vec::with_capacity(2);
        if !data.is_empty() {
            blocks.push((base + H_DATA, data));
        }
        blocks.push((base, &header));
        let t0 = Instant::now();
        let ok = cx.with(|c| c.adapter.as_deref_mut().ok_or_else(|| other("no adapter")).and_then(|a| a.write_bulk(&blocks)))?;
        self.timing.write += t0.elapsed();
        self.timing.bytes += data.len();
        if !ok {
            return Err(Error::Memory { addr: base as u64, write: true, detail: "flash loader slot write failed".into() });
        }
        self.jobs[k] = Some(job);
        self.next = 1 - k;
        self.commands += 1;
        Ok(k)
    }

    /// Waits for every command given so far.
    pub fn drain(&mut self, cx: &Cx) -> Result<()> {
        for k in [self.next, 1 - self.next] {
            self.wait_slot(cx, k)?;
        }
        Ok(())
    }

    /// Whether each of `n` sectors from `addr` is erased (DMU "verify erased", one per sector).
    pub fn blank_check(&mut self, cx: &Cx, bank: &Pflash, addr: u32, n: u32) -> Result<Vec<bool>> {
        let mut out = Vec::with_capacity(n as usize);
        let max = (self.layout.data * 8) as u32;
        let mut done = 0;
        while done < n {
            let m = (n - done).min(max);
            let a = addr + done * bank.sector_size;
            let job = Job { cmd: CMD_BLANK, addr: a, timeout: ERASE_TIMEOUT + Duration::from_millis(20) * m };
            let k = self.submit(cx, job, m, 1 << bank.busy_bit, bank.sector_size, &[])?;
            self.drain(cx)?;
            let bits = cx.with(|c| c.io().and_then(|mut io| io.read(self.layout.slots[k] + H_DATA, (m as usize).div_ceil(32) * 4)))?;
            out.extend((0..m as usize).map(|i| bits[i / 8] >> (i % 8) & 1 == 0));
            done += m;
        }
        Ok(out)
    }

    /// Bytes of data per slot: the most [`Loader::program`] takes.
    pub fn slot_data(&self) -> usize {
        self.layout.data
    }

    /// Checksum runs per command: (address, bytes) pairs in the slot's data.
    pub fn max_runs(&self) -> usize {
        self.layout.data / 8
    }

    /// Whether the checksum command is usable (FLASHCON1.MASKUECC took, see [`FLASHCON1`]).
    pub fn checksums(&self) -> bool {
        self.checksums
    }

    /// The sums ([`checksum`]) of PFLASH runs (address, bytes; each a multiple of 32 bytes, at
    /// most [`Loader::max_runs`]) as the loader reads them. A page that reads all zero but is erased
    /// spoils its run's sum, so erased flash never passes for zero data.
    pub fn checksum_runs(&mut self, cx: &Cx, bank: &Pflash, runs: &[(u32, u32)]) -> Result<Vec<(u32, u32)>> {
        if runs.len() > self.max_runs() || runs.iter().any(|&(a, n)| !a.is_multiple_of(32) || !n.is_multiple_of(32)) {
            return Err(other("flash loader: too many checksum runs or not page aligned"));
        }
        let bytes: u64 = runs.iter().map(|r| r.1 as u64).sum();
        let mut list = Vec::with_capacity(runs.len() * 8);
        for &(a, n) in runs {
            list.extend_from_slice(&seg_a(a).to_le_bytes());
            list.extend_from_slice(&n.to_le_bytes());
        }
        let timeout = PROGRAM_TIMEOUT + Duration::from_millis(bytes / 1024);
        let job = Job { cmd: CMD_CHECKSUM, addr: runs.first().map_or(0, |r| r.0), timeout };
        let k = self.submit(cx, job, runs.len() as u32, 1 << bank.busy_bit, 0, &list)?;
        self.drain(cx)?;
        let out = cx.with(|c| c.io().and_then(|mut io| io.read(self.layout.slots[k] + H_DATA, runs.len() * 8)))?;
        let word = |w: &[u8]| u32::from_le_bytes(w.try_into().unwrap());
        Ok(out.chunks_exact(8).map(|w| (word(&w[..4]), word(&w[4..]))).collect())
    }

    /// Erases `sectors` logical sectors from `addr` in one command (within the limits of
    /// [`crate::flash::erase_chunk`]). Does not wait.
    pub fn erase(&mut self, cx: &Cx, bank: &Pflash, addr: u32, sectors: u32) -> Result<()> {
        let job = Job { cmd: CMD_ERASE, addr, timeout: ERASE_TIMEOUT };
        self.submit(cx, job, sectors, 1 << bank.busy_bit, 0, &[]).map(|_| ())
    }

    /// Programs erased flash with page-aligned `data` (at most [`Loader::slot_data`] bytes). Does not
    /// wait.
    pub fn program(&mut self, cx: &Cx, bank: &Pflash, addr: u32, data: &[u8]) -> Result<()> {
        if !addr.is_multiple_of(32) || !data.len().is_multiple_of(32) || data.len() > self.layout.data {
            return Err(other("flash loader: data is not page aligned or too long"));
        }
        let job = Job { cmd: CMD_PROGRAM, addr, timeout: PROGRAM_TIMEOUT };
        self.submit(cx, job, data.len() as u32, 1 << bank.busy_bit, 0, data).map(|_| ())
    }

    /// Logs where the time went up to the last command: the loader's waits for the flash and for
    /// the debugger (CCNT, calibrated against the host clock), the rest of its time, and the host's
    /// time writing the slots. Only meaningful after [`Loader::drain`].
    fn log_timing(&self, cx: &Cx) {
        let (Some((t0, c0)), Some(t1)) = (self.timing.start, self.timing.last_done) else { return };
        let c = self.layout.ctrl;
        let Ok(v) = cx.with(|x| x.io().and_then(|mut io| io.read32_many(&[c + T_STAMP, c + T_WAITS, c + T_WAITS + 4, c + T_WAITS + 8, c + T_WAITS + 12]))) else {
            return;
        };
        // Calibrated up to the last command's end (the loader's last timestamp, give or take a poll).
        let (secs, ticks) = (t1.duration_since(t0).as_secs_f64(), v[0].wrapping_sub(c0));
        if ticks == 0 || secs < 0.02 {
            return;
        }
        let tick_s = secs / ticks as f64;
        let s = |t: u32| t as f64 * tick_s;
        let (erase, program, blank, idle) = (s(v[1]), s(v[2]), s(v[3]), s(v[4]));
        log_info!(
            "flash loader: {:.1} s on cpu0 at {:.0} MHz: erase {:.1} s, program {:.1} s, erase check {:.1} s, waiting for the debugger {:.1} s, the rest {:.1} s; the debugger wrote {} KB in {:.1} s ({:.0} KB/s)",
            secs,
            ticks as f64 * 256.0 / secs / 1e6,
            erase,
            program,
            blank,
            idle,
            secs - erase - program - blank - idle,
            self.timing.bytes / 1024,
            self.timing.write.as_secs_f64(),
            self.timing.bytes as f64 / 1024.0 / self.timing.write.as_secs_f64().max(1e-9),
        );
    }

    /// Halts CPU0 and gives it back its registers and its place on trigger line 1. A DMU left in
    /// page mode or with error flags is reset to read mode.
    pub fn stop(self, cx: &Cx) -> Result<()> {
        if self.jobs.iter().all(|j| j.is_none()) {
            self.log_timing(cx);
        }
        cx.with(|c| -> Result<()> {
            let (mut io, e) = c.target_io(self.t)?;
            let Core::Tricore(tc) = &mut e.core else { unreachable!() };
            let dbgsr_addr = tc.csfr(DBGSR);
            io.write32(dbgsr_addr, DBGSR_HALT_REQ | (self.dbgsr & DBGSR_SUSP))?;
            let t0 = Instant::now();
            let halted = loop {
                if io.read32(dbgsr_addr)? & DBGSR_HALTED != 0 {
                    break true;
                }
                if t0.elapsed() > HALT_TIMEOUT {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(1));
            };
            if let Some(v) = self.tl1st {
                io.write32(CBS_TL1ST, v)?;
            }
            if !halted {
                return Err(other("cpu0 did not halt after the flash loader"));
            }
            let err = io.read32(crate::flash::DMU_HF_ERRSR)? & ERR_MASK;
            if err & !ERR_EVER != 0 {
                io.write32(crate::flash::KEY, crate::flash::KEY_RESET_TO_READ)?;
            }
            if self.report_pc || self.jobs.iter().any(|j| j.is_some()) {
                let v = io.read32_many(&[tc.csfr(PC), tc.csfr(PSW), tc.csfr(SYSCON), self.layout.ctrl])?;
                log_warn!(
                    "flash loader: cpu0 halted at 0x{:08x} (loader at 0x{CODE_ADDR:08x}..0x{:08x}), PSW 0x{:08x}, SYSCON 0x{:08x}, status 0x{:08x}",
                    v[0],
                    CODE_ADDR + 4 * LOADER.len() as u32,
                    v[1],
                    v[2],
                    v[3]
                );
            }
            io.write32_seq(&self.saved)?;
            tc.invalidate();
            Ok(())
        })
    }
}
