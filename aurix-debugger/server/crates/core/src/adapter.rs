//! Debug adapter abstraction (OpenOCD `adapter driver`).

use std::any::Any;

use tas::{Batch, OpResult};

use crate::cortex_m::word_only;
use crate::error::{Error, Result};

#[derive(Debug, Clone, Default)]
pub struct AdapterSettings {
    pub speed_khz: Option<u32>,
    pub serial: Option<String>,
    /// `reset_config connect_assert_srst`: connect with reset and halt.
    pub connect_under_srst: bool,
    /// `adapter debug_password`: the device's 256-bit debug password (TC3xx `UCB_DBG` PW0..PW7),
    /// for a device whose debug interface is locked. The start-up software takes it only in the
    /// window right after a reset, so such a device also needs `connect_under_srst`. Never logged.
    pub debug_password: Option<[u32; 8]>,
}

#[derive(Debug, Clone, Default)]
pub struct DeviceInfo {
    /// JTAG ID of the device (OCMTS TAP ID).
    pub device_id: u32,
    pub description: String,
}

/// What one request/response packet of the connection can carry. The DAQ sampler uses it to bound
/// how many round trips a single tick may cost.
#[derive(Debug, Clone, Copy)]
pub struct BatchLimits {
    /// Read/write transactions per packet.
    pub max_num_rw: usize,
    pub max_rq: usize,
    pub max_rsp: usize,
}

impl Default for BatchLimits {
    fn default() -> Self {
        BatchLimits { max_num_rw: 16, max_rq: 0x1000, max_rsp: 0x1000 }
    }
}

/// Why the target, or the probe, is not usable, as far as the probe can tell. Probes report
/// different parts of it: a BlueBox measures the target supply and latches resets, a miniWiggler
/// (TasServer) only knows whether it sees a device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkHealth {
    Ok,
    /// The target supply is missing or outside the probe's limits.
    Unpowered { vref_mv: u32 },
    /// The target reset since the probe last connected, and the debug connection is down (a
    /// BlueBox latches the reset until it connects again).
    Reset,
    /// The target is supplied, the debug connection is down.
    NoDebugAccess,
    /// The access hardware sees no device (miniWiggler: not powered, not connected).
    NoDevice,
    /// The probe itself is gone: unplugged, powered off, or its server (TasServer) stopped.
    /// The core ends the frontends' sessions when this lasts (`probe_lost_timeout`).
    ProbeLost { why: &'static str },
}

impl LinkHealth {
    /// Short name for frontends: `ok`, `unpowered`, `reset`, `noDebugAccess`, `noDevice`, `probeLost`.
    pub fn state(&self) -> &'static str {
        match self {
            LinkHealth::Ok => "ok",
            LinkHealth::Unpowered { .. } => "unpowered",
            LinkHealth::Reset => "reset",
            LinkHealth::NoDebugAccess => "noDebugAccess",
            LinkHealth::NoDevice => "noDevice",
            LinkHealth::ProbeLost { .. } => "probeLost",
        }
    }

    pub fn text(&self) -> String {
        match self {
            LinkHealth::Ok => "target connected".into(),
            LinkHealth::Unpowered { vref_mv } => format!("target not powered (Vref {vref_mv} mV)"),
            LinkHealth::Reset => "target reset, reconnecting".into(),
            LinkHealth::NoDebugAccess => "no debug access to the target".into(),
            LinkHealth::NoDevice => "the probe sees no device (target not powered or not connected?)".into(),
            LinkHealth::ProbeLost { why } => format!("debug probe lost: {why}"),
        }
    }

    pub fn vref_mv(&self) -> Option<u32> {
        match self {
            LinkHealth::Unpowered { vref_mv } => Some(*vref_mv),
            _ => None,
        }
    }
}

/// What a probe's sampler can take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeDaqLimits {
    pub max_items: usize,
    /// Largest item, in bytes; items are 1, 2, 4 or 8 bytes, naturally aligned.
    pub max_item_size: u8,
}

/// One address the probe samples, with the period its point asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeItem {
    pub addr: u32,
    pub size: u8,
    pub period_us: u32,
}

/// A value the probe read: `item` indexes the list last passed to [`ProbeDaq::configure`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeSample {
    pub item: usize,
    /// The probe's clock, in microseconds.
    pub time_us: u64,
    pub data: Vec<u8>,
}

/// What one drain brought.
#[derive(Debug, Clone, Default)]
pub struct ProbeDrain {
    /// The probe's clock when it answered, to place `samples` on the host's timeline.
    pub time_us: u64,
    pub samples: Vec<ProbeSample>,
    /// Samples were lost because the probe's buffer was full.
    pub overflow: bool,
    /// The reply was full: more samples are waiting.
    pub more: bool,
}

/// A sampler in the probe: it checks a list of bus addresses by itself and buffers what it read,
/// so the host only drains the buffer instead of reading each point.
///
/// Semantics the registry relies on (measured on the BlueBox, see `bluebox::daq`):
/// * after [`ProbeDaq::configure`], every readable item delivers one sample, and afterwards a
///   sample whenever its value changed;
/// * an unreadable item delivers nothing and reports no error.
pub trait ProbeDaq {
    fn limits(&self) -> ProbeDaqLimits;
    /// How often the probe checks an item whose point asked for `period_us`: never less often
    /// than asked. 0 means "as fast as it can".
    fn check_period_us(&self, period_us: u32) -> u32;
    /// Replaces the item list and starts sampling; an empty list stops it.
    fn configure(&mut self, items: &[ProbeItem]) -> Result<()>;
    /// Takes what the probe buffered since the last drain.
    fn drain(&mut self) -> Result<ProbeDrain>;
    /// Changes whenever the connection to the probe or the target was rebuilt, which may have
    /// dropped the item list: the registry configures again.
    fn generation(&self) -> u64;
}

/// Why the SCR stopped, as its debug interface reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrStop {
    /// A halt request (also the start breakpoint the boot ROM sets at 0x0000 in OCDS mode).
    Request,
    /// A hardware breakpoint.
    Breakpoint,
    /// A single step finished.
    Step,
    /// Anything else (e.g. a `TRAP` instruction).
    Other,
}

/// Run state of the SCR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrStatus {
    pub halted: bool,
    /// The address the SCR continues at; valid while halted.
    pub pc: u16,
    pub reason: ScrStop,
}

/// The SCR's registers as its debug monitor saved them on entry. While the SCR is halted, the
/// monitor uses ACC, B, SP, PSW, DPTR and register bank 0 itself: reading them as SFRs or IRAM
/// shows the monitor's values (measured through the BlueBox), so they come from here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScrRegs {
    pub acc: u8,
    pub b: u8,
    pub sp: u8,
    pub psw: u8,
    pub dpl: u8,
    pub dph: u8,
    pub pc: u16,
    /// R0..R7 of the bank PSW.RS selects.
    pub r: [u8; 8],
}

/// One of the [`ScrRegs`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrReg {
    Acc,
    B,
    Sp,
    Psw,
    Dpl,
    Dph,
    Pc,
    /// R0..R7 of the selected bank.
    R(u8),
}

/// Start of each space in the SCR's virtual address map (TC3xx UM 49.5.2.3.2, Table 558), the
/// addresses [`ScrDebug::read`] and [`ScrDebug::write`] take.
pub mod scr_space {
    /// Program memory (XRAM as code), 0x0000-0xFFFF.
    pub const CODE: u32 = 0x0_0000;
    /// External data memory (XRAM as data).
    pub const XDATA: u32 = 0x1_0000;
    /// Internal RAM, 0x00-0xFF (R0..R7 of bank n at 8n; see [`super::ScrRegs`] while halted).
    pub const IRAM: u32 = 0x2_0000;
    /// SFRs: bit 18 set, page in bits 17:15, SFR page address in bits 14:8, RMAP in bit 7,
    /// SFR address (without its bit 7) in bits 6:0.
    pub const SFR: u32 = 0x4_0000;
    /// End of the map.
    pub const END: u32 = 0x8_0000;

    /// Virtual address of a standard SFR (`addr` 0x80..0xFF, page 0, RMAP 0).
    pub const fn sfr(addr: u8) -> u32 {
        SFR | (addr as u32 & 0x7F)
    }
}

/// Debugging the TC3xx standby controller (SCR, an XC800/8051 core) through the probe.
///
/// The SCR is not reachable over the host bus: its OCDS sits behind its own debug port, which
/// the SoC DAP reaches once the application started the SCR in an OCDS boot mode (PMSWCR4.SCRCFG
/// 0x0A/0x0B). How a probe talks to it differs -- the BlueBox has services for it, a TAS
/// miniWiggler would drive the SCR's JTAG and its boot ROM monitor directly -- so the SCR target
/// ([`crate::xc800`]) asks the adapter for this interface instead of reading registers itself.
///
/// The primitives are the ones the SCR's debug monitor offers (TC3xx UM 49.5.2): halt, exit to
/// the user program, single step, hardware breakpoints, and reads and writes in its virtual
/// address map ([`scr_space`]). Splitting into the probe's block sizes is the implementation's
/// job.
pub trait ScrDebug {
    /// Establishes debug access to the SCR (it must run in an OCDS boot mode).
    fn attach(&mut self) -> Result<()>;
    /// Gives the SCR back to its program; the default does nothing.
    fn detach(&mut self) -> Result<()> {
        Ok(())
    }
    fn status(&mut self) -> Result<ScrStatus>;
    fn halt(&mut self) -> Result<()>;
    /// Continues the user program at the halted PC.
    fn resume(&mut self) -> Result<()>;
    /// Executes one instruction; the SCR is halted afterwards.
    fn step(&mut self) -> Result<()>;
    /// Registers saved by the monitor; only while halted.
    fn read_regs(&mut self) -> Result<ScrRegs>;
    /// Changes a saved register (the PC: where the program continues); only while halted.
    fn write_reg(&mut self, reg: ScrReg, value: u16) -> Result<()>;
    /// Number of hardware breakpoints.
    fn hw_breakpoint_slots(&self) -> usize;
    fn set_hw_breakpoint(&mut self, slot: usize, addr: u16) -> Result<()>;
    fn clear_hw_breakpoint(&mut self, slot: usize) -> Result<()>;
    /// Reads `len` bytes at a virtual address ([`scr_space`]).
    fn read(&mut self, vaddr: u32, len: usize) -> Result<Vec<u8>>;
    /// Writes at a virtual address ([`scr_space`]).
    fn write(&mut self, vaddr: u32, data: &[u8]) -> Result<()>;
}

pub trait Adapter {
    fn driver_name(&self) -> &'static str;
    /// Driver-specific Tcl command groups, e.g. `tas-client`.
    fn command_groups(&self) -> &'static [&'static str] {
        &[]
    }
    /// Handles a driver-specific command (`args[0]` is the group name).
    fn command(&mut self, args: &[&str]) -> Result<String> {
        Err(Error::Usage(format!("unknown command '{}'", args.join(" "))))
    }
    fn init(&mut self, settings: &AdapterSettings) -> Result<DeviceInfo>;
    fn initialized(&self) -> bool;
    fn execute(&mut self, batch: &Batch) -> Result<Vec<OpResult>>;
    /// Packet limits of the connection; the default is a conservative guess.
    fn limits(&self) -> BatchLimits {
        BatchLimits::default()
    }
    fn assert_srst(&mut self) -> Result<()>;
    /// Releases reset. `halt_after_reset` requests a halt at the reset vector.
    fn deassert_srst(&mut self, halt_after_reset: bool) -> Result<()>;
    /// Number of device resets noticed since the last call.
    fn take_resets(&mut self) -> u64;
    /// Whether every reset this probe performs leaves the cores halted (the
    /// BlueBox has only a halting reset). `reset run` then resets as a halting
    /// reset, so the cfg's reset hooks see halted cores, and resumes the
    /// debug-halted cores after `reset-end` through the normal resume path.
    fn reset_always_halts(&self) -> bool {
        false
    }
    fn set_speed_khz(&mut self, khz: u32) -> Result<()>;
    /// Run control the probe performs itself, when it does not expose the raw
    /// OCDS registers the TriCore target drives.
    ///
    /// `None` (the default, and what the TAS and simulator adapters return)
    /// means the caller writes the debug registers over the bus as usual.
    /// See [`crate::target::RunControl`].
    fn run_control(&mut self) -> Option<&mut dyn crate::target::RunControl> {
        None
    }
    /// Programming the probe performs itself, from its own list of files,
    /// instead of by driving the on-chip flash controller over the bus.
    ///
    /// `None` (the default) means the caller runs the TriCore DMU sequences in
    /// [`crate::flash`]. See [`FlashDownload`].
    fn flash_download(&mut self) -> Option<&mut dyn FlashDownload> {
        None
    }
    /// A sampler in the probe itself, which the DAQ registry hands plain bus points to instead of
    /// polling them (see [`ProbeDaq`]).
    ///
    /// `None` (the default, and what the TAS, winIDEA and simulator adapters return) means every
    /// point is read by the host, as it always was.
    fn probe_daq(&mut self) -> Option<&mut dyn ProbeDaq> {
        None
    }
    /// Debug access to the TC3xx standby controller (SCR), see [`ScrDebug`].
    ///
    /// `None` (the default) means this adapter cannot debug the SCR; the SCR target then fails
    /// its examination with a message saying so.
    fn scr_debug(&mut self) -> Option<&mut dyn ScrDebug> {
        None
    }
    /// The run-state poll period this probe wants when the cfg sets none
    /// (`polling_period`). `None`: the server default.
    fn default_poll_period_ms(&self) -> Option<u64> {
        None
    }
    /// The DAQ coalescing gap this probe wants when the cfg sets none
    /// (`daq gap`). `None`: [`crate::daq::DEFAULT_COALESCE_GAP`], measured on a miniWiggler.
    fn default_coalesce_gap(&self) -> Option<u32> {
        None
    }
    /// What the probe knows about the target connection, for logs and frontends. `None`: the
    /// probe cannot tell (the default).
    fn health(&self) -> Option<LinkHealth> {
        None
    }
    /// Work the adapter does between polls, once per poll period before the run-state batch
    /// (e.g. the BlueBox's health query). Never called inside a job or a Tcl script, so it cannot
    /// delay a time-critical sequence.
    fn background(&mut self) {}
    fn quit(&mut self) {}
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

pub const DRIVERS: &[&str] = &[
    "tas_client",
    "sim",
    #[cfg(feature = "winidea")]
    "winidea",
    #[cfg(feature = "bluebox")]
    "bluebox",
];

pub fn create(name: &str) -> Option<Box<dyn Adapter>> {
    match name {
        "tas_client" => Some(Box::new(crate::tas_adapter::TasAdapter::default())),
        "sim" => Some(Box::new(crate::sim::SimAdapter::new())),
        #[cfg(feature = "winidea")]
        "winidea" => Some(Box::new(crate::winidea_adapter::WinIdeaAdapter::new())),
        #[cfg(feature = "bluebox")]
        "bluebox" => Some(Box::new(crate::bluebox_adapter::BlueBoxAdapter::new())),
        _ => None,
    }
}

/// Convenience memory access on top of an adapter.
pub struct Io<'a> {
    pub adapter: &'a mut dyn Adapter,
}

impl<'a> Io<'a> {
    pub fn new(adapter: &'a mut dyn Adapter) -> Self {
        Io { adapter }
    }

    pub fn batch(&mut self, b: &Batch) -> Result<Vec<OpResult>> {
        self.adapter.execute(b)
    }

    pub fn read32(&mut self, addr: u32) -> Result<u32> {
        let mut b = Batch::new();
        b.read32(addr as u64);
        let r = self.batch(&b)?;
        check(&r[0], addr as u64, false)?;
        Ok(r[0].u32())
    }

    pub fn write32(&mut self, addr: u32, value: u32) -> Result<()> {
        let mut b = Batch::new();
        b.write32(addr as u64, value);
        let r = self.batch(&b)?;
        check(&r[0], addr as u64, true)
    }

    /// Writes all (address, value) pairs in one transaction, in order.
    pub fn write32_seq(&mut self, writes: &[(u32, u32)]) -> Result<()> {
        let mut b = Batch::new();
        for &(a, v) in writes {
            b.write32(a as u64, v);
        }
        let r = self.batch(&b)?;
        for (res, &(a, _)) in r.iter().zip(writes) {
            check(res, a as u64, true)?;
        }
        Ok(())
    }

    pub fn read32_many(&mut self, addrs: &[u32]) -> Result<Vec<u32>> {
        let mut b = Batch::new();
        for &a in addrs {
            b.read32(a as u64);
        }
        let r = self.batch(&b)?;
        r.iter().zip(addrs).map(|(res, &a)| check(res, a as u64, false).map(|_| res.u32())).collect()
    }

    pub fn read(&mut self, addr: u32, len: usize) -> Result<Vec<u8>> {
        let mut r = self.read_op(addr, len)?;
        check(&r, addr as u64, false)?;
        Ok(std::mem::take(&mut r.data))
    }

    pub fn write(&mut self, addr: u32, data: &[u8]) -> Result<()> {
        let r = self.write_op(addr, data)?;
        check(&r, addr as u64, true)
    }

    /// Reads host memory like [`Self::read`], but for memory that only allows word access (the HSM
    /// bridge) a failing word only fails its own bytes: unused bridge addresses and the registers
    /// only the HSM may access answer with bus errors. Other memory fails as a whole.
    pub fn read_partial(&mut self, addr: u32, len: usize) -> Result<PartialRead> {
        if !word_only(addr, len) {
            return Ok(PartialRead { data: self.read(addr, len)?, unreadable: Vec::new() });
        }
        PartialRead::from_words(addr, len, |words| {
            let mut b = Batch::new();
            for &w in words {
                b.read32(w as u64);
            }
            let r = self.batch(&b)?;
            Ok(r.iter().zip(words).map(|(res, &w)| check(res, w as u64, false).map(|_| res.u32())).collect())
        })
    }

    /// Reads host memory as one batch op. Memory that only allows word access (the HSM bridge)
    /// is read with one 32-bit access per word; the result then covers the requested bytes as
    /// if it were a single op (`bytes_ok` up to the first failing word).
    pub fn read_op(&mut self, addr: u32, len: usize) -> Result<OpResult> {
        if !word_only(addr, len) {
            let mut b = Batch::new();
            b.read(addr as u64, len);
            return Ok(self.batch(&b)?.remove(0));
        }
        let start = addr & !3;
        let words = (addr - start) as usize + len;
        let words = words.div_ceil(4);
        let mut b = Batch::new();
        for i in 0..words {
            b.read32(start as u64 + 4 * i as u64);
        }
        let r = self.batch(&b)?;
        let failed = r.iter().position(|res| !res.ok());
        let mut data = Vec::with_capacity(4 * words);
        for (i, res) in r.iter().enumerate() {
            let ok = failed.is_none_or(|f| i < f);
            data.extend_from_slice(if ok { &res.data[..4] } else { &[0; 4] });
        }
        let skip = (addr - start) as usize;
        Ok(OpResult {
            err: failed.map_or(tas::PL0_ERR_NO_ERROR, |f| r[f].err),
            bytes_ok: failed.map_or(len, |f| (4 * f).saturating_sub(skip).min(len)),
            data: data[skip..skip + len].to_vec(),
        })
    }

    /// Writes host memory as one batch op; see [`Self::read_op`]. Memory that only allows word
    /// access takes whole aligned words only: merging a partial word by read-modify-write would
    /// set or clear the flags of the other bytes in the bridge's flag registers.
    pub fn write_op(&mut self, addr: u32, data: &[u8]) -> Result<OpResult> {
        let mut b = Batch::new();
        if !word_only(addr, data.len()) {
            b.write(addr as u64, data);
            return Ok(self.batch(&b)?.remove(0));
        }
        if addr % 4 != 0 || data.len() % 4 != 0 {
            return Err(Error::Memory {
                addr: addr as u64,
                write: true,
                detail: "only 32-bit aligned word writes are allowed here (HSM bridge)".into(),
            });
        }
        for (i, w) in data.chunks_exact(4).enumerate() {
            b.write(addr as u64 + 4 * i as u64, w);
        }
        let r = self.batch(&b)?;
        let failed = r.iter().position(|res| !res.ok());
        Ok(OpResult {
            err: failed.map_or(tas::PL0_ERR_NO_ERROR, |f| r[f].err),
            bytes_ok: failed.map_or(data.len(), |f| 4 * f),
            data: Vec::new(),
        })
    }
}

/// Result of a read that may fail for parts of the range.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PartialRead {
    /// The requested bytes, zero where unreadable.
    pub data: Vec<u8>,
    /// Unreadable (address, length, error) ranges within the request, in address order.
    pub unreadable: Vec<(u32, usize, String)>,
}

impl PartialRead {
    /// Reads `addr..addr + len` as aligned words with [`read_words_tolerant`].
    pub fn from_words(addr: u32, len: usize, run: impl FnMut(&[u32]) -> Result<Vec<Result<u32>>>) -> Result<PartialRead> {
        let start = addr & !3;
        let req_end = addr as u64 + len as u64;
        let words: Vec<u32> = (start as u64..(req_end + 3) & !3).step_by(4).map(|a| a as u32).collect();
        let vals = read_words_tolerant(&words, run)?;
        let mut out = PartialRead::default();
        let mut bytes = Vec::with_capacity(4 * words.len());
        for (&w, v) in words.iter().zip(vals) {
            match v {
                Ok(v) => bytes.extend_from_slice(&v.to_le_bytes()),
                Err(e) => {
                    bytes.extend_from_slice(&[0; 4]);
                    // The word clipped to the request, merged with a directly preceding failure.
                    let a = w.max(addr);
                    let n = ((w as u64 + 4).min(req_end) - a as u64) as usize;
                    match out.unreadable.last_mut() {
                        Some((pa, pn, _)) if *pa as u64 + *pn as u64 == a as u64 => *pn += n,
                        _ => out.unreadable.push((a, n, e.to_string())),
                    }
                }
            }
        }
        let off = (addr - start) as usize;
        out.data = bytes[off..off + len].to_vec();
        Ok(out)
    }
}

/// Attempts of a single failing word in [`read_words_tolerant`].
const WORD_TRIES: usize = 4;

/// Reads `words` with `run` (one transaction, a result per word) when single words may fail:
/// - A failing access also fails the accesses after it in the same transaction, so the words after
///   a failure are read again in a new transaction.
/// - A failing word is retried alone, as some accesses fail only now and then (the HSM debug
///   window while the HSM runs).
///
/// `Err` only if `run` itself fails.
pub fn read_words_tolerant(words: &[u32], mut run: impl FnMut(&[u32]) -> Result<Vec<Result<u32>>>) -> Result<Vec<Result<u32>>> {
    let mut out = Vec::with_capacity(words.len());
    while out.len() < words.len() {
        let mut failed = None;
        for v in run(&words[out.len()..])? {
            match v {
                Ok(v) => out.push(Ok(v)),
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            }
        }
        let Some(mut err) = failed else { break };
        let i = out.len();
        let mut value = None;
        for _ in 1..WORD_TRIES {
            match run(&words[i..=i])?.into_iter().next() {
                Some(Ok(v)) => {
                    value = Some(v);
                    break;
                }
                Some(Err(e)) => err = e,
                None => break,
            }
        }
        out.push(value.ok_or(err));
    }
    Ok(out)
}

pub fn check(r: &OpResult, addr: u64, write: bool) -> Result<()> {
    if r.ok() {
        Ok(())
    } else {
        Err(Error::Memory { addr: addr + r.bytes_ok as u64, write, detail: tas::pl_err_name(r.err).to_string() })
    }
}

/// Programming performed by the probe itself, from its own list of files.
///
/// winIDEA flashes from a download list, so the TriCore DMU driver in
/// [`crate::flash`] -- which drives the flash controller over the debug bus --
/// does not apply there and the files are handed over instead.
///
/// Verifying is deliberately *not* part of this. `verify_image` reads the image
/// back through ordinary memory, which works on either backend, so both report
/// verification the same way.
pub trait FlashDownload {
    /// Programs `files` (code only -- symbols stay with our own DWARF reader).
    fn download(&mut self, files: &[crate::image::ImageFile]) -> Result<()>;
}
