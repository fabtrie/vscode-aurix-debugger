//! The `bluebox` driver: an iSYSTEM BlueBox (iC5700 or iC5000) over USB, without winIDEA.
//!
//! Unlike the `winidea` driver, this one only moves memory. Halting,
//! stepping and breakpoints go through the TriCore OCDS registers, as on the
//! TAS path: [`Adapter::run_control`] stays `None`, and `tc3xx_ocds.cfg`
//! enables its OCDS setup for every driver but `winidea`.
//!
//! Connecting:
//!
//! Everything version-specific (kernel, service numbers, session value,
//! attach) is winIDEA 9.21.356's: service numbers differ from version to
//! version, so the driver works with that version only.
//!
//! 1. Link bring-up (`bluebox::lowlevel`): hello and handshake. A running
//!    kernel is asked whether it is winIDEA 9.21.356's. Another version's (the
//!    BlueBox was last used by another winIDEA) is stopped and replaced, as
//!    winIDEA does; nothing else may go to it. After a power cycle no kernel
//!    runs. Either way the driver loads the kernel (`iC6kKrnl.s`, `iC3kKrnl.s`
//!    on an iC5000, which it tells by its USB ids) and sets the session: from
//!    `bluebox kernel <file>`, or from a winIDEA 9.21.356 install, given by
//!    `bluebox winidea <folder>` or found in `C:\winIDEA`, `C:\winIDEA*` or
//!    `C:\iSYSTEM\*` (its `deploy.json` gives the version). Without one, the
//!    error says which versions were found and asks for 9.21.356.
//! 2. The debug FPGA image. An iC5700 keeps it in flash: it must be
//!    9.21.356's. Another version's leaves the probe unusable for this driver;
//!    the error asks to connect once with winIDEA 9.21.356, which loads its
//!    image. An iC5000 gets it on every connect, as winIDEA does it: the driver
//!    takes it from the install's `ICWHW32.HRS` and uploads it inside the attach.
//! 3. The target connection (`bluebox::attach`): winIDEA 9.21.356's attach to
//!    a TC377 after a power cycle, 3582 messages, built in (about 3.2 s).
//!    `bluebox attach_replay <trace.jsonl>` uses another captured attach
//!    instead, sent message by message after the driver's own bring-up.
//!    Whether an active probe sits between BlueBox and target is told by the
//!    node count of the attach's node-5 scan (`//5///22/ 00 ... c8`, 2 with
//!    one, 1 without), as winIDEA does: the built-in attach then continues
//!    with that setup's script (`Script::tc37x` or `Script::tc37x_system`).
//!    The setup also decides how the DAP clock is set: `//7//1//` (Hz) with an
//!    active probe, `//4///7/` (kHz) without. An iC5000 has its own attach
//!    (`Script::tc37x_ic5000`, winIDEA's hot attach, 272 messages, 2.6 s) and
//!    no node scan; it sets its DAP clock through node 5 (30 MHz divided by an
//!    integer, 2 to 30 MHz).
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
//! bluebox winidea C:/winIDEA_9_21_356     ;# optional; a winIDEA 9.21.356 install, for its kernel
//! bluebox kernel C:/winIDEA/iC6kKrnl.s    ;# optional; that version's kernel file itself, unchecked
//! bluebox attach_replay C:/traces/x.jsonl ;# optional; another target or setup
//! ```
//!
//! `adapter speed <kHz>` sets the DAP clock (`//7//1//`): 100 MHz divided by an
//! integer, rounded down, 1 to 25 MHz (30 MHz gives 25); on an iC5000 30 MHz
//! divided by an integer, 2 to 30 MHz (`//5///2/ 8e 00 00 b0 <n> ff`). The attach and every
//! reconnect run at the captured 10 MHz (bringing a target into debug mode
//! failed at 25 MHz); the configured clock is switched to once the target
//! answers, and back to 10 MHz if it does not answer there. A change after
//! `init` applies at once; resets keep it.
//!
//! Memory accesses of a batch go to the probe batched: reads share a message
//! (up to 64, and a reply of at most 60,000 bytes; ops over 8 KiB are split);
//! a write has a message of its own, followed by a read of `STM0_TIM0` that
//! carries the write's late bus error. The probe runs every access of a
//! message, also after a failed one, so this keeps a write from running after
//! a failed op, which the core reports as consequential, and keeps anything
//! from running after a write that failed. Each access costs about 0.45 ms in the probe,
//! so 4 small reads take about 2 ms instead of 5; 8 KiB reads reach 1.5 MB/s.
//! Unless the cfg sets them, the run-state poll runs every 15 ms and the DAQ
//! merges points up to 512 bytes apart (`polling_period`, `daq gap`).
//!
//! `reset run|halt|init` reset the chip with the probe's reset (`bluebox::reset`; on an
//! iC5000 it waits 110 ms before reconnecting),
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

/// The DAP clock the built-in attach was captured with.
#[cfg(windows)]
const ATTACH_CLOCK_HZ: u32 = 10_000_000;

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
    /// `bluebox winidea`: the folder of a winIDEA 9.21.356 install, for its kernel.
    winidea: Option<PathBuf>,
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
    /// Debug clock from `adapter speed`, in Hz; `None`: the attach's 10 MHz.
    clock_hz: Option<u32>,
    /// With or without active probe, as the attach found it; it decides the
    /// attach script and how the clock is set. `None` before the first attach
    /// (taken as with active probe, the setup the driver was first written for).
    setup: Option<bluebox::attach::Setup>,
    /// The BlueBox model, from its USB ids; known after the first connect.
    model: Option<bluebox::Model>,
    /// The iC5000's debug FPGA image, read once from winIDEA's `ICWHW32.HRS`.
    fpga_image: Option<Vec<u8>>,
}

impl BlueBoxAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset with halt (see [`Adapter::deassert_srst`]).
    #[cfg(windows)]
    fn reset(&mut self) -> Result<()> {
        self.ensure_connected()?;
        let clock = self.clock_hz.map(|hz| self.dap_clock(hz).0);
        let setup = self.setup();
        let probe = self.probe()?;
        if clock.is_some_and(|hz| hz != ATTACH_CLOCK_HZ) {
            // The reconnect after the reset brings a fresh target into debug mode:
            // at the attach's clock, as the attach does.
            probe.set_clock_for(setup, ATTACH_CLOCK_HZ).map_err(|e| other(format!("bluebox: reset: {e}")))?;
        }
        let st = probe.reset_halt().map_err(|e| other(format!("bluebox: reset: {e}")))?;
        self.apply_clock();
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

    /// Switches to the `adapter speed` clock on a connected target. If the target
    /// does not answer there, goes back to the attach's 10 MHz. Returns whether
    /// the configured clock is in use.
    #[cfg(windows)]
    fn apply_clock(&mut self) -> bool {
        let Some(want) = self.clock_hz else { return true };
        let (hz, div) = self.dap_clock(want);
        let setup = self.setup();
        let ok = self.probe.as_mut().is_some_and(|p| p.set_clock_for(setup, hz).is_ok()) && self.target_answers();
        if ok {
            if hz == want {
                crate::log_info!("[bluebox] DAP clock {} kHz", hz / 1000);
            } else {
                let base = if self.model == Some(bluebox::Model::Ic5000) { 30 } else { 100 };
                crate::log_info!(
                    "[bluebox] DAP clock {} kHz ({base} MHz / {div}; adapter speed {} kHz)",
                    hz / 1000,
                    want / 1000
                );
            }
            return true;
        }
        crate::log_warn!("[bluebox] the target does not answer at {} kHz; staying at {} kHz", hz / 1000, ATTACH_CLOCK_HZ / 1000);
        if let Some(p) = self.probe.as_mut() {
            let _ = p.set_clock_for(setup, ATTACH_CLOCK_HZ);
        }
        false
    }

    /// The clock this probe runs for `adapter speed` `hz`, and its divider.
    fn dap_clock(&self, hz: u32) -> (u32, u32) {
        bluebox::attach::dap_clock_for(self.model.unwrap_or_default(), hz)
    }

    /// With or without active probe; with, until an attach has looked.
    fn setup(&self) -> bluebox::attach::Setup {
        self.setup.unwrap_or(bluebox::attach::Setup::ActiveProbe)
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
        if !self.target_answers() {
            return Err(other("bluebox: the target does not answer after the reconnect"));
        }
        self.apply_clock();
        Ok(())
    }

    /// Runs a batch: one probe request per op. A probe-level error ends it.
    #[cfg(windows)]
    fn run_batch(&mut self, batch: &Batch) -> Result<Vec<OpResult>> {
        let ops: Vec<bluebox::Access> = batch
            .iter()
            .map(|op| match op {
                BatchOp::Read { addr, len, .. } => bluebox::Access::Read { addr: addr as u32, len },
                BatchOp::Write { addr, data, .. } => bluebox::Access::Write { addr: addr as u32, data },
            })
            .collect();
        // Batched (see `bluebox::Probe::access`): reads share a message, a write
        // has its own with the check read, and the results stop at the
        // first failed op, so the rest is consequential as on a TAS packet.
        let replies = self.probe()?.access(&ops).map_err(|e| other(format!("bluebox: {e}")))?;
        let mut out = Vec::with_capacity(ops.len());
        for (op, m) in ops.iter().zip(replies) {
            let len = op.len();
            let ok = m.bytes_ok(len);
            let err = if ok == len { tas::PL0_ERR_NO_ERROR } else { tas::PL0_ERR_DATA };
            let data = if op.is_write() {
                Vec::new()
            } else {
                let mut d = m.data;
                d.resize(len, 0);
                d[ok..].fill(0);
                d
            };
            out.push(OpResult { err, bytes_ok: ok, data });
        }
        while out.len() < ops.len() {
            out.push(OpResult { err: tas::PL0_ERR_CONSEQUENTIAL, bytes_ok: 0, data: Vec::new() });
        }
        Ok(out)
    }

    /// Makes winIDEA 9.21.356's kernel run on `probe`, given whose ran
    /// (`bluebox::Probe::kernel_state`). Returns a note for the status.
    #[cfg(windows)]
    fn ensure_kernel(&self, probe: &mut Probe, state: bluebox::KernelState) -> Result<String> {
        use bluebox::KernelState;
        if state == KernelState::Ours {
            return Ok("kernel was running".to_string());
        }
        let why = match state {
            KernelState::Foreign => "the probe runs another winIDEA version's kernel",
            _ => "the probe has no kernel running (it was power cycled)",
        };
        // Only winIDEA 9.21.356's kernel: this driver's service numbers and attach are that version's.
        let kpath = match &self.kernel {
            Some(k) => k.clone(),
            None => bluebox::kernel::find_kernel(self.winidea.as_deref(), probe.model()).map_err(|e| {
                other(format!(
                    "bluebox: {why}, and {e}. Install winIDEA {WINIDEA}, or point to its folder with \
                     'bluebox winidea <folder>' in the configuration"
                ))
            })?,
        };
        let k = bluebox::Kernel::from_file(&kpath).map_err(|e| other(format!("bluebox: {e}")))?;
        if state == KernelState::Foreign {
            crate::log_warn!("[bluebox] {why}: stopping it and loading {}", kpath.display());
        }
        let t0 = std::time::Instant::now();
        probe.ensure_kernel(state, Some(&k), |_| {}).map_err(|e| other(format!("bluebox: {e}")))?;
        crate::log_info!("[bluebox] {why}: loaded {} ({} bytes) in {} ms", kpath.display(), k.len(), t0.elapsed().as_millis());
        Ok(match state {
            KernelState::Foreign => format!("another version's kernel replaced from {}", kpath.display()),
            _ => format!("kernel loaded from {}", kpath.display()),
        })
    }

    /// The iC5000's debug FPGA image, from the winIDEA install the kernel comes
    /// from (`bluebox winidea`, the folder of `bluebox kernel`, or the usual
    /// folders). Read once.
    #[cfg(windows)]
    fn fpga_image(&mut self) -> Result<Vec<u8>> {
        if let Some(i) = &self.fpga_image {
            return Ok(i.clone());
        }
        let dir = match (&self.winidea, &self.kernel) {
            (Some(d), _) => Ok(d.clone()),
            (None, Some(k)) => Ok(k.parent().map(PathBuf::from).unwrap_or_default()),
            (None, None) => bluebox::kernel::find_kernel(None, bluebox::Model::Ic5000)
                .map(|k| k.parent().map(PathBuf::from).unwrap_or_default()),
        };
        let image = dir.and_then(|d| bluebox::fpga::ic5000::image_from(&d)).map_err(|e| {
            other(format!(
                "bluebox: the iC5000 loads its debug FPGA on every connect, from winIDEA {WINIDEA}'s files, and {e}. \
                 Install winIDEA {WINIDEA}, or point to its folder with 'bluebox winidea <folder>' in the configuration"
            ))
        })?;
        self.fpga_image = Some(image.clone());
        Ok(image)
    }

    #[cfg(windows)]
    fn connect(&mut self, settings: &AdapterSettings, first: bool) -> Result<DeviceInfo> {
        use bluebox::replay::Replay;
        use bluebox::usb::UsbLink;

        let bb = |e: bluebox::Error| other(format!("bluebox: {e}"));

        // The attach runs at the clock it was captured with (10 MHz): a faster one
        // fails when the attach has to bring a target up that is not yet in debug
        // mode (seen at 25 MHz). `adapter speed` is applied once it is connected.

        let path = UsbLink::open(settings.serial.as_deref()).map_err(bb)?.path().to_string();
        // Link bring-up. It ends any earlier client session; the attach below
        // makes the target connection again. A probe still booting, or with a
        // stuck kernel, is waited for and then restarted (`open_ready`).
        let serial = settings.serial.clone();
        let open = || {
            bluebox::lowlevel::open_ready(
                || UsbLink::open(serial.as_deref()),
                Duration::from_secs(20),
                |m| crate::log_warn!("[bluebox] {m}"),
            )
        };
        let (mut probe, hs) = open().map_err(bb)?;
        let model = probe.model();
        self.model = Some(model);
        let state = probe.kernel_state(&hs).map_err(bb)?;
        let kernel_note = self.ensure_kernel(&mut probe, state)?;
        // The iC5000 gets its debug FPGA on every connect, from winIDEA's files.
        let image = match model {
            bluebox::Model::Ic5000 => Some(self.fpga_image()?),
            bluebox::Model::Ic5700 => None,
        };

        // The attach to send: the built-in one for this model, or one taken from a capture.
        let (script, script_note) = match &self.replay {
            Some((trace, step)) => {
                // Its own bring-up (and kernel load, in a trace taken after a power
                // cycle) is left out: the driver does that part.
                let r = Replay::from_trace(trace, step).map_err(bb)?;
                (r.attach_script().map_err(bb)?, format!("attach from {}", trace.display()))
            }
            None => (bluebox::attach::Script::builtin(model), "built-in TC37x attach".to_string()),
        };
        // iC5700: the debug FPGA status, then the node scan, whose node count
        // tells whether an active probe sits between BlueBox and target; the
        // built-in attach continues with that setup's script. iC5000: the FPGA
        // upload inside the attach. See `bluebox::Probe::connect_target`.
        let t0 = std::time::Instant::now();
        let c = probe.connect_target(&script, self.replay.is_none(), image.as_deref()).map_err(bb)?;
        for n in &c.notes {
            crate::log_warn!("[bluebox] {script_note}: {n}");
        }
        self.setup = Some(c.setup);
        let what = match c.setup {
            bluebox::attach::Setup::ActiveProbe => "with active probe",
            bluebox::attach::Setup::System => "without active probe",
        };
        crate::log_info!("[bluebox] the {} is connected {what}", model.name());
        let script_note = format!("{script_note}, {} {what}", model.name());
        crate::log_info!(
            "[bluebox] {script_note}: {} exchanges in {} ms{}",
            c.sent,
            t0.elapsed().as_millis(),
            if c.other_len == 0 { String::new() } else { format!(", {} replies of another length than captured", c.other_len) }
        );
        for d in &c.other_desc {
            crate::log_debug!("[bluebox] other length at {d}");
        }
        let script = c.script;
        let id = probe.read_memory(SCU_CHIPID, 4).map_err(bb)?;
        if !id.all_ok() {
            return Err(other(format!(
                "bluebox: the attach ran ({script_note}) but the target does not answer: is it powered \
                 and is it a TC37x (the built-in attach is for that derivative)?"
            )));
        }
        let chipid = u32::from_le_bytes(id.data[..4].try_into().unwrap());
        self.probe = Some(probe);
        self.apply_clock();
        let probe = self.probe.as_mut().unwrap();
        if first && settings.connect_under_srst {
            // `reset_config connect_assert_srst`: connect with reset and halt.
            let st = probe.reset_halt().map_err(bb)?;
            crate::log_info!("[bluebox] connected under reset; cpu0 halted at 0x{:08x}", st.pc);
        }
        self.status = format!("{path}, SCU_CHIPID 0x{chipid:08x}");
        self.status.push_str(&format!(", {kernel_note}, {script_note}"));
        self.script = Some(script);
        // The JTAG id is not read here: where the probe exposes it is unknown,
        // and `ops::init` skips the `-expected-id` check on 0.
        Ok(DeviceInfo { device_id: 0, description: format!("BlueBox {}", self.status) })
    }
}

/// `adapter speed` in kHz to Hz. The probe runs the closest clock at or below
/// it (`bluebox::attach::dap_clock_for`): on an iC5700 100 MHz divided by an
/// integer, 1 to 25 MHz; on an iC5000 30 MHz divided by an integer, 2 to 30 MHz.
/// Which one is known once connected. 0 (OpenOCD's "adaptive" on some probes)
/// has no meaning here.
fn khz_to_hz(khz: u32) -> Result<u32> {
    if khz == 0 {
        return Err(Error::Usage("bluebox: adapter speed must be at least 1 kHz".into()));
    }
    Ok(khz.saturating_mul(1000))
}

/// The winIDEA version this driver's kernel, service numbers and attach are from.
const WINIDEA: &str = bluebox::kernel::WINIDEA_VERSION;

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

    /// `bluebox attach_replay <trace.jsonl> [step]`, `bluebox kernel <iC6kKrnl.s>`, `bluebox winidea <folder>`,
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
            Some("winidea") => {
                let p = args.get(2).ok_or_else(|| Error::Usage("usage: bluebox winidea <folder>".into()))?;
                self.winidea = Some(PathBuf::from(p));
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
            _ => Err(Error::Usage("usage: bluebox attach_replay|kernel|winidea|status|drop_target ...".into())),
        }
    }

    fn init(&mut self, settings: &AdapterSettings) -> Result<DeviceInfo> {
        #[cfg(windows)]
        {
            if let Some(khz) = settings.speed_khz {
                self.clock_hz = Some(khz_to_hz(khz)?);
            }
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

    /// Reads share probe messages, a write has one of its own (see the module
    /// docs). A failing op makes the rest of the batch consequential, as a TAS
    /// packet reports a fault part-way.
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

    /// A message takes 64 accesses, but each costs about 0.45 ms in the probe
    /// however they are packed: `max_num_rw` is what bounds a DAQ tick's time
    /// (2 x 32 reads, about 30 ms), not the message's capacity.
    fn limits(&self) -> BatchLimits {
        BatchLimits {
            max_num_rw: 32,
            max_rq: bluebox::memory::MAX_BATCH_REQUEST,
            max_rsp: bluebox::memory::MAX_BATCH_REPLY,
        }
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

    /// The debug clock (`//7//1//`). Before `init` the value is used by the
    /// attach; afterwards it is set at once and kept for reconnects and resets.
    fn set_speed_khz(&mut self, khz: u32) -> Result<()> {
        let hz = khz_to_hz(khz)?;
        self.clock_hz = Some(hz);
        #[cfg(windows)]
        {
            if self.inited && self.lost.is_none() && self.probe.is_some() && !self.apply_clock() {
                return Err(other(format!("bluebox: adapter speed: the target does not answer at {} kHz", self.dap_clock(hz).0 / 1000)));
            }
        }
        Ok(())
    }

    /// A run-state poll is two register reads per core, batched into one
    /// message: about 0.7 ms plus 0.45 ms per read, 4.3 ms with four cores.
    /// The server's 10 ms would keep the core thread almost half busy and queue
    /// frontend requests behind it; 15 ms keeps it under a third.
    fn default_poll_period_ms(&self) -> Option<u64> {
        Some(15)
    }

    /// An access costs about 450 us in the probe, a byte inside a read about
    /// 0.6 us (from 8 KiB reads at 25 MHz), so merging should pay up to a hole
    /// of about 700 bytes. 512 stays below that, as 96 does on a miniWiggler
    /// (break-even there about 105 bytes). Estimated, not measured with points.
    fn default_coalesce_gap(&self) -> Option<u32> {
        Some(512)
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
        a.command(&["bluebox", "winidea", "C:/winIDEA_9_21_356"]).unwrap();
        assert_eq!(a.winidea, Some(PathBuf::from("C:/winIDEA_9_21_356")));
        assert!(a.command(&["bluebox", "winidea"]).is_err());
        assert_eq!(a.command(&["bluebox", "status"]).unwrap(), "not connected");
        assert!(a.command(&["bluebox", "frobnicate"]).is_err());
        assert!(!a.initialized());
    }

    #[test]
    fn the_built_in_attach_splits_at_the_fpga_status() {
        let full = bluebox::attach::Script::tc37x();
        let (head, rest) = bluebox::attach::split_at_fpga_status(&full);
        let head = head.expect("the built-in attach asks for the FPGA status");
        let path = |x: &bluebox::Exchange| bluebox::message::decode(&x.payload).map(|(_, p, _)| p.to_string());
        let paths: Vec<_> = head.exchanges.iter().filter_map(path).collect();
        assert_eq!(paths, ["/////6/", "//1///12/", "//1///13/"], "what winIDEA sends before the status");
        assert_eq!(head.exchanges.len() + 1 + rest.exchanges.len(), full.exchanges.len());
        assert_eq!(rest.exchanges.last(), full.exchanges.last());
        let none = bluebox::attach::Script { exchanges: rest.exchanges[..10].to_vec() };
        assert!(bluebox::attach::split_at_fpga_status(&none).0.is_none());
    }

    #[test]
    fn the_connect_part_starts_at_the_last_l_command() {
        let full = bluebox::attach::Script::tc37x();
        let part = connect_part(&full);
        assert_eq!(part.exchanges.first().unwrap().payload, [0x03, 0x21, b'L']);
        assert!(part.exchanges.len() < full.exchanges.len() / 10, "{} of {}", part.exchanges.len(), full.exchanges.len());
        assert_eq!(part.exchanges.last(), full.exchanges.last());
        // The iC5000's has its second handshake too, and no FPGA upload in it.
        let ic5000 = bluebox::attach::Script::tc37x_ic5000();
        let part = connect_part(&ic5000);
        assert_eq!(part.exchanges.first().unwrap().payload, [0x03, 0x21, b'L']);
        assert!(part.exchanges.len() < ic5000.exchanges.len());
    }

    #[test]
    fn adapter_speed_before_init_is_kept_for_the_attach() {
        let mut a = BlueBoxAdapter::new();
        a.set_speed_khz(30_000).unwrap();
        assert_eq!(a.clock_hz, Some(30_000_000), "kept as asked: the model decides the rounding");
        assert_eq!(a.dap_clock(30_000_000), (25_000_000, 4), "iC5700 until connected: 100 MHz / 4");
        a.model = Some(bluebox::Model::Ic5000);
        assert_eq!(a.dap_clock(30_000_000), (30_000_000, 1), "iC5000: 30 MHz / 1");
        a.set_speed_khz(10_000).unwrap();
        assert_eq!(a.clock_hz, Some(10_000_000));
        assert!(a.set_speed_khz(0).is_err());
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
