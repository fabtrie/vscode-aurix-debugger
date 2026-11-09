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
//! attach) is winIDEA 9.21.417's: service numbers differ from version to
//! version (9.21.356's node-1 to node-3 services were numbered differently),
//! so the driver works with that version only.
//!
//! 1. Link bring-up (`bluebox::lowlevel`): hello and handshake. A running
//!    kernel is asked whether it is winIDEA 9.21.417's. Another version's (the
//!    BlueBox was last used by another winIDEA) is stopped and replaced, as
//!    winIDEA does; nothing else may go to it. After a power cycle no kernel
//!    runs. Either way the driver loads the kernel (`iC6kKrnl.s`, `iC3kKrnl.s`
//!    on an iC5000, which it tells by its USB ids) and sets the session: from
//!    `bluebox kernel <file>`, or from a winIDEA 9.21.417 install, given by
//!    `bluebox winidea <folder>` or found in `%WINIDEA_DIR%`, `C:\winIDEA`,
//!    `C:\winIDEA*`, `C:\iSYSTEM\*` or Chocolatey's `lib\winidea.*` (its
//!    `deploy.json` gives the version). Without one, the error says which
//!    versions were found and asks for 9.21.417.
//! 2. The debug FPGA image. An iC5700 keeps it in flash: it must be
//!    9.21.417's. Another version's leaves the probe unusable for this driver;
//!    the error asks to connect once with winIDEA 9.21.417, which loads its
//!    image. An iC5000 gets it on every connect, as winIDEA does it: the driver
//!    takes it from the install's `ICWHW32.HRS` and uploads it inside the attach.
//! 3. The target connection (`bluebox::attach`): winIDEA 9.21.417's hot
//!    attach, built in. iC5000: `Script::tc37x_ic5000` (261 messages, about
//!    2.3 s), no node scan; it sets its DAP clock through node 5 (30 MHz
//!    divided by an integer, 2 to 30 MHz). iC5700: `Script::tc37x_system`
//!    (without active probe, 3532 messages) and `Script::tc37x` (with one,
//!    3705), the same up to their node-5 scan (`//5///22/ 00 ... c8`), whose
//!    node count (2 with an active probe, 1 without) picks the one the driver
//!    continues with, as winIDEA does. The setup also decides how the DAP
//!    clock is set: `//7//1//` (Hz) with an active probe, `//4///7/` (kHz)
//!    without. `bluebox attach_replay <trace.jsonl>` sends another captured
//!    attach message by message after the driver's own bring-up.
//!
//! Neither resets the target. The probe keeps no session across connects:
//! every `init` runs both steps.
//!
//! A target that does not answer after the attach (not powered, held in reset,
//! or reset over and over by an external watchdog that cannot be turned off)
//! does not fail `init`, as with a miniWiggler: the session starts, the core
//! leaves the cores unexamined, and the driver tries the target connection
//! again once a second until it answers (see Recovery). A `reset` then tries
//! the probe's reset at once, and so does `reset_config connect_assert_srst`
//! at connect.
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
//! Health ([`bluebox::health`]): the background poll asks the probe about the target connection
//! every 100 ms while all is well (`bluebox health_period`, 0 turns it off), every poll period
//! while the target is lost. The answer (about 1.15 ms, no target access) names the cause for logs
//! and frontends: no target supply, a reset (the probe latches it until it connects again), no
//! debug access. While the target is lost the probe is also asked before each reconnect attempt
//! (an attempt on a target without supply blocks for about 2.6 s): a missing supply holds the
//! attempts back (one every 5 s still runs), and its return starts one at once. The reset latch never holds anything back. A probe that answers
//! implausibly at connect (the target reads, the probe says otherwise) is not asked again on that
//! connection, nor one that refuses the query. A probe lost through USB (unplugged, powered off)
//! counts as gone: the core ends the frontends' sessions after `probe_lost_timeout` (1 s); other
//! probe errors keep the reopen every 3 s.
//!
//! ```tcl
//! adapter driver bluebox
//! adapter serial 01ABC                    ;# optional, when several probes are plugged in
//! bluebox winidea C:/winIDEA_9_21_417     ;# optional; a winIDEA 9.21.417 install (kernel, iC5000 FPGA image)
//! bluebox kernel C:/winIDEA/iC6kKrnl.s    ;# optional; that version's kernel file itself, unchecked
//! bluebox attach_replay C:/traces/x.jsonl ;# optional; another target or setup
//! bluebox health_period 100               ;# optional; ms between health queries, 0 = off
//! probe_lost_timeout 1000                 ;# optional; end the sessions after the probe is gone this long, 0 = never
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
//!
//! DAQ: the probe has a sampler of its own (`bluebox::daq`), offered to the core as
//! [`Adapter::probe_daq`]. The registry hands it plain bus points; the probe checks them in a loop
//! and buffers the changes, and the core drains the buffer instead of reading each point.
//! `bluebox daq_offload off` withdraws it, and every point is read by the host again. A probe
//! that refuses the DAQ services withdraws it by itself.

use std::any::Any;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tas::{Batch, BatchOp, OpResult};

use crate::adapter::{Adapter, AdapterSettings, BatchLimits, DeviceInfo, LinkHealth, ProbeDaq, ProbeDaqLimits, ProbeDrain, ProbeItem, ProbeSample};
use crate::error::{other, Error, Result};

const NOT_CONNECTED: &str = "bluebox: not connected (init has not run, or failed)";
const DEFAULT_STEP: &str = "connect: begin_attach";
/// Read once at init: proves the probe reaches the target.
const SCU_CHIPID: u32 = 0xF003_6140;

/// The DAP clock the built-in attach was captured with.
#[cfg(windows)]
const ATTACH_CLOCK_HZ: u32 = 10_000_000;

/// Interval of reconnect attempts after the target stopped answering, from the
/// end of one attempt to the start of the next. It doubles with each failed
/// attempt up to [`TARGET_RETRY_MAX`]: an attempt on an unpowered target takes
/// about 1.75 s (iC5700), and back to back they kept the core thread busy.
const TARGET_RETRY: Duration = Duration::from_secs(1);
/// Longest interval between target reconnect attempts.
const TARGET_RETRY_MAX: Duration = Duration::from_secs(5);
/// Interval of reconnect attempts after the probe was lost (each a full connect, ~3.5 s).
const PROBE_RETRY: Duration = Duration::from_secs(3);
/// How often the background poll asks the probe about the target connection while all is well
/// (`bluebox health_period`); while the target is lost or not well, every poll period.
const HEALTH_PERIOD: Duration = Duration::from_millis(100);
/// A health answer this old still decides whether a reconnect is worth trying.
#[cfg(windows)]
const HEALTH_FRESH: Duration = Duration::from_millis(100);
/// How long a lost target is tried with the probe's fast reconnect (`//2///34/`, as after its own
/// reset) before the attach's connect part. After an external reset it answers within ~40 ms.
const FAST_RECONNECT_FOR: Duration = Duration::from_millis(300);

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
    /// `bluebox winidea`: the folder of a winIDEA 9.21.417 install, for its kernel and the iC5000's FPGA image.
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
    /// When the last reconnect attempt ended.
    last_try: Option<Instant>,
    /// The current interval between target reconnect attempts (see [`TARGET_RETRY`]).
    target_retry: Option<Duration>,
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
    /// The probe is lost because USB failed or it is not plugged in (not a protocol or kernel
    /// error, which a reopen fixes): the core ends the sessions when this lasts.
    probe_gone: bool,
    /// The probe's view of the target connection (see [`bluebox::health`]), when last asked.
    health: Option<LinkHealth>,
    health_raw: Option<bluebox::TargetHealth>,
    health_at: Option<Instant>,
    /// `bluebox health_period <ms>`; `None`: [`HEALTH_PERIOD`]. Zero turns the query off.
    health_period: Option<Duration>,
    /// The probe refused the query or answered implausibly on this connection.
    health_off: bool,
    /// `bluebox daq_offload off`: no probe sampler for the core; every point is read by the host.
    daq_offload_off: bool,
    /// The probe refused a DAQ service: withdrawn for this adapter's life.
    daq_refused: bool,
    /// Bumped whenever the probe or target connection is rebuilt (see [`ProbeDaq::generation`]).
    daq_generation: u64,
    /// The sampler's limits, asked once per connection.
    daq_info: Option<bluebox::daq::Info>,
    /// The items last configured, and the handle the probe gave each.
    daq_items: Vec<bluebox::daq::Item>,
    daq_handles: Vec<u16>,
}

/// Whether a probe error means the probe itself is gone (unplugged, powered off, USB broken).
#[cfg(windows)]
fn probe_gone(e: &bluebox::Error) -> bool {
    matches!(e, bluebox::Error::Usb { .. } | bluebox::Error::NotFound(_))
}

/// What a health answer means for the target connection. The reset flag is a latch (it stays set
/// until the probe connects again), so it only names the cause while the connection is down.
fn derive_health(h: &bluebox::TargetHealth) -> LinkHealth {
    if !h.vref_ok {
        LinkHealth::Unpowered { vref_mv: h.vref_mv }
    } else if h.comm_works && h.has_access {
        LinkHealth::Ok
    } else if h.srst_active {
        LinkHealth::Reset
    } else {
        LinkHealth::NoDebugAccess
    }
}

impl BlueBoxAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reset with halt (see [`Adapter::deassert_srst`]).
    #[cfg(windows)]
    fn reset(&mut self) -> Result<()> {
        // With the target not answering, the probe's reset is tried at once: it
        // can catch a target that an external watchdog keeps resetting.
        let target_down = self.lost == Some(Lost::Target) && self.probe.is_some();
        if !target_down {
            self.ensure_connected()?;
        }
        let clock = self.clock_hz.map(|hz| self.dap_clock(hz).0);
        let setup = self.setup();
        let probe = self.probe()?;
        if clock.is_some_and(|hz| hz != ATTACH_CLOCK_HZ) {
            // The reconnect after the reset brings a fresh target into debug mode:
            // at the attach's clock, as the attach does.
            probe.set_clock_for(setup, ATTACH_CLOCK_HZ).map_err(|e| other(format!("bluebox: reset: {e}")))?;
        }
        let st = probe.reset_halt().map_err(|e| other(format!("bluebox: reset: {e}")))?;
        self.daq_connection_rebuilt();
        if target_down {
            if !self.target_answers() {
                return Err(other("bluebox: reset: the target does not answer"));
            }
            crate::log_info!("[bluebox] the target answers after the reset");
            self.lost = None;
        }
        self.apply_clock();
        if st.halted() {
            crate::log_info!("[bluebox] reset; cpu0 halted at 0x{:08x}", st.pc);
        } else {
            crate::log_warn!("[bluebox] after the reset cpu0 is not halted (DBGSR 0x{:08x})", st.dbgsr);
        }
        Ok(())
    }

    /// The probe or target connection was rebuilt: the sampler's item list may be gone.
    fn daq_connection_rebuilt(&mut self) {
        self.daq_generation += 1;
        self.daq_info = None;
        self.daq_items.clear();
        self.daq_handles.clear();
    }

    /// A DAQ service failed: a refused request withdraws the sampler, a USB or protocol error
    /// loses the probe as any other access would.
    #[cfg(windows)]
    fn daq_failed(&mut self, e: bluebox::Error, what: &str) -> Error {
        // `bluebox::daq` reports a reply that is not one (a refused request) with the service path.
        let refused = matches!(&e, bluebox::Error::Protocol(m) if m.starts_with("//1///") && !m.contains("reply ends"));
        if refused && !self.daq_refused {
            self.daq_refused = true;
            crate::log_warn!("[bluebox] the probe refused the DAQ {what} ({e}); points are read by the host");
        } else if !refused {
            self.lose_probe(&e, &format!("probe error in DAQ {what}: {e}"));
        }
        other(format!("bluebox: DAQ {what}: {e}"))
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

    fn health_period(&self) -> Duration {
        self.health_period.unwrap_or(HEALTH_PERIOD)
    }

    /// Takes a health answer: remembers it, and when the target supply came back while the target
    /// is lost, lets the next batch reconnect at once instead of after the retry interval.
    #[cfg(windows)]
    fn take_health(&mut self, raw: bluebox::TargetHealth) {
        let new = derive_health(&raw);
        if raw.app_reset && !self.health_raw.is_some_and(|h| h.app_reset) {
            crate::log_info!("[bluebox] the probe reports an application reset");
        }
        let was_unpowered = matches!(self.health, Some(LinkHealth::Unpowered { .. }));
        if was_unpowered && !matches!(new, LinkHealth::Unpowered { .. }) && self.lost == Some(Lost::Target) {
            crate::log_debug!("[bluebox] target supply back ({} mV): reconnecting now", raw.vref_mv);
            self.last_try = None;
            self.target_retry = None;
        }
        if self.health != Some(new) {
            crate::log_debug!("[bluebox] health {new:?} ({raw:?})");
        }
        self.health = Some(new);
        self.health_raw = Some(raw);
        self.health_at = Some(Instant::now());
    }

    /// Asks the probe once at connect, with the target answering: a probe that then reports
    /// anything but a working connection (another setup, another model) is not believed on this
    /// connection, so its answers can never hold back a reconnect.
    #[cfg(windows)]
    fn check_health_plausible(&mut self) {
        if self.health_period().is_zero() {
            return;
        }
        let Some(probe) = self.probe.as_mut() else { return };
        match probe.target_health() {
            Ok(raw) if derive_health(&raw) == LinkHealth::Ok => self.take_health(raw),
            Ok(raw) => {
                crate::log_warn!("[bluebox] health query off for this connection: the probe reports {raw:?} while the target answers");
                self.health_off = true;
            }
            Err(e) => {
                crate::log_debug!("[bluebox] health query off for this connection: {e}");
                self.health_off = true;
            }
        }
    }

    /// One health query, unless it is off: takes the answer, notes a lost probe (USB) or switches
    /// the query off for this connection (any other error).
    #[cfg(windows)]
    fn query_health(&mut self) {
        if self.health_off || self.health_period().is_zero() {
            return;
        }
        let Some(probe) = self.probe.as_mut() else { return };
        match probe.target_health() {
            Ok(raw) => self.take_health(raw),
            Err(e) if probe_gone(&e) => self.lose_probe(&e, &format!("probe error: {e}")),
            Err(e) => {
                crate::log_debug!("[bluebox] health query off for this connection: {e}");
                self.health_off = true;
            }
        }
    }

    /// Notes a probe error: lost, and whether because the probe itself is gone.
    #[cfg(windows)]
    fn lose_probe(&mut self, e: &bluebox::Error, why: &str) {
        self.probe_gone = probe_gone(e);
        self.lose(Lost::Probe, why);
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
        if kind == Lost::Target {
            // Ask the probe before an attempt (about 1.15 ms): an attempt on a target without
            // supply blocks for about 2.6 s and says nothing. Asked again on every call while the
            // supply is missing, so its return (also after a dip of a few ms) is seen at once.
            // An `Ok` answer is from before the loss.
            let ask = matches!(self.health, Some(LinkHealth::Unpowered { .. }) | Some(LinkHealth::Ok) | None);
            if ask || self.health_at.is_none_or(|t| t.elapsed() >= HEALTH_FRESH) {
                self.query_health();
                if self.lost == Some(Lost::Probe) {
                    return Err(Error::Adapter("bluebox: the probe is not connected; reconnecting".into()));
                }
            }
            // The probe says the target has no supply: no use trying until that changes
            // (`take_health` then starts an attempt at once), but still now and then in case the
            // answer is wrong.
            let unpowered = matches!(self.health, Some(LinkHealth::Unpowered { .. })) && self.health_at.is_some_and(|t| t.elapsed() < HEALTH_FRESH);
            if unpowered {
                let since = *self.last_try.get_or_insert_with(Instant::now);
                if since.elapsed() < TARGET_RETRY_MAX {
                    return Err(Error::Adapter("bluebox: the target is not powered; waiting for it".into()));
                }
            }
        }
        let wait = if kind == Lost::Target { self.target_retry.unwrap_or(TARGET_RETRY) } else { PROBE_RETRY };
        let what = if kind == Lost::Target { "the target does not answer" } else { "the probe is not connected" };
        if self.last_try.is_some_and(|t| t.elapsed() < wait) {
            return Err(Error::Adapter(format!("bluebox: {what}; reconnecting")));
        }
        let t0 = Instant::now();
        let r = match kind {
            Lost::Target => self.reconnect_target(),
            Lost::Probe => {
                let settings = self.settings.clone();
                self.connect(&settings, false).map(|_| ())
            }
        };
        self.last_try = Some(Instant::now());
        // Back off while the target stays away; a success starts over at TARGET_RETRY.
        let target_down = self.lost == Some(Lost::Target) && (r.is_err() || kind == Lost::Probe);
        self.target_retry = target_down.then(|| match self.target_retry {
            Some(d) if kind == Lost::Target => (d * 2).min(TARGET_RETRY_MAX),
            _ => TARGET_RETRY,
        });
        match r {
            // The probe is back, but `connect` found the target not answering: it
            // stays lost, and the next tries go to the target.
            Ok(()) if self.lost == Some(Lost::Target) && kind == Lost::Probe => {
                crate::log_info!("[bluebox] probe reconnected in {} ms; the target does not answer yet", t0.elapsed().as_millis());
                Err(Error::Adapter("bluebox: the target does not answer; reconnecting".into()))
            }
            Ok(()) => {
                self.lost = None;
                self.daq_connection_rebuilt();
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

    /// Reconnects to a target that stopped answering, in the probe session that is open: first
    /// with the probe's fast reconnect, then with the connect part of the attach.
    ///
    /// After a reset that followed one of the probe's own (an SBC whose RSTB is wired to the
    /// debugger's reset line) the probe's reconnect brings the target back within ~40 ms. After a
    /// reset the probe did not see coming (an SBC resetting a running application) it answers but
    /// the target stays unreadable; the attach's connect messages without polls
    /// ([`connect_core`]) bring it back within ~200 ms, the whole connect part takes ~0.85 s. An
    /// application that checks at start-up whether a debugger is connected (to turn the SBC's
    /// watchdog off) sees it only with the fast ways.
    #[cfg(windows)]
    fn reconnect_target(&mut self) -> Result<()> {
        if self.fast_reconnect() {
            return Ok(());
        }
        let part = match &self.script {
            Some(s) => connect_part(s),
            None => return Err(Error::Usage(NOT_CONNECTED.into())),
        };
        let probe = self.probe.as_mut().ok_or_else(|| Error::Usage(NOT_CONNECTED.into()))?;
        if let Err(e) = probe.attach(&part) {
            // The probe itself failed: next time, open it again.
            self.lose_probe(&e, &format!("probe error while reconnecting: {e}"));
            return Err(other(format!("bluebox: {e}")));
        }
        if !self.target_answers() {
            return Err(other("bluebox: the target does not answer after the reconnect"));
        }
        self.apply_clock();
        Ok(())
    }

    /// The probe's fast reconnect (`//2///34/`), tried every 1 ms for [`FAST_RECONNECT_FOR`]:
    /// whether the target answers again.
    #[cfg(windows)]
    fn fast_reconnect(&mut self) -> bool {
        // After a reset the probe did not do itself (an SBC resetting a running application), its
        // reconnect answers but the target stays unreadable (TC37x/FS26 board: for the whole 300 ms,
        // neither at 10 MHz nor with OCDS enabled; winIDEA too only got it back with a reset of its
        // own). The attach's connect messages without polls (connect_core, 34 exchanges, ~100 ms)
        // bring it back: 207 ms after the loss, in time for the application to see the debugger.
        // The connect part with its polls took ~850 ms.
        let core = self.script.as_ref().map(connect_core);
        let t0 = Instant::now();
        let mut last = String::new();
        let mut core_at: Option<Instant> = None;
        while t0.elapsed() < FAST_RECONNECT_FOR {
            let Some(probe) = self.probe.as_mut() else { return false };
            let why = match probe.core_state() {
                Ok(_) if self.target_answers() => {
                    self.apply_clock();
                    crate::log_debug!("[bluebox] fast reconnect after {} ms", t0.elapsed().as_millis());
                    return true;
                }
                Ok(s) => {
                    // After a reset the probe did not do itself, its reconnect answers but the target
                    // stays unreadable: run the attach's connect messages again (without its polls).
                    if let Some(c) = core.as_ref().filter(|_| core_at.is_none_or(|t| t.elapsed() >= Duration::from_millis(100))) {
                        core_at = Some(Instant::now());
                        let t1 = Instant::now();
                        let r = self.probe.as_mut().map(|p| p.attach(c));
                        let ok = matches!(r, Some(Ok(_))) && self.target_answers();
                        crate::log_debug!(
                            "[bluebox] short reconnect at {} ms ({} exchanges, {} ms): {}",
                            t0.elapsed().as_millis(),
                            c.exchanges.len(),
                            t1.elapsed().as_millis(),
                            if ok { "target answers" } else { "target does not answer" }
                        );
                        if ok {
                            self.apply_clock();
                            return true;
                        }
                    }
                    format!("reconnect answered (DBGSR {:08x}), SCU_CHIPID does not read", s.dbgsr)
                }
                Err(e) => format!("reconnect: {e}"),
            };
            if why != last {
                crate::log_debug!("[bluebox] fast reconnect at {} ms: {why}", t0.elapsed().as_millis());
                last = why;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        false
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
        let replies = match self.probe()?.access(&ops) {
            Ok(r) => r,
            Err(e) => {
                // Classified here, while the error still has its kind (see `execute`).
                self.probe_gone = probe_gone(&e);
                return Err(other(format!("bluebox: {e}")));
            }
        };
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

    /// Makes winIDEA 9.21.417's kernel run on `probe`, given whose ran
    /// (`bluebox::Probe::kernel_state`). Returns a note for the status.
    #[cfg(windows)]
    fn ensure_kernel(&self, probe: &mut Probe, state: bluebox::KernelState) -> Result<String> {
        use bluebox::KernelState;
        if state == KernelState::Ours {
            return Ok("kernel was running".to_string());
        }
        let why = match state {
            KernelState::Foreign => "the probe runs another winIDEA version's kernel",
            // After a power cycle, or a connect that stopped before loading one (`I` state 01).
            _ => "the probe has no kernel running",
        };
        // Only winIDEA 9.21.417's kernel: this driver's service numbers and attach are that version's.
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
        self.daq_connection_rebuilt();

        // The attach runs at the clock it was captured with (10 MHz): a faster one
        // fails when the attach has to bring a target up that is not yet in debug
        // mode (seen at 25 MHz). `adapter speed` is applied once it is connected.

        // A probe that cannot be opened at all is gone; other connect errors are not (they keep
        // the reopen every few seconds).
        self.probe_gone = false;
        self.health = None;
        self.health_raw = None;
        self.health_at = None;
        self.health_off = false;
        let path = match UsbLink::open(settings.serial.as_deref()) {
            Ok(l) => l.path().to_string(),
            Err(e) => {
                self.probe_gone = probe_gone(&e);
                return Err(bb(e));
            }
        };
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
        let (mut probe, hs) = match open() {
            Ok(x) => x,
            Err(e) => {
                self.probe_gone = probe_gone(&e);
                return Err(bb(e));
            }
        };
        let model = probe.model();
        self.model = Some(model);
        let builtin = bluebox::attach::Script::builtin(model);
        if builtin.is_none() && self.replay.is_none() {
            return Err(other(format!(
                "bluebox: the {} has no built-in winIDEA {WINIDEA} attach yet (its 9.21.356 attaches no longer fit). \
                 Give a captured one with 'bluebox attach_replay <trace.jsonl>'",
                model.name()
            )));
        }
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
            None => (builtin.expect("checked above"), "built-in TC37x attach".to_string()),
        };
        // iC5700: the debug FPGA status, then the node scan, whose node count
        // tells whether an active probe sits between BlueBox and target.
        // iC5000: the FPGA upload inside the attach. See `bluebox::Probe::connect_target`.
        let t0 = std::time::Instant::now();
        let c = probe.connect_target(&script, image.as_deref()).map_err(bb)?;
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
            "[bluebox] {script_note}: {} exchanges in {} ms{}{}",
            c.sent,
            t0.elapsed().as_millis(),
            if c.other_len == 0 { String::new() } else { format!(", {} replies of another length than captured", c.other_len) },
            // winIDEA's STMx_OCS writes: refused by an application that locked the STMs (SMU alarm,
            // and a reset where its reaction is armed); the cfg scripts suspend the STMs themselves.
            if c.skipped == 0 { String::new() } else { format!(", {} STMx_OCS writes left out", c.skipped) }
        );
        for d in &c.other_desc {
            crate::log_debug!("[bluebox] other length at {d}");
        }
        let script = c.script;
        self.probe = Some(probe);
        self.script = Some(script);
        if first && settings.connect_under_srst {
            // `reset_config connect_assert_srst`: connect with reset and halt, at the
            // attach's clock, also when the target did not answer the attach (a
            // board whose external watchdog keeps resetting it).
            let answered = self.target_answers();
            match self.probe.as_mut().unwrap().reset_halt() {
                Ok(st) => crate::log_info!("[bluebox] connected under reset; cpu0 halted at 0x{:08x}", st.pc),
                Err(e) if answered => return Err(bb(e)),
                Err(e) => crate::log_debug!("[bluebox] reset at connect: {e}"),
            }
        }
        let id = self.probe.as_mut().unwrap().read_memory(SCU_CHIPID, 4).map_err(bb)?;
        // The JTAG id is not read here: where the probe exposes it is unknown,
        // and `ops::init` skips the `-expected-id` check on 0.
        if !id.all_ok() {
            // As with a miniWiggler: the session starts without the target (not
            // powered, held in reset, or reset over and over by an external
            // watchdog). The core leaves its cores unexamined and examines them
            // again; each batch tries the target connection again, once a second.
            crate::log_warn!(
                "[bluebox] the target does not answer ({script_note}): not powered, held in reset or \
                 resetting itself? The session starts; the driver connects once the target answers"
            );
            self.lost = Some(Lost::Target);
            self.last_try = Some(Instant::now());
            self.status = format!("{path}, target not answering, {kernel_note}, {script_note}");
            return Ok(DeviceInfo { device_id: 0, description: format!("BlueBox {}", self.status) });
        }
        let chipid = u32::from_le_bytes(id.data[..4].try_into().unwrap());
        self.apply_clock();
        self.check_health_plausible();
        self.status = format!("{path}, SCU_CHIPID 0x{chipid:08x}, {kernel_note}, {script_note}");
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
/// The connect messages of the attach: from its last `L` to its first reset reconnect
/// (`//2///34/`), without the probe status polls (`//5///19/`) and FPGA status queries
/// (`//1///35/`) winIDEA sends in between. A short reconnect after a reset the probe did not do.
fn connect_core(script: &bluebox::attach::Script) -> bluebox::attach::Script {
    let part = connect_part(script);
    let path = |x: &bluebox::Exchange| bluebox::message::decode(&x.payload).map(|(_, p, _)| p.to_string());
    let end = part.exchanges.iter().position(|x| path(x).as_deref() == Some("//2///34/")).map_or(part.exchanges.len(), |i| i + 1);
    let exchanges = part.exchanges[..end]
        .iter()
        .filter(|x| !matches!(path(x).as_deref(), Some("//5///19/") | Some("//1///35/")))
        .cloned()
        .collect();
    bluebox::attach::Script { exchanges }
}

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
    /// `bluebox health_period <ms>`, `bluebox status`, `bluebox health` and `bluebox drop_target` (a test
    /// aid for recovery). Only `health` and `drop_target` talk to the probe; it is opened in [`Adapter::init`].
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
            // The probe's view of the target connection, asked now.
            Some("health") => {
                #[cfg(windows)]
                {
                    let raw = self.probe()?.target_health().map_err(|e| other(format!("bluebox: {e}")))?;
                    let h = derive_health(&raw);
                    Ok(format!(
                        "{}: Vref {} mV ({}), reset latch {}, OSTATE 0x{:08x}, access {}, comm {}, application reset {}",
                        h.text(),
                        raw.vref_mv,
                        if raw.vref_ok { "ok" } else { "out of range" },
                        if raw.srst_active { "set" } else { "clear" },
                        raw.ostate,
                        if raw.has_access { "yes" } else { "no" },
                        if raw.comm_works { "works" } else { "down" },
                        if raw.app_reset { "yes" } else { "no" }
                    ))
                }
                #[cfg(not(windows))]
                {
                    Err(Error::Usage(NOT_CONNECTED.into()))
                }
            }
            // How often the background poll asks the probe about the target connection (0: never).
            Some("health_period") => {
                if let Some(v) = args.get(2) {
                    let ms: u64 = v.parse().map_err(|_| Error::Usage("usage: bluebox health_period <ms>".into()))?;
                    self.health_period = Some(Duration::from_millis(ms));
                }
                Ok(self.health_period().as_millis().to_string())
            }
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
            // Protocol aid: one service message, sent as is; the reply as hex.
            // `bluebox raw <path> [hex body]`, e.g. `bluebox raw //3//3/6/ 00`.
            Some("raw") => {
                let usage = || Error::Usage("usage: bluebox raw <path> [hex body]".into());
                let path = args.get(2).ok_or_else(usage)?;
                let hex: String = args[3..].concat();
                let body = (0..hex.len())
                    .step_by(2)
                    .map(|i| hex.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok()))
                    .collect::<Option<Vec<u8>>>()
                    .ok_or_else(usage)?;
                #[cfg(windows)]
                {
                    let msg = bluebox::message::encode(bluebox::message::KIND, path, &body);
                    let reply = self.probe()?.transact(&msg).map_err(|e| other(format!("bluebox: {e}")))?;
                    Ok(reply.iter().map(|b| format!("{b:02x}")).collect())
                }
                #[cfg(not(windows))]
                {
                    let _ = (path, body);
                    Err(Error::Usage(NOT_CONNECTED.into()))
                }
            }
            // The probe's sampler for the core's DAQ points: on (default) or off.
            Some("daq_offload") => {
                match args.get(2).copied() {
                    Some("on") => self.daq_offload_off = false,
                    Some("off") => self.daq_offload_off = true,
                    None => {}
                    Some(_) => return Err(Error::Usage("usage: bluebox daq_offload [on|off]".into())),
                }
                Ok(if self.daq_offload_off { "off" } else if self.daq_refused { "refused by the probe" } else { "on" }.into())
            }
            // The sampler as the probe reports it, and the items configured.
            Some("daq") => {
                #[cfg(windows)]
                {
                    let probe = self.probe()?;
                    let info = probe.daq_info().map_err(|e| other(format!("bluebox: {e}")))?;
                    let st = probe.daq_control(bluebox::daq::Ctrl::Status).map_err(|e| other(format!("bluebox: {e}")))?;
                    let mut out = format!(
                        "{} ({} items of up to {} bytes, buffer {} samples), {} items, {} loops, longest pass {} us{}",
                        if st.enabled { "enabled" } else { "disabled" },
                        info.max_items,
                        info.max_item_size,
                        info.buffer_samples,
                        self.daq_items.len(),
                        st.loops,
                        st.max_loop_us,
                        if st.overflow { ", overflow" } else { "" }
                    );
                    for it in &self.daq_items {
                        out.push_str(&format!("\n  0x{:08x}:{} {:?}", it.addr, it.size, it.tier));
                    }
                    Ok(out)
                }
                #[cfg(not(windows))]
                {
                    Err(Error::Usage(NOT_CONNECTED.into()))
                }
            }
            _ => Err(Error::Usage("usage: bluebox attach_replay|kernel|winidea|status|health|health_period|drop_target|daq|daq_offload ...".into())),
        }
    }

    fn init(&mut self, settings: &AdapterSettings) -> Result<DeviceInfo> {
        #[cfg(windows)]
        {
            if let Some(khz) = settings.speed_khz {
                self.clock_hz = Some(khz_to_hz(khz)?);
            }
            // `connect` marks the target lost if it does not answer; the session starts anyway.
            self.lost = None;
            let info = self.connect(settings, true)?;
            self.settings = settings.clone();
            self.inited = true;
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
                    // `run_batch` noted whether the probe itself is gone.
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

    /// The probe's sampler, unless `bluebox daq_offload off` or the probe refused it.
    fn probe_daq(&mut self) -> Option<&mut dyn ProbeDaq> {
        if cfg!(windows) && self.inited && !self.daq_offload_off && !self.daq_refused {
            Some(self)
        } else {
            None
        }
    }

    /// The probe's view of the target connection, asked by [`Adapter::background`]: `ProbeLost`
    /// while USB is gone, `None` while the query is off or refused.
    fn health(&self) -> Option<LinkHealth> {
        if !self.inited {
            return None;
        }
        if self.lost == Some(Lost::Probe) {
            return self.probe_gone.then_some(LinkHealth::ProbeLost { why: "the BlueBox does not answer over USB" });
        }
        if self.health_off || self.health_period().is_zero() {
            return None;
        }
        self.health
    }

    /// The health query ([`bluebox::health`], about 1.15 ms): every `health_period` while all is
    /// well, every poll period while the target is lost or not well.
    fn background(&mut self) {
        #[cfg(windows)]
        {
            if !self.inited || self.health_off || self.health_period().is_zero() || self.lost == Some(Lost::Probe) {
                return;
            }
            let well = self.lost.is_none() && self.health == Some(LinkHealth::Ok);
            if well && self.health_at.is_some_and(|t| t.elapsed() < self.health_period()) {
                return;
            }
            self.query_health();
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl ProbeDaq for BlueBoxAdapter {
    /// What the probe reported on this connection; before that, what it reported when measured
    /// (256 items of up to 8 bytes).
    fn limits(&self) -> ProbeDaqLimits {
        let i = self.daq_info.unwrap_or(bluebox::daq::Info { max_items: 256, max_item_size: 8, buffer_samples: 4096 });
        ProbeDaqLimits { max_items: i.max_items as usize, max_item_size: i.max_item_size }
    }

    fn check_period_us(&self, period_us: u32) -> u32 {
        bluebox::daq::Tier::for_period_us(period_us).period_us().unwrap_or(0)
    }

    /// Disables the sampler, configures the items and enables it again, as winIDEA does.
    fn configure(&mut self, items: &[ProbeItem]) -> Result<()> {
        #[cfg(not(windows))]
        {
            let _ = items;
            Err(Error::Usage(NOT_CONNECTED.into()))
        }
        #[cfg(windows)]
        {
            use bluebox::daq::{Ctrl, Item, Tier};
            if !self.inited {
                return Err(Error::Usage(NOT_CONNECTED.into()));
            }
            self.ensure_connected()?;
            let want: Vec<Item> = items.iter().map(|i| Item { addr: i.addr, size: i.size, tier: Tier::for_period_us(i.period_us) }).collect();
            let need_info = self.daq_info.is_none();
            let probe = self.probe()?;
            /// The limits (if asked) and the handles, or the error and the step it failed in.
            type Configured = std::result::Result<(Option<bluebox::daq::Info>, Vec<u16>), (bluebox::Error, &'static str)>;
            let r = (|| -> Configured {
                let info = if need_info { Some(probe.daq_info().map_err(|e| (e, "info"))?) } else { None };
                probe.daq_control(Ctrl::Disable).map_err(|e| (e, "disable"))?;
                if want.is_empty() {
                    return Ok((info, Vec::new()));
                }
                let handles = probe.daq_configure(&want).map_err(|e| (e, "configure"))?;
                probe.daq_control(Ctrl::Enable).map_err(|e| (e, "enable"))?;
                Ok((info, handles))
            })();
            match r {
                Ok((info, handles)) => {
                    if info.is_some() {
                        self.daq_info = info;
                    }
                    self.daq_items = want;
                    self.daq_handles = handles;
                    Ok(())
                }
                Err((e, what)) => {
                    self.daq_items.clear();
                    self.daq_handles.clear();
                    Err(self.daq_failed(e, what))
                }
            }
        }
    }

    fn drain(&mut self) -> Result<ProbeDrain> {
        #[cfg(not(windows))]
        {
            Err(Error::Usage(NOT_CONNECTED.into()))
        }
        #[cfg(windows)]
        {
            if !self.inited {
                return Err(Error::Usage(NOT_CONNECTED.into()));
            }
            let gen = self.daq_generation;
            self.ensure_connected()?;
            if self.daq_generation != gen {
                // Reconnected just now: the registry configures again before draining.
                return Ok(ProbeDrain::default());
            }
            let r = self.probe()?.daq_read();
            let (st, samples) = match r {
                Ok(v) => v,
                Err(e) => return Err(self.daq_failed(e, "read")),
            };
            if !st.enabled && !self.daq_items.is_empty() {
                // The probe dropped the item list (its kernel restarted): configure again.
                crate::log_warn!("[bluebox] the probe's DAQ is disabled; configuring it again");
                self.daq_connection_rebuilt();
            }
            let more = samples.len() >= bluebox::daq::READ_MAX as usize;
            let handles = &self.daq_handles;
            let samples = samples
                .into_iter()
                .filter_map(|s| {
                    let item = handles.iter().position(|&h| h == s.handle)?;
                    Some(ProbeSample { item, time_us: s.time_us, data: s.data })
                })
                .collect();
            Ok(ProbeDrain { time_us: st.time_us, samples, overflow: st.overflow, more })
        }
    }

    fn generation(&self) -> u64 {
        self.daq_generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_answers_name_the_cause() {
        let ok = bluebox::TargetHealth { vref_mv: 4990, vref_ok: true, has_access: true, comm_works: true, ostate: 1, ..Default::default() };
        assert_eq!(derive_health(&ok), LinkHealth::Ok);
        // The reset flag is a latch: with the connection back it is no reason.
        assert_eq!(derive_health(&bluebox::TargetHealth { srst_active: true, ..ok }), LinkHealth::Ok);
        let down = bluebox::TargetHealth { comm_works: false, ..ok };
        assert_eq!(derive_health(&bluebox::TargetHealth { srst_active: true, ..down }), LinkHealth::Reset);
        assert_eq!(derive_health(&down), LinkHealth::NoDebugAccess);
        // No supply wins over everything else (the probe then sends no debug subsystem entry).
        assert_eq!(derive_health(&bluebox::TargetHealth { vref_mv: 609, ..Default::default() }), LinkHealth::Unpowered { vref_mv: 609 });
    }

    #[test]
    fn commands_only_set_fields() {
        let mut a = BlueBoxAdapter::new();
        a.command(&["bluebox", "attach_replay", "C:/t.jsonl"]).unwrap();
        assert_eq!(a.replay, Some((PathBuf::from("C:/t.jsonl"), DEFAULT_STEP.to_string())));
        a.command(&["bluebox", "attach_replay", "x.jsonl", "connect: other"]).unwrap();
        assert_eq!(a.replay.as_ref().unwrap().1, "connect: other");
        a.command(&["bluebox", "kernel", "C:/winIDEA/iC6kKrnl.s"]).unwrap();
        assert_eq!(a.kernel, Some(PathBuf::from("C:/winIDEA/iC6kKrnl.s")));
        a.command(&["bluebox", "winidea", "C:/winIDEA_9_21_417"]).unwrap();
        assert_eq!(a.winidea, Some(PathBuf::from("C:/winIDEA_9_21_417")));
        assert!(a.command(&["bluebox", "winidea"]).is_err());
        assert_eq!(a.command(&["bluebox", "status"]).unwrap(), "not connected");
        assert_eq!(a.command(&["bluebox", "health_period"]).unwrap(), "100");
        assert_eq!(a.command(&["bluebox", "health_period", "0"]).unwrap(), "0");
        assert_eq!(a.health_period, Some(Duration::ZERO));
        assert!(a.command(&["bluebox", "health_period", "x"]).is_err());
        assert_eq!(a.health(), None);
        assert!(a.command(&["bluebox", "frobnicate"]).is_err());
        assert!(!a.initialized());
    }

    #[test]
    fn an_attach_splits_at_the_fpga_status() {
        // An iC5700 attach starts /////6/, //1///12/, //1///13/, then the status query.
        let x = |path: &str| bluebox::Exchange {
            payload: bluebox::message::encode(1, path, &[0]),
            fetch: bluebox::Fetch::new(0x0e, 0xf000, 2),
            reads: vec![62480],
            expect: vec![vec![0; 32]],
            pause_ms: 0,
        };
        let full = bluebox::attach::Script {
            exchanges: ["/////6/", "//1///12/", "//1///13/", bluebox::fpga::STATUS, "//1///43/"].map(x).to_vec(),
        };
        let (head, rest) = bluebox::attach::split_at_fpga_status(&full);
        assert_eq!(head.expect("a status query").exchanges.len(), 3);
        assert_eq!(rest.exchanges, [x("//1///43/")]);
        assert!(bluebox::attach::split_at_fpga_status(&rest).0.is_none());
    }

    #[test]
    fn the_connect_part_starts_at_the_last_l_command() {
        // The iC5000's attach has its second handshake, and no FPGA upload in it.
        let full = bluebox::attach::Script::tc37x_ic5000();
        let part = connect_part(&full);
        assert_eq!(part.exchanges.first().unwrap().payload, [0x03, 0x21, b'L']);
        assert!(part.exchanges.len() < full.exchanges.len());
        assert_eq!(part.exchanges.last(), full.exchanges.last());
    }

    /// The short reconnect: the connect messages from the last `L` to the first reset reconnect,
    /// without polls (measured on the iC5700 without active probe: 34 exchanges, ~100 ms, where
    /// the connect part took ~850 ms).
    #[test]
    fn the_connect_core_runs_from_l_to_the_reset_reconnect_without_polls() {
        for script in [bluebox::attach::Script::tc37x_system(), bluebox::attach::Script::tc37x(), bluebox::attach::Script::tc37x_ic5000()] {
            let core = connect_core(&script);
            let path = |x: &bluebox::Exchange| bluebox::message::decode(&x.payload).map(|(_, p, _)| p.to_string());
            assert_eq!(core.exchanges.first().unwrap().payload, [0x03, 0x21, b'L']);
            assert_eq!(path(core.exchanges.last().unwrap()).as_deref(), Some("//2///34/"));
            assert!(core.exchanges.iter().all(|x| !matches!(path(x).as_deref(), Some("//5///19/") | Some("//1///35/"))));
            assert!(core.exchanges.len() < 60, "{} exchanges", core.exchanges.len());
        }
        assert_eq!(connect_core(&bluebox::attach::Script::tc37x_system()).exchanges.len(), 34);
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
