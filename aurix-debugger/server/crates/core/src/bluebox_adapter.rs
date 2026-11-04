//! The `bluebox` driver: an iSYSTEM BlueBox (iC5700) over USB, without winIDEA.
//!
//! Unlike the `winidea` driver, this one only moves memory. Halting,
//! stepping and breakpoints go through the TriCore OCDS registers, as on the
//! TAS path: [`Adapter::run_control`] stays `None`, and `tc3xx_ocds.cfg`
//! enables its OCDS setup for every driver but `winidea`.
//!
//! Connecting, with `bluebox attach_replay` in the cfg:
//!
//! 1. Link bring-up (`bluebox::lowlevel`): hello and handshake. After a power
//!    cycle of the BlueBox no kernel runs; the driver then loads `iC6kKrnl.s`
//!    from `bluebox kernel <path>` or `C:\winIDEA` and sets the session.
//! 2. The target connection: not decoded, so a captured winIDEA attach
//!    (taken with the kernel running) is played back.
//!
//! Without `attach_replay`, the probe must still be attached from an earlier
//! session, and the driver reads directly: a handshake would end that session.
//!
//! ```tcl
//! adapter driver bluebox
//! adapter serial 01ABC                ;# optional, when several probes are plugged in
//! bluebox kernel C:/winIDEA/iC6kKrnl.s  ;# optional; the same winIDEA version as the trace
//! bluebox attach_replay C:/traces/followup.jsonl "connect: begin_attach"
//! ```
//!
//! Reset is not decoded either: `reset` fails on this driver.

use std::any::Any;
use std::path::PathBuf;

use tas::{Batch, BatchOp, OpResult};

use crate::adapter::{Adapter, AdapterSettings, BatchLimits, DeviceInfo};
use crate::error::{other, Error, Result};

const NOT_CONNECTED: &str = "bluebox: not connected (init has not run, or failed)";
const DEFAULT_STEP: &str = "connect: begin_attach";
/// Read once at init: proves the probe reaches the target.
const SCU_CHIPID: u32 = 0xF003_6140;

#[cfg(windows)]
type Probe = bluebox::Probe<bluebox::usb::UsbLink>;

#[derive(Default)]
pub struct BlueBoxAdapter {
    replay: Option<(PathBuf, String)>,
    kernel: Option<PathBuf>,
    #[cfg(windows)]
    probe: Option<Probe>,
    status: String,
}

impl BlueBoxAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(windows)]
    fn probe(&mut self) -> Result<&mut Probe> {
        self.probe.as_mut().ok_or_else(|| Error::Usage(NOT_CONNECTED.into()))
    }

    #[cfg(windows)]
    fn connect(&mut self, settings: &AdapterSettings) -> Result<DeviceInfo> {
        use bluebox::replay::Replay;
        use bluebox::usb::UsbLink;

        let bb = |e: bluebox::Error| other(format!("bluebox: {e}"));
        let serial = settings.serial.as_deref();
        let mut link = UsbLink::open(serial).map_err(bb)?;
        let path = link.path().to_string();
        let mut toggle = false;
        let mut kernel_note = String::new();
        if let Some((trace, step)) = &self.replay {
            let mut r = Replay::from_trace(trace, step).map_err(bb)?;
            if r.loads_kernel() {
                return Err(other(format!(
                    "bluebox: {} loads the probe's kernel itself (it was captured right after a power cycle); \
                     give a trace of an attach with the kernel running: the driver loads the kernel itself",
                    trace.display()
                )));
            }
            // Link bring-up. It ends any earlier client session, so only here,
            // where the replay re-establishes the target connection.
            let mut p = bluebox::Probe::start(link).map_err(bb)?;
            let hs = p.handshake().map_err(bb)?;
            if hs.kernel_running {
                kernel_note = "kernel was running".into();
            } else {
                let kpath = self.kernel.clone().or_else(bluebox::kernel::find_in_winidea).ok_or_else(|| {
                    other(format!(
                        "bluebox: the probe has no kernel running (it was power cycled) and no {} was found; \
                         give 'bluebox kernel <path>' from the winIDEA version the trace was captured with",
                        bluebox::kernel::FILE_NAME
                    ))
                })?;
                let k = bluebox::Kernel::from_file(&kpath).map_err(bb)?;
                let t0 = std::time::Instant::now();
                p.load_kernel(&k, |_| {}).map_err(bb)?;
                p.set_session(&bluebox::lowlevel::SESSION_9_21_356).map_err(bb)?;
                kernel_note = format!("kernel loaded from {}", kpath.display());
                crate::log_info!(
                    "[bluebox] no kernel was running: loaded {} ({} bytes) in {} ms",
                    kpath.display(),
                    k.len(),
                    t0.elapsed().as_millis()
                );
            }
            drop(p);
            // The captured attach starts with its own hello: a fresh open.
            link = UsbLink::open(serial).map_err(bb)?;
            let disarmed = r.disarm_breakpoints();
            if disarmed > 0 {
                crate::log_info!("[bluebox] the captured attach arms {disarmed} breakpoint(s); replaying them as cleared");
            }
            let t0 = std::time::Instant::now();
            let rep = r.play(&mut link, |_| {}).map_err(bb)?;
            crate::log_info!(
                "[bluebox] replayed '{step}' from {}: {} transfers in {} ms, {} of {} replies as captured",
                trace.display(),
                r.transfers.len(),
                t0.elapsed().as_millis(),
                rep.same + rep.same_len,
                rep.reads
            );
            toggle = r.toggle_after().unwrap_or(false);
        }
        // A freshly opened probe accepted toggle 0 in every run without a replay.
        let mut probe = Probe::new(link, toggle);
        let id = probe.read_memory(SCU_CHIPID, 4).map_err(bb)?;
        if !id.all_ok() {
            return Err(other(
                "bluebox: the probe answers but cannot reach the target. It is not attached: \
                 attach once with winIDEA, or give 'bluebox attach_replay <trace.jsonl>'",
            ));
        }
        let chipid = u32::from_le_bytes(id.data[..4].try_into().unwrap());
        self.status = format!("{path}, SCU_CHIPID 0x{chipid:08x}");
        if !kernel_note.is_empty() {
            self.status.push_str(&format!(", {kernel_note}"));
        }
        self.probe = Some(probe);
        // The JTAG id is not read here: where the probe exposes it is unknown,
        // and `ops::init` skips the `-expected-id` check on 0.
        Ok(DeviceInfo { device_id: 0, description: format!("BlueBox {}", self.status) })
    }
}

impl Adapter for BlueBoxAdapter {
    fn driver_name(&self) -> &'static str {
        "bluebox"
    }

    fn command_groups(&self) -> &'static [&'static str] {
        &["bluebox"]
    }

    /// `bluebox attach_replay <trace.jsonl> [step]`, `bluebox kernel <iC6kKrnl.s>`
    /// and `bluebox status`.
    /// Only sets fields; the probe is opened in [`Adapter::init`].
    fn command(&mut self, args: &[&str]) -> Result<String> {
        match args.get(1).copied() {
            Some("attach_replay") => {
                let p = args
                    .get(2)
                    .ok_or_else(|| Error::Usage("usage: bluebox attach_replay <trace.jsonl> [step]".into()))?;
                let step = args.get(3).copied().unwrap_or(DEFAULT_STEP);
                self.replay = Some((PathBuf::from(p), step.to_string()));
                Ok(String::new())
            }
            Some("kernel") => {
                let p = args.get(2).ok_or_else(|| Error::Usage("usage: bluebox kernel <iC6kKrnl.s>".into()))?;
                self.kernel = Some(PathBuf::from(p));
                Ok(String::new())
            }
            Some("status") => Ok(if self.initialized() { self.status.clone() } else { "not connected".into() }),
            _ => Err(Error::Usage("usage: bluebox attach_replay|kernel|status ...".into())),
        }
    }

    fn init(&mut self, settings: &AdapterSettings) -> Result<DeviceInfo> {
        #[cfg(windows)]
        {
            self.connect(settings)
        }
        #[cfg(not(windows))]
        {
            let _ = settings;
            Err(other("bluebox: the USB link needs WinUSB (Windows)"))
        }
    }

    fn initialized(&self) -> bool {
        #[cfg(windows)]
        {
            self.probe.is_some()
        }
        #[cfg(not(windows))]
        {
            false
        }
    }

    /// Closes the USB handle. The probe stays attached to the target.
    fn quit(&mut self) {
        #[cfg(windows)]
        {
            self.probe = None;
        }
    }

    /// One probe request per op (split at 8 KiB). A failing op makes the
    /// rest of the batch consequential, as a TAS packet reports a fault part-way.
    fn execute(&mut self, batch: &Batch) -> Result<Vec<OpResult>> {
        #[cfg(not(windows))]
        {
            let _ = batch;
            Err(Error::Usage(NOT_CONNECTED.into()))
        }
        #[cfg(windows)]
        {
            let probe = self.probe()?;
            let mut out = Vec::with_capacity(batch.len());
            let mut failed = false;
            for op in batch.iter() {
                if failed {
                    out.push(OpResult { err: tas::PL0_ERR_CONSEQUENTIAL, bytes_ok: 0, data: Vec::new() });
                    continue;
                }
                let r = match op {
                    BatchOp::Read { addr, len, .. } => {
                        let m = probe.read_memory(addr as u32, len).map_err(|e| other(format!("bluebox: {e}")))?;
                        let ok = m.bytes_ok(len);
                        let mut data = m.data;
                        data[ok..].fill(0);
                        let err = if ok == len { tas::PL0_ERR_NO_ERROR } else { tas::PL0_ERR_DATA };
                        OpResult { err, bytes_ok: ok, data }
                    }
                    BatchOp::Write { addr, data, .. } => {
                        let m = probe.write_memory(addr as u32, data).map_err(|e| other(format!("bluebox: {e}")))?;
                        let ok = m.bytes_ok(data.len());
                        let err = if ok == data.len() { tas::PL0_ERR_NO_ERROR } else { tas::PL0_ERR_DATA };
                        OpResult { err, bytes_ok: ok, data: Vec::new() }
                    }
                };
                failed = r.err != tas::PL0_ERR_NO_ERROR;
                out.push(r);
            }
            Ok(out)
        }
    }

    /// Each op is its own USB round trip (about 1.2 ms for a small one), so
    /// this is a round-trip budget rather than packet capacity.
    fn limits(&self) -> BatchLimits {
        BatchLimits { max_num_rw: 16, max_rq: bluebox::MAX_CHUNK, max_rsp: bluebox::MAX_CHUNK }
    }

    fn assert_srst(&mut self) -> Result<()> {
        Err(other("bluebox: reset is not decoded yet"))
    }

    fn deassert_srst(&mut self, _halt_after_reset: bool) -> Result<()> {
        Err(other("bluebox: reset is not decoded yet"))
    }

    fn take_resets(&mut self) -> u64 {
        0
    }

    /// The JTAG/DAP clock is part of the bring-up this driver does not
    /// perform, so the setting has nothing to change.
    fn set_speed_khz(&mut self, _khz: u32) -> Result<()> {
        Ok(())
    }

    /// A run-state poll is a few register reads per core at about 1.2 ms
    /// each. The server's default period would keep the core thread busy.
    fn default_poll_period_ms(&self) -> Option<u64> {
        Some(25)
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_only_set_fields() {
        let mut a = BlueBoxAdapter::new();
        a.command(&["bluebox", "attach_replay", "C:/t.jsonl"]).unwrap();
        assert_eq!(a.replay, Some((PathBuf::from("C:/t.jsonl"), DEFAULT_STEP.to_string())));
        a.command(&["bluebox", "attach_replay", "x.jsonl", "connect: other"]).unwrap();
        assert_eq!(a.replay.as_ref().unwrap().1, "connect: other");
        a.command(&["bluebox", "kernel", "C:/winIDEA/iC6kKrnl.s"]).unwrap();
        assert_eq!(a.kernel, Some(PathBuf::from("C:/winIDEA/iC6kKrnl.s")));
        assert_eq!(a.command(&["bluebox", "status"]).unwrap(), "not connected");
        assert!(a.command(&["bluebox", "frobnicate"]).is_err());
        assert!(!a.initialized());
    }

    #[test]
    fn nothing_runs_before_init() {
        let mut a = BlueBoxAdapter::new();
        let mut b = Batch::new();
        b.read32(0x7000_0000);
        assert!(matches!(a.execute(&b), Err(Error::Usage(_))));
        assert!(a.run_control().is_none());
        assert!(a.assert_srst().is_err());
    }
}
