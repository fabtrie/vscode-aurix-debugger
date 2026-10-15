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
    /// The run-state poll period this probe wants when the cfg sets none
    /// (`polling_period`). `None`: the server default.
    fn default_poll_period_ms(&self) -> Option<u64> {
        None
    }
    fn quit(&mut self) {}
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

#[cfg(feature = "winidea")]
pub const DRIVERS: &[&str] = &["tas_client", "sim", "winidea"];
#[cfg(not(feature = "winidea"))]
pub const DRIVERS: &[&str] = &["tas_client", "sim"];

pub fn create(name: &str) -> Option<Box<dyn Adapter>> {
    match name {
        "tas_client" => Some(Box::new(crate::tas_adapter::TasAdapter::default())),
        "sim" => Some(Box::new(crate::sim::SimAdapter::new())),
        #[cfg(feature = "winidea")]
        "winidea" => Some(Box::new(crate::winidea_adapter::WinIdeaAdapter::new())),
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
    fn download(&mut self, files: &[String]) -> Result<()>;
}
