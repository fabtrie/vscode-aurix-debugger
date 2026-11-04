//! The `bluebox` driver: an iSYSTEM BlueBox (iC5700) over USB, without winIDEA.
//!
//! Unlike the `winidea` driver, this one only moves memory. Halting,
//! stepping and breakpoints go through the TriCore OCDS registers, as on the
//! TAS path: [`Adapter::run_control`] stays `None`, and `tc3xx_ocds.cfg`
//! enables its OCDS setup for every driver but `winidea`.
//!
//! Connecting:
//!
//! 1. Link bring-up (`bluebox::lowlevel`): hello and handshake. After a power
//!    cycle of the BlueBox no kernel runs; the driver then loads `iC6kKrnl.s`
//!    from `bluebox kernel <path>` or `C:\winIDEA` and sets the session.
//! 2. The target connection (`bluebox::attach`): winIDEA 9.21.356's attach to
//!    a TC377 after a power cycle, 3582 messages, built in (about 3.2 s).
//!    `bluebox attach_replay <trace.jsonl>` uses another captured attach
//!    instead, sent message by message after the driver's own bring-up.
//!
//! Neither resets the target. The probe keeps no session across connects:
//! every `init` runs both steps.
//!
//! Recovery, in the place of what the TAS server does for a miniWiggler:
//! after a batch with failed accesses the driver reads `SCU_CHIPID`. If that
//! fails too, the target connection is lost (seen after the target reset
//! itself). It then sends the connect part of the attach again (from its last
//! `L` command, about 0.4 s), at most once a second. A USB or protocol error
//! means the probe is lost: it is opened again and the whole connect runs, at
//! most every 3 s. Batches fail in between, so the core marks the targets
//! failed and re-examines them until the connection is back. A target
//! reconnect also counts as a reset for [`Adapter::take_resets`] ("Device
//! reset detected"); a probe reconnect does not, since a BlueBox power cycle
//! left the application running. A target reconnect right after the target
//! reset itself was once seen to reset the chip again.
//!
//! ```tcl
//! adapter driver bluebox
//! adapter serial 01ABC                    ;# optional, when several probes are plugged in
//! bluebox kernel C:/winIDEA/iC6kKrnl.s    ;# optional; winIDEA 9.21.356's
//! bluebox attach_replay C:/traces/x.jsonl ;# optional; another target or setup
//! ```
//!
//! `reset run|halt|init` reset the chip with the probe's reset (`bluebox::reset`),
//! which always halts cpu0 at its start address. For `reset run` the core treats
//! it as a halting reset (so `tc3xx_ocds.cfg` starts the HSM as after `reset
//! halt`) and resumes the halted cores after `reset-end`.
//! `reset_config connect_assert_srst` resets and halts right after connecting.

use std::any::Any;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tas::{Batch, BatchOp, OpResult};

use crate::adapter::{Adapter, AdapterSettings, BatchLimits, DeviceInfo};
use crate::error::{other, Error, Result};

const NOT_CONNECTED: &str = "bluebox: not connected (init has not run, or failed)";
const DEFAULT_STEP: &str = "connect: begin_attach";
/// Read once at init: proves the probe reaches the target.
const SCU_CHIPID: u32 = 0xF003_6140;

/// Interval of reconnect attempts after the target stopped answering.
const TARGET_RETRY: Duration = Duration::from_secs(1);
/// Interval of reconnect attempts after the probe was lost (each a full connect, ~3.5 s).
const PROBE_RETRY: Duration = Duration::from_secs(3);

#[cfg(windows)]
type Probe = bluebox::Probe<bluebox::usb::UsbLink>;

/// What a failed access lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Lost {
    /// The probe answers, the target does not: reconnect the target.
    Target,
    /// USB or protocol errors: open the probe again.
    Probe,
}

#[derive(Default)]
pub struct BlueBoxAdapter {
    replay: Option<(PathBuf, String)>,
    srst_pending: bool,
    kernel: Option<PathBuf>,
    #[cfg(windows)]
    probe: Option<Probe>,
    status: String,
    /// `init` succeeded once; the connection may be lost since.
    inited: bool,
    settings: AdapterSettings,
    /// The attach `init` used; its connect part is what a reconnect sends.
    script: Option<bluebox::attach::Script>,
    lost: Option<Lost>,
    last_try: Option<Instant>,
    resets: u64,
}

impl BlueBoxAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset with halt (see [`Adapter::deassert_srst`]).
    #[cfg(windows)]
    fn reset(&mut self) -> Result<()> {
        self.ensure_connected()?;
        let st = self.probe()?.reset_halt().map_err(|e| other(format!("bluebox: reset: {e}")))?;
        if st.halted() {
            crate::log_info!("[bluebox] reset; cpu0 halted at 0x{:08x}", st.pc);
        } else {
            crate::log_warn!("[bluebox] after the reset cpu0 is not halted (DBGSR 0x{:08x})", st.dbgsr);
        }
        Ok(())
    }

    #[cfg(windows)]
    fn probe(&mut self) -> Result<&mut Probe> {
        self.probe.as_mut().ok_or_else(|| Error::Usage(NOT_CONNECTED.into()))
    }

    /// Notes a lost connection; the next batch tries to reconnect.
    #[cfg(windows)]
    fn lose(&mut self, kind: Lost, why: &str) {
        if self.lost.is_none() {
            crate::log_warn!("[bluebox] {why}; reconnecting");
        }
        self.lost = self.lost.max(Some(kind));
        if kind == Lost::Probe {
            self.probe = None;
        }
        self.last_try = None;
    }

    /// Whether the target answers (`SCU_CHIPID` reads back).
    #[cfg(windows)]
    fn target_answers(&mut self) -> bool {
        self.probe.as_mut().is_some_and(|p| p.read_memory(SCU_CHIPID, 4).is_ok_and(|m| m.all_ok()))
    }

    /// Reconnects if a connection was lost and the retry interval has passed.
    #[cfg(windows)]
    fn ensure_connected(&mut self) -> Result<()> {
        let Some(kind) = self.lost else { return Ok(()) };
        let wait = if kind == Lost::Target { TARGET_RETRY } else { PROBE_RETRY };
        let what = if kind == Lost::Target { "the target does not answer" } else { "the probe is not connected" };
        if self.last_try.is_some_and(|t| t.elapsed() < wait) {
            return Err(Error::Adapter(format!("bluebox: {what}; reconnecting")));
        }
        self.last_try = Some(Instant::now());
        let t0 = Instant::now();
        let r = match kind {
            Lost::Target => self.reconnect_target(),
            Lost::Probe => {
                let settings = self.settings.clone();
                self.connect(&settings, false).map(|_| ())
            }
        };
        match r {
            Ok(()) => {
                self.lost = None;
                // Losing the target means it reset (or lost power). Losing the probe
                // does not: on a BlueBox power cycle the application ran on.
                if kind == Lost::Target {
                    self.resets += 1;
                }
                crate::log_info!("[bluebox] reconnected in {} ms", t0.elapsed().as_millis());
                Ok(())
            }
            Err(e) => {
                crate::log_debug!("[bluebox] reconnect failed: {e}");
                Err(Error::Adapter(format!("bluebox: {what}; reconnecting")))
            }
        }
    }

    /// The connect part of the attach again, in the probe session that is open.
    #[cfg(windows)]
    fn reconnect_target(&mut self) -> Result<()> {
        let part = match &self.script {
            Some(s) => connect_part(s),
            None => return Err(Error::Usage(NOT_CONNECTED.into())),
        };
        let probe = self.probe.as_mut().ok_or_else(|| Error::Usage(NOT_CONNECTED.into()))?;
        if let Err(e) = probe.attach(&part) {
            // The probe itself failed: next time, open it again.
            self.lose(Lost::Probe, &format!("probe error while reconnecting: {e}"));
            return Err(other(format!("bluebox: {e}")));
        }
        if self.target_answers() {
            Ok(())
        } else {
            Err(other("bluebox: the target does not answer after the reconnect"))
        }
    }

    /// Runs a batch: one probe request per op. A probe-level error ends it.
    #[cfg(windows)]
    fn run_batch(&mut self, batch: &Batch) -> Result<Vec<OpResult>> {
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

    #[cfg(windows)]
    fn connect(&mut self, settings: &AdapterSettings, first: bool) -> Result<DeviceInfo> {
        use bluebox::replay::Replay;
        use bluebox::usb::UsbLink;

        let bb = |e: bluebox::Error| other(format!("bluebox: {e}"));
        // The attach to send: the built-in one, or one taken from a capture.
        let (script, script_note) = match &self.replay {
            Some((trace, step)) => {
                // Its own bring-up (and kernel load, in a trace taken after a power
                // cycle) is left out: the driver does that part.
                let r = Replay::from_trace(trace, step).map_err(bb)?;
                (r.attach_script().map_err(bb)?, format!("attach from {}", trace.display()))
            }
            None => (bluebox::attach::Script::tc37x(), "built-in TC37x attach".to_string()),
        };

        let link = UsbLink::open(settings.serial.as_deref()).map_err(bb)?;
        let path = link.path().to_string();
        // Link bring-up. It ends any earlier client session; the attach below
        // makes the target connection again.
        let mut probe = bluebox::Probe::start(link).map_err(bb)?;
        let hs = probe.handshake().map_err(bb)?;
        let kernel_note = if hs.kernel_running {
            "kernel was running".to_string()
        } else {
            let kpath = self.kernel.clone().or_else(bluebox::kernel::find_in_winidea).ok_or_else(|| {
                other(format!(
                    "bluebox: the probe has no kernel running (it was power cycled) and no {} was found; \
                     give 'bluebox kernel <path>' from winIDEA 9.21.356",
                    bluebox::kernel::FILE_NAME
                ))
            })?;
            let k = bluebox::Kernel::from_file(&kpath).map_err(bb)?;
            let t0 = std::time::Instant::now();
            probe.load_kernel(&k, |_| {}).map_err(bb)?;
            probe.set_session(&bluebox::lowlevel::SESSION_9_21_356).map_err(bb)?;
            crate::log_info!(
                "[bluebox] no kernel was running: loaded {} ({} bytes) in {} ms",
                kpath.display(),
                k.len(),
                t0.elapsed().as_millis()
            );
            format!("kernel loaded from {}", kpath.display())
        };

        let t0 = std::time::Instant::now();
        let rep = probe.attach(&script).map_err(bb)?;
        crate::log_info!(
            "[bluebox] {script_note}: {} exchanges in {} ms{}",
            rep.sent,
            t0.elapsed().as_millis(),
            if rep.other_len.is_empty() {
                String::new()
            } else {
                format!(", {} replies of another length than captured", rep.other_len.len())
            }
        );
        let id = probe.read_memory(SCU_CHIPID, 4).map_err(bb)?;
        if !id.all_ok() {
            return Err(other(format!(
                "bluebox: the attach ran ({script_note}) but the target does not answer: is it powered \
                 and is it a TC37x (the built-in attach is for that derivative)?"
            )));
        }
        let chipid = u32::from_le_bytes(id.data[..4].try_into().unwrap());
        if first && settings.connect_under_srst {
            // `reset_config connect_assert_srst`: connect with reset and halt.
            let st = probe.reset_halt().map_err(bb)?;
            crate::log_info!("[bluebox] connected under reset; cpu0 halted at 0x{:08x}", st.pc);
        }
        self.status = format!("{path}, SCU_CHIPID 0x{chipid:08x}");
        self.status.push_str(&format!(", {kernel_note}, {script_note}"));
        self.probe = Some(probe);
        self.script = Some(script);
        // The JTAG id is not read here: where the probe exposes it is unknown,
        // and `ops::init` skips the `-expected-id` check on 0.
        Ok(DeviceInfo { device_id: 0, description: format!("BlueBox {}", self.status) })
    }
}

/// The part of an attach that makes the target connection: from its last `L`
/// command (the second handshake) on. After the target reset itself, it
/// restores the connection in about 0.4 s; the whole attach does too, in 3.2 s.
fn connect_part(script: &bluebox::attach::Script) -> bluebox::attach::Script {
    let from = script.exchanges.iter().rposition(|x| x.payload == [0x03, 0x21, b'L']).unwrap_or(0);
    bluebox::attach::Script { exchanges: script.exchanges[from..].to_vec() }
}

impl Adapter for BlueBoxAdapter {
    fn driver_name(&self) -> &'static str {
        "bluebox"
    }

    fn command_groups(&self) -> &'static [&'static str] {
        &["bluebox"]
    }

    /// `bluebox attach_replay <trace.jsonl> [step]`, `bluebox kernel <iC6kKrnl.s>`,
    /// `bluebox status` and `bluebox drop_target` (a test aid for recovery).
    /// Only `drop_target` talks to the probe; it is opened in [`Adapter::init`].
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
            // Test aid: reset the target and leave the connection broken, as after
            // a reset the target did by itself. The next accesses reconnect.
            Some("drop_target") => {
                #[cfg(windows)]
                {
                    self.probe()?.reset_without_reconnect().map_err(|e| other(format!("bluebox: {e}")))?;
                    Ok("target reset, connection left broken".into())
                }
                #[cfg(not(windows))]
                {
                    Err(Error::Usage(NOT_CONNECTED.into()))
                }
            }
            _ => Err(Error::Usage("usage: bluebox attach_replay|kernel|status|drop_target ...".into())),
        }
    }

    fn init(&mut self, settings: &AdapterSettings) -> Result<DeviceInfo> {
        #[cfg(windows)]
        {
            let info = self.connect(settings, true)?;
            self.settings = settings.clone();
            self.inited = true;
            self.lost = None;
            Ok(info)
        }
        #[cfg(not(windows))]
        {
            let _ = settings;
            Err(other("bluebox: the USB link needs WinUSB (Windows)"))
        }
    }

    fn initialized(&self) -> bool {
        self.inited
    }

    /// Closes the USB handle. The probe stays attached to the target.
    fn quit(&mut self) {
        #[cfg(windows)]
        {
            self.probe = None;
        }
        self.inited = false;
        self.lost = None;
    }

    /// One probe request per op (split at 8 KiB). A failing op makes the
    /// rest of the batch consequential, as a TAS packet reports a fault part-way.
    /// Notices a lost connection and reconnects (see the module docs).
    fn execute(&mut self, batch: &Batch) -> Result<Vec<OpResult>> {
        #[cfg(not(windows))]
        {
            let _ = batch;
            Err(Error::Usage(NOT_CONNECTED.into()))
        }
        #[cfg(windows)]
        {
            if !self.inited {
                return Err(Error::Usage(NOT_CONNECTED.into()));
            }
            self.ensure_connected()?;
            match self.run_batch(batch) {
                Ok(out) => {
                    let failed = out.iter().any(|r| r.err != tas::PL0_ERR_NO_ERROR && r.err != tas::PL0_ERR_CONSEQUENTIAL);
                    if failed && !self.target_answers() {
                        self.lose(Lost::Target, "the target stopped answering");
                    }
                    Ok(out)
                }
                Err(e) => {
                    self.lose(Lost::Probe, &format!("probe error: {e}"));
                    Err(e)
                }
            }
        }
    }

    /// Each op is its own USB round trip (about 1.2 ms for a small one), so
    /// this is a round-trip budget rather than packet capacity.
    fn limits(&self) -> BatchLimits {
        BatchLimits { max_num_rw: 16, max_rq: bluebox::MAX_CHUNK, max_rsp: bluebox::MAX_CHUNK }
    }

    /// Records the request: the probe has no assert/release split. The reset
    /// happens in [`Adapter::deassert_srst`], where the halt-or-run choice is known.
    fn assert_srst(&mut self) -> Result<()> {
        if !self.initialized() {
            return Err(Error::Usage(NOT_CONNECTED.into()));
        }
        self.srst_pending = true;
        Ok(())
    }

    /// The whole reset (`bluebox::reset`): the chip resets and cpu0 halts at
    /// its start address. There is no reset without halt; for `reset run` the
    /// core resumes afterwards ([`Adapter::reset_always_halts`]).
    fn deassert_srst(&mut self, _halt_after_reset: bool) -> Result<()> {
        if !std::mem::take(&mut self.srst_pending) {
            return Ok(());
        }
        #[cfg(windows)]
        {
            self.reset()
        }
        #[cfg(not(windows))]
        {
            Err(Error::Usage(NOT_CONNECTED.into()))
        }
    }

    fn reset_always_halts(&self) -> bool {
        true
    }

    /// Target reconnects since the last call: the target stopped answering,
    /// so it has most likely been reset (or powered down) in between.
    fn take_resets(&mut self) -> u64 {
        std::mem::take(&mut self.resets)
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
    fn the_connect_part_starts_at_the_last_l_command() {
        let full = bluebox::attach::Script::tc37x();
        let part = connect_part(&full);
        assert_eq!(part.exchanges.first().unwrap().payload, [0x03, 0x21, b'L']);
        assert!(part.exchanges.len() < full.exchanges.len() / 10, "{} of {}", part.exchanges.len(), full.exchanges.len());
        assert_eq!(part.exchanges.last(), full.exchanges.last());
    }

    #[test]
    fn nothing_runs_before_init() {
        let mut a = BlueBoxAdapter::new();
        let mut b = Batch::new();
        b.read32(0x7000_0000);
        assert!(matches!(a.execute(&b), Err(Error::Usage(_))));
        assert!(a.run_control().is_none());
        assert!(a.assert_srst().is_err(), "no reset before init");
        assert!(a.deassert_srst(true).is_ok(), "nothing pending: nothing to do");
    }
}
