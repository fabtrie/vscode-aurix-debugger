//! The device watch: answers a device reset at the access that reports it, wherever that access
//! comes from (a Tcl command, a handler, flashing, the poll), instead of only in the background
//! poll between commands.
//!
//! [`Watched`] wraps the session's adapter. After every access it collects the resets the probe
//! reported (the TasServer attaches a reset notification to the next reply) and, for a reset it
//! did not cause itself, switches OCDS on again right away ([`ops::enable_ocds`]). While the boot
//! ROM still keeps the device locked, every following access tries again, for up to
//! [`GIVE_UP`]. Waits in Tcl (`sleep`, `after`) call [`Watched::idle_tick`], so a reset is also
//! answered while a script only waits.
//!
//! The reset itself still goes to the background poll ([`Adapter::take_resets`] counts it), which
//! re-examines the targets as before: triggers, the cfg's `examine-end` handlers. What changes is
//! how long the application runs without OCDS after the reset: one access instead of the rest of the
//! command. The time this took is bounded by the last moment the device was known not to have reset
//! yet or not to answer yet (the last clean access before the report, or the last failed attempt):
//! a restore later than the threshold is "late" ([`Watched::take_late`]), which the poll turns into
//! `ocds_found_off` (an application that checks for the debugger at start-up may have missed it).

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::adapter::{Adapter, AdapterSettings, BatchLimits, DeviceInfo, FlashDownload, LinkHealth, ProbeDaq, ScrDebug};
use tas::{Batch, OpResult};

use crate::error::Result;

/// How long failing restore attempts go on before the watch leaves the reset to the poll.
pub const GIVE_UP: Duration = Duration::from_secs(3);
/// Default threshold for a restore in time: an application that checks for the debugger does so
/// ~25 ms after it starts.
pub const DEFAULT_THRESHOLD_MS: u64 = 15;
/// Default slice of a Tcl wait: an idle wait accesses the device this often.
pub const DEFAULT_SLICE_MS: u64 = 5;

/// The watch's settings, kept in the context so that they can be set before `init`.
#[derive(Clone, Copy, Debug)]
pub struct WatchSettings {
    pub on: bool,
    pub threshold_ms: u64,
    pub slice_ms: u64,
}

impl Default for WatchSettings {
    fn default() -> Self {
        WatchSettings { on: true, threshold_ms: DEFAULT_THRESHOLD_MS, slice_ms: DEFAULT_SLICE_MS }
    }
}

/// What the watch saw of the resets it answered (`reset_info`).
#[derive(Clone, Copy, Debug, Default)]
pub struct ResetInfo {
    /// Resets seen while watching (not the server's own), since the connection.
    pub count: u64,
    /// When the last one was seen, in ms since the Unix epoch (as `clock milliseconds`).
    pub last_ms: u64,
    /// Upper bound of the time the device answered without OCDS before it was switched on again.
    pub latency_ms: Option<u64>,
    /// OCDS is on again after the last reset (false while restoring or after giving up).
    pub restored: bool,
    /// The last restore was later than the threshold, or failed.
    pub late: bool,
}

struct Restoring {
    /// When the reset was reported.
    since: Instant,
    /// The last moment the device was known not to answer yet (or, before any failed attempt, the
    /// last clean access before the report).
    bound: Option<Instant>,
}

/// The adapter of a session, watched for resets (see the module documentation).
pub struct Watched {
    inner: Box<dyn Adapter>,
    settings: WatchSettings,
    /// Whether the session's targets let the watch act (TriCore cores whose OCDS this server
    /// switches on: not on a probe that does run control itself).
    active: bool,
    /// The server's own reset sequence is running: its resets are not answered or counted here.
    own_reset: bool,
    /// Resets for [`Adapter::take_resets`] (the poll and the reset sequence).
    for_poll: u64,
    last_access: Instant,
    last_clean: Option<Instant>,
    restoring: Option<Restoring>,
    late: bool,
    info: ResetInfo,
    /// The set-up to restore after a reset, besides OCDS ([`crate::restore`]).
    restore: Option<crate::restore::SharedRestore>,
    /// [`Adapter::ram_writes`].
    ram_writes: u64,
}

/// Whether a write to `addr` (host address) goes to CPU RAM: DSPR/PSPR (global and local
/// segments), LMU/DLMU (cached and non-cached segments); not SFRs, flash or the trace RAM.
fn is_ram(addr: u64) -> bool {
    let a = addr as u32;
    let tram = (a & 0x0FFF_FFFF) >= 0x0800_0000;
    match a >> 28 {
        0x5 | 0x6 | 0x7 | 0xC | 0xD => true,
        0x9 | 0xB => !tram,
        _ => false,
    }
}

fn count_ram_writes(batch: &Batch) -> u64 {
    batch.iter().filter(|op| matches!(op, tas::BatchOp::Write { addr, .. } if is_ram(*addr))).count() as u64
}

impl Watched {
    pub fn new(inner: Box<dyn Adapter>) -> Watched {
        Watched {
            inner,
            settings: WatchSettings::default(),
            active: false,
            own_reset: false,
            for_poll: 0,
            last_access: Instant::now(),
            last_clean: None,
            restoring: None,
            late: false,
            info: ResetInfo::default(),
            restore: None,
            ram_writes: 0,
        }
    }

    /// What to set up again after a reset, once OCDS is back (set at `init`).
    pub fn set_restore(&mut self, rs: crate::restore::SharedRestore) {
        self.restore = Some(rs);
    }

    pub fn set_settings(&mut self, s: WatchSettings) {
        self.settings = s;
    }

    /// Whether the session lets the watch act (set after the examination in `init`).
    pub fn set_active(&mut self, active: bool) {
        self.active = active;
        if !active {
            self.restoring = None;
        }
    }

    fn watching(&self) -> bool {
        self.active && self.settings.on && !self.own_reset
    }

    /// Whether the watch acts now (on, active, not inside the server's own reset).
    pub fn is_watching(&self) -> bool {
        self.watching()
    }

    /// The server's own reset sequence starts (`true`) or has collected its resets (`false`): in
    /// between, resets are the sequence's own and are neither answered nor counted.
    pub fn set_own_reset(&mut self, on: bool) {
        if self.own_reset == on {
            return;
        }
        self.own_reset = on;
        // The sequence switches OCDS on itself and collects its resets; what was pending is its.
        self.restoring = None;
        if !on {
            self.last_clean = Some(Instant::now());
        }
    }

    pub fn info(&self) -> ResetInfo {
        self.info
    }

    /// A reset whose OCDS came back later than the threshold (or not at all) since the last call.
    pub fn take_late(&mut self) -> bool {
        std::mem::take(&mut self.late)
    }

    /// The slice a Tcl wait sleeps in while the watch acts, else `None` (sleep in one go).
    pub fn wait_slice(&self) -> Option<Duration> {
        (self.watching() && self.settings.slice_ms > 0 && self.inner.initialized())
            .then(|| Duration::from_millis(self.settings.slice_ms))
    }

    /// During a Tcl wait: one cheap access when nothing accessed the device for a slice (or a
    /// restore is pending), so that a reset is reported and answered while a script waits.
    pub fn idle_tick(&mut self) {
        let Some(slice) = self.wait_slice() else { return };
        if self.restoring.is_none() && self.last_access.elapsed() < slice {
            return;
        }
        let mut b = Batch::new();
        b.read32(crate::arch::OSTATE as u64);
        let r = self.inner.execute(&b);
        self.after_access(access_ok(&r));
    }

    /// Bookkeeping after an access of a caller: collect the resets it reported and answer them.
    fn after_access(&mut self, ok: bool) {
        if !self.active || !self.settings.on {
            return;
        }
        let now = Instant::now();
        self.last_access = now;
        let n = self.inner.take_resets();
        self.for_poll += n;
        if self.own_reset {
            return;
        }
        if n > 0 {
            self.info.count += n;
            self.info.last_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
            self.info.restored = false;
            self.info.latency_ms = None;
            if self.restoring.is_none() {
                self.restoring = Some(Restoring { since: now, bound: self.last_clean });
            }
            if let Some(rs) = &self.restore {
                crate::restore::mark_reset(rs);
            }
        } else if ok && self.restoring.is_none() {
            self.last_clean = Some(now);
        }
        if self.restoring.is_some() {
            self.try_restore();
        }
    }

    /// One attempt to switch OCDS on again after a reported reset.
    fn try_restore(&mut self) {
        let r = crate::ops::enable_ocds(&mut crate::adapter::Io::new(self.inner.as_mut()));
        let now = Instant::now();
        // The attempt's own replies may carry a further report: the same outage.
        self.for_poll += self.inner.take_resets();
        let Some(st) = self.restoring.as_mut() else { return };
        match r {
            Ok(_) => {
                let latency = st.bound.map(|b| now.duration_since(b));
                let late = latency.is_none_or(|l| l > Duration::from_millis(self.settings.threshold_ms));
                let ms = latency.map(|l| l.as_millis() as u64);
                self.info.restored = true;
                self.info.latency_ms = ms;
                self.info.late = late;
                self.late |= late;
                self.restoring = None;
                self.last_clean = Some(now);
                // The rest of what the reset cleared, right behind OCDS.
                if let Some(rs) = self.restore.clone() {
                    crate::restore::after_reset(&mut crate::adapter::Io::new(self.inner.as_mut()), &rs);
                    self.for_poll += self.inner.take_resets();
                }
                match ms {
                    Some(ms) if !late => crate::log_info!("Device reset: OCDS on again within {ms} ms"),
                    Some(ms) => crate::log_warn!(
                        "Device reset: OCDS on again only after up to {ms} ms (the access before was that long ago): \
                         an application that checks for the debugger at start-up may have missed it"
                    ),
                    None => crate::log_warn!("Device reset: OCDS on again; how long it was off is unknown"),
                }
            }
            Err(e) => {
                st.bound = Some(now);
                if now.duration_since(st.since) >= GIVE_UP {
                    self.restoring = None;
                    self.info.late = true;
                    self.late = true;
                    crate::log_warn!("Device reset: OCDS could not be switched on again for {} s ({e}); left to the poll", GIVE_UP.as_secs());
                }
            }
        }
    }
}

/// An access that went through with every op: the device answered.
fn access_ok(r: &Result<Vec<OpResult>>) -> bool {
    r.as_ref().is_ok_and(|v| v.iter().all(|o| o.ok()))
}

impl Adapter for Watched {
    fn driver_name(&self) -> &'static str {
        self.inner.driver_name()
    }
    fn command_groups(&self) -> &'static [&'static str] {
        self.inner.command_groups()
    }
    fn command(&mut self, args: &[&str]) -> Result<String> {
        self.inner.command(args)
    }
    fn init(&mut self, settings: &AdapterSettings) -> Result<DeviceInfo> {
        self.inner.init(settings)
    }
    fn initialized(&self) -> bool {
        self.inner.initialized()
    }
    fn execute(&mut self, batch: &Batch) -> Result<Vec<OpResult>> {
        self.ram_writes += count_ram_writes(batch);
        let r = self.inner.execute(batch);
        self.after_access(access_ok(&r));
        r
    }
    fn execute_packed(&mut self, batch: &Batch) -> Result<Vec<OpResult>> {
        self.ram_writes += count_ram_writes(batch);
        let r = self.inner.execute_packed(batch);
        self.after_access(access_ok(&r));
        r
    }
    fn write_bulk(&mut self, blocks: &[(u32, &[u8])]) -> Result<bool> {
        self.ram_writes += blocks.iter().filter(|(a, _)| is_ram(*a as u64)).count() as u64;
        let r = self.inner.write_bulk(blocks);
        self.after_access(matches!(r, Ok(true)));
        r
    }
    fn limits(&self) -> BatchLimits {
        self.inner.limits()
    }
    fn assert_srst(&mut self) -> Result<()> {
        self.inner.assert_srst()
    }
    fn deassert_srst(&mut self, halt_after_reset: bool) -> Result<()> {
        self.inner.deassert_srst(halt_after_reset)
    }
    fn take_resets(&mut self) -> u64 {
        std::mem::take(&mut self.for_poll) + self.inner.take_resets()
    }
    fn ram_writes(&self) -> u64 {
        self.ram_writes
    }
    fn reset_always_halts(&self) -> bool {
        self.inner.reset_always_halts()
    }
    fn set_speed_khz(&mut self, khz: u32) -> Result<()> {
        self.inner.set_speed_khz(khz)
    }
    fn run_control(&mut self) -> Option<&mut dyn crate::target::RunControl> {
        self.inner.run_control()
    }
    fn flash_download(&mut self) -> Option<&mut dyn FlashDownload> {
        self.inner.flash_download()
    }
    fn probe_daq(&mut self) -> Option<&mut dyn ProbeDaq> {
        self.inner.probe_daq()
    }
    fn scr_debug(&mut self) -> Option<&mut dyn ScrDebug> {
        self.inner.scr_debug()
    }
    fn default_poll_period_ms(&self) -> Option<u64> {
        self.inner.default_poll_period_ms()
    }
    fn default_coalesce_gap(&self) -> Option<u32> {
        self.inner.default_coalesce_gap()
    }
    fn health(&self) -> Option<LinkHealth> {
        self.inner.health()
    }
    fn background(&mut self) {
        self.inner.background()
    }
    fn quit(&mut self) {
        self.inner.quit()
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self.inner.as_any_mut()
    }
    fn watched(&mut self) -> Option<&mut Watched> {
        Some(self)
    }
}
