//! iSYSTEM BlueBox support, through a winIDEA instance driven by the SDK.
//!
//! # Why this adapter is shaped differently
//!
//! On the TAS path the server owns the whole debug stack: [`crate::tricore`]
//! halts, steps and sets breakpoints by writing AURIX OCDS registers as
//! ordinary memory, so the adapter only has to move bytes.
//!
//! winIDEA is itself a debugger and *owns* OCDS. It offers run control
//! directly and does not let a second master drive the debug registers
//! underneath it. So this adapter works at two levels:
//!
//! * as an [`Adapter`] it serves ordinary memory, which is enough for DAQ, OS
//!   awareness, stack unwinding, expressions, disassembly and `verify_image`;
//! * as a [`RunControl`] it takes over halt/resume/step/breakpoints, so the
//!   OCDS register paths are never reached.
//!
//! Only TriCore cores are taken over. The HSM and the GTM MCS channels use no
//! OCDS registers, so they keep working over plain memory accesses.
//!
//! # What this module does *not* do
//!
//! Nothing here opens the SDK. The adapter is constructed while the cfg script
//! is still being parsed ([`crate::commands`] creates it on `adapter driver`),
//! so connecting belongs in [`Adapter::init`] -- otherwise `adapter driver
//! winidea` on the first line of a cfg would launch winIDEA before the rest of
//! the configuration had even been read.

use std::any::Any;

use tas::{Batch, OpResult};

use crate::adapter::{Adapter, AdapterSettings, BatchLimits, DeviceInfo, FlashDownload};
use crate::error::{other, Error, Result};
use crate::target::{DebugReason, RunControl, RunMode, TargetState, Transition};
use crate::tricore::Tricore;

/// How winIDEA is obtained when the session starts.
///
/// These mirror `CConnectionConfig`'s start modes, so "attach if one is
/// running, otherwise launch" is the SDK's own behaviour rather than something
/// rebuilt out of instance enumeration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Start {
    /// Attach to a running winIDEA, launching one only if none is there.
    #[default]
    IfRequired,
    /// Attach only; fail when nothing is running.
    Existing,
    /// Always start a fresh instance.
    Always,
}

impl Start {
    pub fn parse(s: &str) -> Option<Start> {
        match s {
            "if-required" => Some(Start::IfRequired),
            "existing" => Some(Start::Existing),
            "always" => Some(Start::Always),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Start::IfRequired => "if-required",
            Start::Existing => "existing",
            Start::Always => "always",
        }
    }
}

// --- address policy -------------------------------------------------------

/// The per-core CSFR / debug window: `dbgbase + 0x10000 ..= + 0x1FFFF`, with
/// the TC3xx core bases at `0xF880_0000 + coreid * 0x20000`. DBGSR, the trigger
/// registers and the core's GPRs all live here.
const CSFR_WINDOW: std::ops::RangeInclusive<u32> = 0xF880_0000..=0xF88F_FFFF;

/// CBS registers that arm the OCDS trigger fabric (trigger lines, TL1
/// capture-and-hold, the HSM trigger routing) plus the OCDS enable key. These
/// are ordinary SFR addresses -- winIDEA reads and writes them happily -- but
/// writing them steers OCDS behind winIDEA's back.
const OCDS_CONTROL: &[u32] = &[
    crate::arch::OEC, // 0xF0000478, OCDS enable key
    0xF000_0490,      // CBS_TLC
    0xF000_0494,      // CBS_TL1ST
    0xF000_0498,      // CBS_TLCHE
    0xF000_049C,      // CBS_TLCHS
    0xF000_0438,      // CBS_TRHSM
];

/// Per-core trigger routing, `CBS_TRC<coreid> = 0xF0000420 + 4 * coreid`.
const CBS_TRC: std::ops::Range<u32> = 0xF000_0420..0xF000_0438;

/// Why a write was refused, for the error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Inside a core's debug/CSFR window.
    DebugWindow,
    /// An OCDS trigger-fabric or enable register.
    OcdsControl,
}

impl Refusal {
    pub fn what(self) -> &'static str {
        match self {
            Refusal::DebugWindow => "a core debug register",
            Refusal::OcdsControl => "an OCDS control register",
        }
    }
}

/// Whether a *write* to `addr` would steer OCDS, which winIDEA owns.
///
/// Reads are never refused: `tc3xx_connect_reset_halt` confirms a halt by
/// reading DBGSR, and the `tc3xx_stm_status` / `tc3xx_wdt_status` diagnostics
/// read ordinary SFRs. Refusing those would break scripts that work fine.
///
/// This is the backstop, not the mechanism -- the cfg guard in `tc3xx_ocds.cfg`
/// stops these writes being attempted at all.
pub fn refuse_write(addr: u32) -> Option<Refusal> {
    if CSFR_WINDOW.contains(&addr) {
        return Some(Refusal::DebugWindow);
    }
    if OCDS_CONTROL.contains(&addr) || CBS_TRC.contains(&addr) {
        return Some(Refusal::OcdsControl);
    }
    None
}

/// Maps a refusal or an SDK failure onto the PL0 status byte an [`OpResult`]
/// carries.
///
/// Everything above the adapter -- `adapter::check`, the DAQ sampler's error
/// classification -- already understands these, so reusing them keeps both
/// backends reporting failures the same way and leaves `daq.rs` untouched.
pub fn pl0_err(refusal: Option<Refusal>) -> u8 {
    match refusal {
        Some(_) => tas::PL0_ERR_ADDR_BLOCKED,
        None => tas::PL0_ERR_DATA,
    }
}

// --- the adapter ----------------------------------------------------------

/// Settings gathered from the cfg, plus the live session once `init` ran.
#[derive(Default)]
pub struct WinIdeaAdapter {
    pub workspace: Option<String>,
    pub start: Start,
    /// Declarations that do not apply to this backend, reported by
    /// `winidea status` and summarised once at `init`.
    ignored: Vec<String>,
    session: Option<crate::winidea_session::Session>,
    /// Addresses already warned about, so a retrying cfg does not bury the log.
    warned_writes: std::collections::HashSet<u32>,
    /// Synthesised trigger slots, indexed by slot, holding the address.
    slots: Vec<Option<u32>>,
    /// Register indices winIDEA answered "does not exist" for; read through the
    /// CSFR window from then on.
    unnamed_regs: std::collections::HashSet<usize>,
    /// Per core and ELF: whether winIDEA has loaded that ELF itself, i.e.
    /// whether a source step can be left to it.
    elf_matches: std::collections::HashMap<(u32, std::path::PathBuf), bool>,
}

impl WinIdeaAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a declaration that this backend cannot honour.
    ///
    /// Settings -- unlike actions -- are accepted rather than refused, because
    /// they appear in board cfg files we do not own and cannot guard: the
    /// stock `kit_a2g_tc377tx_3v3_trb.cfg` sets `adapter speed 5000` before
    /// anything else runs.
    pub fn ignore(&mut self, what: impl Into<String>) {
        let what = what.into();
        if !self.ignored.contains(&what) {
            self.ignored.push(what);
        }
    }

    pub fn ignored(&self) -> &[String] {
        &self.ignored
    }

    pub fn status(&self) -> String {
        let mut s = format!(
            "winidea: {}\n  workspace: {}\n  start: {}",
            match &self.session {
                Some(s) => format!("connected (winIDEA {})", s.version),
                None => "not connected".to_string(),
            },
            self.workspace.as_deref().unwrap_or("<none>"),
            self.start.name(),
        );
        if !self.ignored.is_empty() {
            s.push_str("\n  ignored on this backend: ");
            s.push_str(&self.ignored.join(", "));
        }
        s
    }
}

impl Adapter for WinIdeaAdapter {
    fn driver_name(&self) -> &'static str {
        "winidea"
    }

    fn command_groups(&self) -> &'static [&'static str] {
        &["winidea"]
    }

    /// `winidea workspace|start|status`.
    ///
    /// Pure field setting: this runs inside the `Ctx` borrow, so it must not
    /// call into the SDK. Connecting happens in [`Adapter::init`].
    fn command(&mut self, args: &[&str]) -> Result<String> {
        match args.get(1).copied() {
            Some("workspace") => {
                let p = args
                    .get(2)
                    .ok_or_else(|| Error::Usage("usage: winidea workspace <path>".into()))?;
                self.workspace = Some((*p).to_string());
                Ok(String::new())
            }
            Some("start") => {
                let m = args.get(2).ok_or_else(|| {
                    Error::Usage("usage: winidea start if-required|existing|always".into())
                })?;
                self.start = Start::parse(m).ok_or_else(|| {
                    Error::Usage(format!(
                        "unknown start mode '{m}' (if-required, existing, always)"
                    ))
                })?;
                Ok(String::new())
            }
            Some("status") => Ok(self.status()),
            _ => Err(Error::Usage(
                "usage: winidea workspace|start|status ...".into(),
            )),
        }
    }

    /// Connects, and reports what was learned.
    ///
    /// Nothing before this point has touched the SDK: the adapter is built
    /// while the cfg is still being read, so this is the first moment it is
    /// safe to launch or attach to winIDEA.
    fn init(&mut self, _settings: &AdapterSettings) -> Result<DeviceInfo> {
        let session =
            crate::winidea_session::Session::connect(self.workspace.as_deref(), self.start)?;
        if !self.ignored.is_empty() {
            crate::log_warn!(
                "[winidea] ignored on this backend: {} (they come from the board cfg and do not \
                 apply to a BlueBox)",
                self.ignored.join(", ")
            );
        }
        let description = format!("winIDEA {}", session.version);
        self.session = Some(session);
        // The OCMTS TAP id is a JTAG property winIDEA does not expose. `ops::init`
        // skips the `-expected-id` check on 0, which is the honest answer here --
        // better than inventing an id the cfg would then appear to have verified.
        Ok(DeviceInfo { device_id: 0, description })
    }

    fn initialized(&self) -> bool {
        self.session.is_some()
    }

    /// Detaches, leaving every winIDEA instance running: it is the user's
    /// debugger, and the session only ever attached to it.
    fn quit(&mut self) {
        if let Some(mut s) = self.session.take() {
            s.disconnect();
        }
    }

    /// Ordinary memory, one `Batch` operation at a time.
    ///
    /// Writes that would steer OCDS are refused here rather than sent: see
    /// [`refuse_write`]. Failures are reported with the same PL0 status bytes
    /// the TAS path uses, so `adapter::check` and the DAQ sampler classify them
    /// identically on both backends.
    fn execute(&mut self, batch: &Batch) -> Result<Vec<OpResult>> {
        let session = self.session.as_ref().ok_or_else(|| Error::Usage(NOT_CONNECTED.into()))?;
        let mut out = Vec::new();
        // A failing op makes the rest of the batch consequential, matching how
        // a TAS packet reports a fault part-way through.
        let mut failed = false;
        let mut refused: Vec<(u32, Refusal)> = Vec::new();
        for op in batch.iter() {
            if failed {
                out.push(OpResult {
                    err: tas::PL0_ERR_CONSEQUENTIAL,
                    bytes_ok: 0,
                    data: Vec::new(),
                });
                continue;
            }
            let r = match op {
                tas::BatchOp::Read { addr, len, .. } => match session.read(addr as u32, len) {
                    Ok(data) => OpResult { err: tas::PL0_ERR_NO_ERROR, bytes_ok: len, data },
                    Err(crate::winidea_session::ReadFailure::Partial { data, bytes_ok }) => {
                        OpResult { err: pl0_err(None), bytes_ok, data }
                    }
                    Err(crate::winidea_session::ReadFailure::Failed(e)) => {
                        crate::log_debug!("[winidea] read 0x{addr:08x}+{len}: {e}");
                        OpResult { err: pl0_err(None), bytes_ok: 0, data: vec![0; len] }
                    }
                    Err(crate::winidea_session::ReadFailure::NoSession) => {
                        return Err(Error::Usage(NOT_CONNECTED.into()))
                    }
                },
                tas::BatchOp::Write { addr, data, .. } => {
                    let a = addr as u32;
                    match refuse_write(a) {
                        Some(why) => {
                            refused.push((a, why));
                            OpResult { err: pl0_err(Some(why)), bytes_ok: 0, data: Vec::new() }
                        }
                        None => match session.write(a, data) {
                            Ok(()) => OpResult {
                                err: tas::PL0_ERR_NO_ERROR,
                                bytes_ok: data.len(),
                                data: Vec::new(),
                            },
                            Err(e) => {
                                crate::log_debug!("[winidea] write 0x{a:08x}: {e}");
                                OpResult { err: pl0_err(None), bytes_ok: 0, data: Vec::new() }
                            }
                        },
                    }
                }
            };
            failed = r.err != tas::PL0_ERR_NO_ERROR;
            out.push(r);
        }
        // After the loop: warning needs `&mut self`, and the session is
        // borrowed until here. Once per register -- a cfg that keeps retrying
        // should not bury the log.
        for (addr, why) in refused {
            if self.warned_writes.insert(addr) {
                crate::log_warn!(
                    "[winidea] refused a write to 0x{addr:08x}: it is {} and winIDEA owns OCDS \n                     on this backend",
                    why.what()
                );
            }
        }
        Ok(out)
    }
    /// One `Batch` operation is one IPC round trip to winIDEA, not one slot in
    /// a packet that carries many, so this is a round-trip budget. The value is
    /// deliberately conservative until it is measured against real hardware.
    fn limits(&self) -> BatchLimits {
        BatchLimits { max_num_rw: MAX_ROUND_TRIPS_PER_BATCH, max_rq: 0x1000, max_rsp: 0x1000 }
    }

    fn assert_srst(&mut self) -> Result<()> {
        // winIDEA has no assert/deassert split: the whole reset happens in
        // `deassert_srst`, which is where the halt/run choice is known.
        Ok(())
    }

    /// The whole reset, not just the release.
    ///
    /// winIDEA has no assert/deassert split, so `assert_srst` recorded the
    /// intent and this performs it in one go -- which is also the only point
    /// where the halt-or-run choice is known.
    fn deassert_srst(&mut self, halt_after_reset: bool) -> Result<()> {
        let session = self.session.as_ref().ok_or_else(|| Error::Usage(NOT_CONNECTED.into()))?;
        // The primary instance resets the SoC; the secondary cores come back
        // up with it.
        let mut exec = session.execution(0)?;
        if halt_after_reset {
            exec.reset().map_err(|e| other(format!("winidea: reset: {e}")))?;
        } else {
            exec.reset_and_run().map_err(|e| other(format!("winidea: reset and run: {e}")))?;
        }
        Ok(())
    }

    /// Always 0. Reporting winIDEA's own reset flag here would make the core
    /// treat every `reset` as a device reset: it blanks the DAQ baselines and
    /// marks every target poll-failed.
    fn take_resets(&mut self) -> u64 {
        0
    }

    /// A status query costs ~1.8 ms per halted core (0.16 ms running) on a
    /// BlueBox, so the default 10 ms poll kept the core thread about half busy
    /// with three cores and queued every frontend request behind it. 50 ms is
    /// still far below the ~250 ms winIDEA itself needs per stop.
    fn default_poll_period_ms(&self) -> Option<u64> {
        Some(50)
    }

    fn set_speed_khz(&mut self, _khz: u32) -> Result<()> {
        // A declaration, not an action: the BlueBox clock comes from the
        // winIDEA workspace. Record it rather than failing the cfg.
        self.ignore("adapter speed");
        Ok(())
    }

    /// Only once connected: before that the OCDS paths are still the right
    /// answer, and pretending otherwise would turn a missing session into a
    /// confusing run-control error.
    fn run_control(&mut self) -> Option<&mut dyn RunControl> {
        self.session.as_ref()?;
        Some(self)
    }

    /// winIDEA programs from its own download list, so the DMU driver does not
    /// apply: `flash write_image`, `write_images` and `program` hand their
    /// files here instead, and the DMU-only commands fail loudly.
    fn flash_download(&mut self) -> Option<&mut dyn FlashDownload> {
        Some(self)
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl FlashDownload for WinIdeaAdapter {
    /// Programs `files` through winIDEA, loading **no** symbols.
    ///
    /// The list is written through the option tree rather than
    /// `addToDownloadList`, whose parameters are not self-describing: setting a
    /// per-file "load symbols" flag by guessing a boolean is exactly the
    /// mistake that would not fail loudly. Here the fields are named.
    ///
    /// Two symbol tables for one target would disagree the moment the files
    /// differ, and every frontend reads ours, so this asserts afterwards that
    /// winIDEA still has none.
    fn download(&mut self, files: &[String]) -> Result<()> {
        let session = self.session.as_ref().ok_or_else(|| Error::Usage(NOT_CONNECTED.into()))?;
        let entries: Vec<String> = files
            .iter()
            .map(|f| {
                format!(
                    r#"{{"Path":"{}","LoadCode":true,"LoadSymbols":false}}"#,
                    json_escape(f)
                )
            })
            .collect();
        let json = format!(r#"{{"Files":[{}]}}"#, entries.join(","));

        let mut ide = session.ide()?;
        ide.set_option_json(winidea_sdk::loader::DOWNLOAD_FILES_OPTION, &json)
            .map_err(|e| other(format!("winidea: set download files: {e}")))?;

        session
            .loader()?
            .download()
            .map_err(|e| other(format!("winidea: download: {e}")))?;

        // `partitions()` empty is the SDK's own signal for "no symbol table".
        match session.data()?.partitions() {
            Ok(p) if !p.is_empty() => crate::log_warn!(
                "[winidea] the download loaded {} symbol partition(s); this server uses its own \n                 DWARF reader and winIDEA's symbols will be ignored",
                p.len()
            ),
            _ => {}
        }
        Ok(())
    }
}

const NOT_CONNECTED: &str =
    "the winidea backend has no session yet (winIDEA connection support is still being brought up)";

/// See [`Adapter::limits`]; revisit once the per-call cost is measured.
const MAX_ROUND_TRIPS_PER_BATCH: usize = 4;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_debug_windows_are_refused_for_writes() {
        // DBGSR of cpu0/1/2 = dbgbase + 0x10000 + 0xFD00.
        for dbgsr in [0xF881_FD00u32, 0xF883_FD00, 0xF885_FD00] {
            assert_eq!(refuse_write(dbgsr), Some(Refusal::DebugWindow), "{dbgsr:#x}");
        }
        // The core's GPRs live in the same window (d0 at +0xFF00).
        assert_eq!(refuse_write(0xF881_FF00), Some(Refusal::DebugWindow));
    }

    #[test]
    fn ocds_trigger_fabric_is_refused_for_writes() {
        for addr in
            [0xF000_0490u32, 0xF000_0494, 0xF000_0498, 0xF000_049C, 0xF000_0438, 0xF000_0478]
        {
            assert_eq!(refuse_write(addr), Some(Refusal::OcdsControl), "{addr:#x}");
        }
        // CBS_TRC0..5, one per core.
        for coreid in 0..6u32 {
            let a = 0xF000_0420 + 4 * coreid;
            assert_eq!(refuse_write(a), Some(Refusal::OcdsControl), "{a:#x}");
        }
    }

    #[test]
    fn ordinary_sfr_and_ram_stay_writable() {
        // The cfg's own diagnostics and the application's memory must not be
        // caught by the policy: blocking these would break scripts that work.
        for addr in [
            0x7000_0000, // DSPR
            0x8000_0000, // PFLASH
            0xF003_6050, // RSTSTAT
            0xF000_0470, // CBS_TLS, trigger-line state (read-only, not a control)
            0xF000_1000, // STM0
            0xF019_FD38, // GTM_OCS
            0xF004_0034, // HSM2HTS
            0xF005_EDF0, // HSM_DHCSR, through the bridge
        ] {
            assert_eq!(refuse_write(addr), None, "{addr:#x}");
        }
    }

    #[test]
    fn refusals_map_to_a_blocked_address_status() {
        assert_eq!(pl0_err(Some(Refusal::DebugWindow)), tas::PL0_ERR_ADDR_BLOCKED);
        assert_eq!(pl0_err(Some(Refusal::OcdsControl)), tas::PL0_ERR_ADDR_BLOCKED);
        assert_eq!(pl0_err(None), tas::PL0_ERR_DATA);
    }

    #[test]
    fn start_modes_round_trip() {
        for m in [Start::IfRequired, Start::Existing, Start::Always] {
            assert_eq!(Start::parse(m.name()), Some(m));
        }
        assert_eq!(Start::parse("nonsense"), None);
        assert_eq!(Start::default(), Start::IfRequired);
    }

    #[test]
    fn ignored_declarations_are_deduplicated_and_reported() {
        let mut a = WinIdeaAdapter::new();
        a.set_speed_khz(5000).unwrap();
        a.set_speed_khz(30000).unwrap();
        assert_eq!(a.ignored(), ["adapter speed"]);
        assert!(a.status().contains("adapter speed"));
    }

    #[test]
    fn commands_only_set_fields() {
        let mut a = WinIdeaAdapter::new();
        a.command(&["winidea", "workspace", "C:/ws/app.xjrf"]).unwrap();
        a.command(&["winidea", "start", "existing"]).unwrap();
        assert_eq!(a.workspace.as_deref(), Some("C:/ws/app.xjrf"));
        assert_eq!(a.start, Start::Existing);
        assert!(a.command(&["winidea", "start", "nope"]).is_err());
        assert!(a.command(&["winidea", "nonsense"]).is_err());
    }

    #[test]
    fn not_connected_until_init_succeeds() {
        let mut a = WinIdeaAdapter::new();
        assert!(!a.initialized());
        assert_eq!(a.take_resets(), 0);
        assert!(a.run_control().is_none());
        assert!(a.execute(&Batch::new()).is_err());
    }
}

/// Run control, delegated to winIDEA.
///
/// Only TriCore cores reach this: the HSM and the MCS channels use no OCDS
/// registers, so `Core` leaves them on the ordinary bus path.
///
/// Everything here reports through the core's own state machine -- the same
/// [`Transition`] and [`DebugReason`] the OCDS path produces -- so `wait_state`,
/// the OpenOCD events, the TCF stop notifications and the OS awareness all
/// behave as they do over TAS.
impl RunControl for WinIdeaAdapter {
    fn examine(&mut self, c: &mut Tricore) -> Result<()> {
        // A further core is attached here, as TAS examines every core at
        // `init`: winIDEA opens its window and hands back a connection.
        self.session
            .as_mut()
            .ok_or_else(|| Error::Usage(NOT_CONNECTED.into()))?
            .ensure_core(c.coreid)?;
        // Nothing to unlock: winIDEA enabled OCDS when it connected. Reading
        // the status is what proves the core is reachable.
        let st = self.cpu_status(c)?;
        c.examined = true;
        if not_started(&st) {
            // As the OCDS path reports a core the startup software has not
            // started yet (cpu1 right after a reset, an unused cpu2): halted,
            // boot-halt. The poll switches it to running once it starts.
            c.state = TargetState::Halted;
            c.reason = DebugReason::BootHalt;
            return Ok(());
        }
        self.apply_status(c, &st);
        Ok(())
    }

    fn poll(&mut self, c: &mut Tricore) -> Result<Transition> {
        let st = self.cpu_status(c)?;
        Ok(self.apply_status(c, &st))
    }

    fn halt(&mut self, c: &mut Tricore) -> Result<()> {
        self.exec_for(c)?.stop().map_err(|e| other(format!("winidea: halt: {e}")))
    }

    fn resume(&mut self, c: &mut Tricore, addr: Option<u32>, _mode: RunMode) -> Result<()> {
        let area = self.code_area();
        let mut exec = self.exec_for(c)?;
        if let Some(a) = addr {
            exec.goto_address(area.max(0) as u8, a as u64)
                .map_err(|e| other(format!("winidea: set pc: {e}")))?;
        }
        exec.run().map_err(|e| other(format!("winidea: resume: {e}")))?;
        // As the OCDS path does: the core is running from here on, whether or
        // not a poll gets to see it before the next query. Without this a
        // `resume` followed by `curstate` reported "halted" on hardware.
        c.state = TargetState::Running;
        c.reason = DebugReason::NotHalted;
        Ok(())
    }

    fn step(&mut self, c: &mut Tricore, addr: Option<u32>, mode: RunMode) -> Result<bool> {
        let area = self.code_area();
        let mut exec = self.exec_for(c)?;
        if let Some(a) = addr {
            exec.goto_address(area.max(0) as u8, a as u64)
                .map_err(|e| other(format!("winidea: set pc: {e}")))?;
        }
        // The frontends' source-level stepping is built on instruction steps
        // and its own temporary breakpoints, so that is all this has to be.
        // `StepOver`/`StepOut` arrive here only as the instruction-level part
        // of that sequence.
        let _ = mode;
        exec.step_instruction().map_err(|e| other(format!("winidea: step: {e}")))?;
        // `ops::step_as` announces the halt itself on `Ok(true)`, so the core
        // has to say why here: winIDEA's own flags after a step differ between
        // cores (cpu1 reported a debug request).
        c.state = TargetState::Halted;
        c.reason = DebugReason::SingleStep;
        Ok(true)
    }

    fn unmask_irq(&mut self, _c: &mut Tricore) -> Result<()> {
        // ICR.IE masking during steps is a winIDEA option (`DisableIRQ_Step*`),
        // applied to the session rather than poked per step, so there is
        // nothing to undo here.
        Ok(())
    }

    /// Registers by name, and the ones winIDEA does not name through the core's
    /// CSFR window.
    ///
    /// On a BlueBox (winIDEA 9.21.356, TC377) the GPRs, PCXI, PSW and PC are
    /// core registers by name, but the other system and debug CSFRs -- ICR, FCX,
    /// BTV, DBGSR, ... -- are not in winIDEA's register namespace at all. They
    /// are memory-mapped at `dbgbase + 0x10000 + offset`, and *reads* of that
    /// window are allowed (see [`refuse_write`]), so they are read from there.
    /// Which registers need that is learned from winIDEA's own "does not
    /// exist" answer rather than hard-coded, so a winIDEA that does name them
    /// is used as-is.
    fn read_regs(&mut self, c: &mut Tricore, idxs: &[usize]) -> Result<Vec<u32>> {
        let mut out = Vec::with_capacity(idxs.len());
        for &i in idxs {
            if !self.unnamed_regs.contains(&i) {
                let n = crate::winidea_regs::name(i)
                    .ok_or_else(|| other(format!("winidea: no register at index {i}")))?;
                // Bound first so the facade's borrow ends before the cache below
                // is updated.
                let read = self.debug_for(c)?.read_register(&n);
                match read {
                    Ok(v) => {
                        out.push(v.as_i64().unwrap_or(0) as u32);
                        continue;
                    }
                    Err(e) if crate::winidea_regs::is_unknown_register(&e.to_string()) => {
                        crate::log_debug!("[winidea] {n} is not a winIDEA register; reading its CSFR");
                        self.unnamed_regs.insert(i);
                    }
                    Err(e) => return Err(other(format!("winidea: read register {n}: {e}"))),
                }
            }
            out.push(self.read_csfr(c, i)?);
        }
        Ok(out)
    }

    fn write_reg(&mut self, c: &mut Tricore, idx: usize, value: u32) -> Result<()> {
        if self.unnamed_regs.contains(&idx) {
            // Writing it would mean writing the core's debug window, which is
            // winIDEA's to steer on this backend.
            let n = crate::arch::TRICORE.regs.get(idx).map_or("?", |d| d.name);
            return Err(other(format!(
                "winidea: register {n} is not writable on this backend (winIDEA does not expose it)"
            )));
        }
        let n = crate::winidea_regs::name(idx)
            .ok_or_else(|| other(format!("winidea: no register at index {idx}")))?;
        self.debug_for(c)?
            .write_register(&n, value as i64)
            .map_err(|e| other(format!("winidea: write register {n}: {e}")))
    }

    fn add_breakpoint(&mut self, c: &mut Tricore, addr: u32, _len: u32) -> Result<u8> {
        let area = self.code_area();
        let session = self.session()?;
        session
            .breakpoints(c.coreid)?
            .set_at_address(area, addr as u64)
            .map_err(|e| other(format!("winidea: breakpoint at 0x{addr:08x}: {e}")))?;
        Ok(self.claim_slot(addr))
    }

    fn add_watchpoint(&mut self, c: &mut Tricore, addr: u32, len: u32, access: u32) -> Result<u8> {
        let area = self.code_area();
        let hw = watch_access(access);
        let session = self.session()?;
        session
            .breakpoints(c.coreid)?
            .set_hw_at_address(hw, len, 0, area, addr as u64, 0)
            .map_err(|e| other(format!("winidea: watchpoint at 0x{addr:08x}: {e}")))?;
        Ok(self.claim_slot(addr))
    }

    fn remove_trigger(&mut self, c: &mut Tricore, slot: u8) -> Result<()> {
        let Some(addr) = self.release_slot(slot) else { return Ok(()) };
        let area = self.code_area();
        let session = self.session()?;
        let mut bp = session.breakpoints(c.coreid)?;
        // winIDEA keeps execution and hardware breakpoints in separate lists
        // and has no "whatever is here" removal, so try both; the one that was
        // never set simply reports nothing to remove.
        let _ = bp.delete_at_address(area, addr as u64);
        let _ = bp.delete_hw_at_address(area, addr as u64);
        Ok(())
    }

    fn restore_triggers(&mut self, c: &mut Tricore) -> Result<()> {
        // winIDEA re-applies its own breakpoints after a reset or download.
        self.session()?
            .breakpoints(c.coreid)?
            .reapply_all()
            .map_err(|e| other(format!("winidea: reapply breakpoints: {e}")))?;
        Ok(())
    }

    /// A whole source step by winIDEA, from its own symbol table.
    ///
    /// Stepping a line one instruction at a time costs ~250 ms per instruction
    /// here -- winIDEA's work per stop, whatever the step -- so a line of eight
    /// instructions took ~2 s, while winIDEA's own step over took ~220 ms
    /// (measured on a BlueBox). Only done when winIDEA has loaded the same ELF
    /// as ours, so the lines it steps to are the lines we show, and when it
    /// has a line at the PC: a high-level step elsewhere blocks inside winIDEA.
    fn source_step(
        &mut self,
        c: &mut Tricore,
        kind: crate::target::SourceStep,
        elf: &std::path::Path,
    ) -> Option<Result<bool>> {
        use crate::target::SourceStep;
        let session = self.session.as_ref()?;
        let key = (c.coreid, elf.to_path_buf());
        let same_elf = match self.elf_matches.get(&key) {
            Some(v) => *v,
            None => {
                let v = session.has_elf(c.coreid, elf).unwrap_or(false);
                if !v {
                    crate::log_info!(
                        "[winidea] core {}: winIDEA has not loaded {} itself, so source steps go \
                         instruction by instruction (slow on this backend)",
                        c.coreid,
                        elf.display()
                    );
                }
                self.elf_matches.insert(key, v);
                v
            }
        };
        if !same_elf {
            return None;
        }
        let pc = match self.cpu_status(c) {
            Ok(st) => st.execution_point as u32,
            Err(e) => return Some(Err(e)),
        };
        if kind != SourceStep::Out && !self.session.as_ref()?.has_line_at(c.coreid, pc) {
            return None;
        }
        let r = (|| -> Result<Option<DebugReason>> {
            let mut exec = self.exec_for(c)?;
            // Bounded, so a step over a slow call does not hold the core
            // thread; the poll reports the halt when it comes.
            let wait = winidea_sdk::TimeoutMode::OneSecond;
            // Right after a halt winIDEA can still be busy with its own stop
            // handling and answer "not available" once (seen on a BlueBox).
            let mut attempt = 0;
            let _ = loop {
                let r = match kind {
                    SourceStep::Over => exec.step_over_with(wait),
                    SourceStep::Into => exec.step_into_with(wait),
                    SourceStep::Out => exec.run_until_return_with(wait),
                };
                match r {
                    Err(e) if attempt < 3 && e.to_string().contains("ICONNECT_E_NOT_AVAILABLE") => {
                        attempt += 1;
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    r => break r.map_err(|e| other(format!("winidea: source step: {e}")))?,
                };
            };
            let st = exec.cpu_status().map_err(|e| other(format!("winidea: status: {e}")))?;
            Ok((st.is_stopped || st.is_halted).then(|| stop_reason(&st)))
        })();
        let r = r.map(|halted| {
            if let Some(reason) = halted {
                c.state = TargetState::Halted;
                // A user breakpoint the step ran into is reported as such.
                c.reason = match reason {
                    DebugReason::Breakpoint | DebugReason::Watchpoint => reason,
                    _ => DebugReason::SingleStep,
                };
            }
            halted.is_some()
        });
        match r {
            Ok(true) => {}
            Ok(false) => {
                c.state = TargetState::Running;
                c.reason = DebugReason::NotHalted;
            }
            Err(_) => {}
        }
        Some(r)
    }

    /// Only the cores a winIDEA instance debugs: core 0 and the mapped ones.
    fn drives(&self, c: &Tricore) -> bool {
        self.session.as_ref().is_some_and(|s| s.is_mapped(c.coreid))
    }

    fn after_reset(&mut self, c: &mut Tricore, _halt: bool) -> Result<Transition> {
        // As on the OCDS path, a core that was never examined is left alone:
        // `reset` visits every target, and an unmapped core would otherwise
        // abort the reset of the ones that are (seen on hardware).
        if !c.examined {
            return Ok(Transition::None);
        }
        // None of the OCDS work the TAS path does here applies: winIDEA
        // re-enabled debug itself, there is no halt-after-reset trigger of ours
        // to clear, and no SWEVT to write. Re-reading the status is the whole job.
        let st = self.cpu_status(c)?;
        let tr = self.apply_status(c, &st);
        // After a halting reset winIDEA reports the stop as a completed step,
        // which is what a frontend would then show. It is a reset.
        if c.state == TargetState::Halted {
            c.reason = DebugReason::Reset;
        }
        Ok(tr)
    }
}

/// Helpers the [`RunControl`] impl above leans on.
impl WinIdeaAdapter {
    fn session(&self) -> Result<&crate::winidea_session::Session> {
        self.session.as_ref().ok_or_else(|| Error::Usage(NOT_CONNECTED.into()))
    }

    fn code_area(&self) -> i32 {
        self.session.as_ref().map(|s| s.areas().code_area()).unwrap_or(0)
    }

    /// Run control for this core, through the winIDEA instance that debugs it.
    /// An unmapped core fails loudly (see `Session::conn_for`).
    fn exec_for(&self, c: &Tricore) -> Result<winidea_sdk::ExecutionController<'_>> {
        self.session()?.execution(c.coreid)
    }

    fn debug_for(&self, c: &Tricore) -> Result<winidea_sdk::DebugFacade<'_>> {
        self.session()?.debug(c.coreid)
    }

    /// A CSFR read straight from the core's debug window.
    fn read_csfr(&self, c: &Tricore, idx: usize) -> Result<u32> {
        let def = crate::arch::TRICORE
            .regs
            .get(idx)
            .ok_or_else(|| other(format!("winidea: no register at index {idx}")))?;
        if c.dbgbase.is_none() {
            return Err(other(format!(
                "winidea: {} needs the core's -dbgbase to be read on this backend",
                def.name
            )));
        }
        let addr = c.csfr(def.offset);
        match self.session()?.read(addr, 4) {
            Ok(b) if b.len() >= 4 => Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]])),
            Ok(_) => Err(other(format!("winidea: short read of {} at 0x{addr:08x}", def.name))),
            Err(crate::winidea_session::ReadFailure::Failed(e)) => {
                Err(other(format!("winidea: read {} at 0x{addr:08x}: {e}", def.name)))
            }
            Err(_) => Err(other(format!("winidea: read {} at 0x{addr:08x} failed", def.name))),
        }
    }

    fn cpu_status(&self, c: &Tricore) -> Result<winidea_sdk::CpuStatus> {
        self.exec_for(c)?.cpu_status().map_err(|e| other(format!("winidea: status: {e}")))
    }

    /// Folds a winIDEA status into the core's own state machine.
    ///
    /// Everything downstream of a stop -- the OpenOCD `halted` event, the TCF
    /// notification, the OS awareness read -- keys off this, so the mapping is
    /// what makes the two backends behave alike.
    fn apply_status(&self, c: &mut Tricore, st: &winidea_sdk::CpuStatus) -> Transition {
        // Still not started: nothing changed, and "not running" must not be
        // read as a resume of a boot-halted core.
        if not_started(st) {
            return Transition::None;
        }
        let prev = c.state;
        let halted = st.is_stopped || st.is_halted;
        if halted != (prev == TargetState::Halted) {
            crate::log_debug!(
                "[winidea] core {} status: running={} stopped={} halted={} idle={} waiting={} reset={} \
                 bp={} hwbp={} step={} explicit={} pc=0x{:08x}",
                c.coreid, st.is_running, st.is_stopped, st.is_halted, st.is_idle, st.is_waiting,
                st.is_reset, st.stopped_at_breakpoint, st.stopped_at_hw_breakpoint,
                st.stopped_after_step, st.stopped_explicitly, st.execution_point
            );
        }
        c.state = if st.is_reset {
            TargetState::Reset
        } else if halted {
            TargetState::Halted
        } else {
            TargetState::Running
        };
        if halted {
            // The reason is why the core stopped, so it is taken on the edge
            // only; a later poll must not replace the one a step or a reset set.
            if prev != TargetState::Halted {
                c.reason = stop_reason(st);
                return Transition::Halted { debug: prev == TargetState::DebugRunning, boot: false };
            }
        } else {
            c.reason = DebugReason::NotHalted;
            if prev == TargetState::Halted {
                return Transition::Running;
            }
        }
        Transition::None
    }

    /// Hands out a trigger slot for an address.
    ///
    /// The rest of the server identifies a planted breakpoint by the slot the
    /// OCDS trigger unit gave it. winIDEA has no such notion, so slots are
    /// synthesised and mapped back to addresses for removal.
    fn claim_slot(&mut self, addr: u32) -> u8 {
        if let Some(slot) = self.slots.iter().position(|s| *s == Some(addr)) {
            return slot as u8;
        }
        match self.slots.iter().position(|s| s.is_none()) {
            Some(slot) => {
                self.slots[slot] = Some(addr);
                slot as u8
            }
            None => {
                self.slots.push(Some(addr));
                (self.slots.len() - 1) as u8
            }
        }
    }

    fn release_slot(&mut self, slot: u8) -> Option<u32> {
        self.slots.get_mut(slot as usize).and_then(|s| s.take())
    }
}

/// A core winIDEA reports as neither running, stopped nor in reset: its window
/// has no core to debug yet. Seen on hardware for cpu1 before the application
/// starts it and for the unused cpu2 -- what the OCDS path calls boot-halt.
fn not_started(st: &winidea_sdk::CpuStatus) -> bool {
    !(st.is_running || st.is_stopped || st.is_halted || st.is_reset)
}

/// `WatchAccess` bits as the core encodes them (store = 1, load = 2).
fn watch_access(access: u32) -> winidea_sdk::HwAccess {
    match (access & 1 != 0, access & 2 != 0) {
        (true, true) => winidea_sdk::HwAccess::ReadWrite,
        (true, false) => winidea_sdk::HwAccess::Write,
        (false, true) => winidea_sdk::HwAccess::Read,
        (false, false) => winidea_sdk::HwAccess::Any,
    }
}

fn stop_reason(st: &winidea_sdk::CpuStatus) -> DebugReason {
    if st.stopped_at_breakpoint {
        DebugReason::Breakpoint
    } else if st.stopped_at_hw_breakpoint {
        DebugReason::Watchpoint
    } else if st.stopped_after_step {
        DebugReason::SingleStep
    } else if st.stopped_explicitly {
        DebugReason::DbgRq
    } else if st.is_reset {
        DebugReason::Reset
    } else {
        DebugReason::Undefined
    }
}

/// Escapes a path for the JSON the option tree takes.
///
/// Windows paths arrive with backslashes, which JSON would read as escapes.
/// winIDEA accepts forward slashes, so normalising is simpler and safer than
/// doubling them.
fn json_escape(path: &str) -> String {
    path.replace('\\', "/").replace('"', "\\\"")
}

#[cfg(test)]
mod download_tests {
    use super::*;

    #[test]
    fn windows_paths_become_json_safe() {
        assert_eq!(json_escape(r"C:\ws\app_dev.hex"), "C:/ws/app_dev.hex");
        assert_eq!(json_escape("C:/ws/app.elf"), "C:/ws/app.elf");
        // A quote in a path would otherwise end the JSON string early.
        assert_eq!(json_escape(r#"C:\a"b"#), r#"C:/a\"b"#);
    }
}

#[cfg(test)]
mod run_control_tests {
    use super::*;

    fn status() -> winidea_sdk::CpuStatus {
        winidea_sdk::CpuStatus {
            is_running: false,
            is_stopped: false,
            is_halted: false,
            is_idle: false,
            is_waiting: false,
            is_reset: false,
            is_attached: true,
            must_init: false,
            stopped_at_breakpoint: false,
            stopped_at_hw_breakpoint: false,
            stopped_after_step: false,
            stopped_explicitly: false,
            execution_point: 0,
            execution_area: 0,
            text: None,
        }
    }

    fn core() -> Tricore {
        let mut c = Tricore::default();
        c.state = TargetState::Unknown;
        c
    }

    #[test]
    fn a_stop_reports_why() {
        // The reason drives what a frontend shows at a stop, so each winIDEA
        // flag has to land on the matching DebugReason.
        let cases: [(fn(&mut winidea_sdk::CpuStatus), DebugReason); 4] = [
            (|s: &mut winidea_sdk::CpuStatus| s.stopped_at_breakpoint = true, DebugReason::Breakpoint),
            (|s: &mut winidea_sdk::CpuStatus| s.stopped_at_hw_breakpoint = true, DebugReason::Watchpoint),
            (|s: &mut winidea_sdk::CpuStatus| s.stopped_after_step = true, DebugReason::SingleStep),
            (|s: &mut winidea_sdk::CpuStatus| s.stopped_explicitly = true, DebugReason::DbgRq),
        ];
        for (set, want) in cases {
            let mut st = status();
            st.is_stopped = true;
            set(&mut st);
            assert_eq!(stop_reason(&st), want);
        }
        // Stopped for a reason winIDEA does not name is not "not halted".
        let mut st = status();
        st.is_stopped = true;
        assert_eq!(stop_reason(&st), DebugReason::Undefined);
    }

    #[test]
    fn transitions_fire_once_per_edge() {
        // handle_transition turns these into the OpenOCD events and the TCF
        // stop notifications, so a repeated poll must not re-announce a halt.
        let a = WinIdeaAdapter::new();
        let mut c = core();
        let mut st = status();

        st.is_running = true;
        assert!(matches!(a.apply_status(&mut c, &st), Transition::None));
        assert_eq!(c.state, TargetState::Running);

        st.is_running = false;
        st.is_stopped = true;
        st.stopped_explicitly = true;
        assert!(matches!(a.apply_status(&mut c, &st), Transition::Halted { .. }));
        assert_eq!(c.state, TargetState::Halted);
        assert_eq!(c.reason, DebugReason::DbgRq);

        // Still halted: no second event.
        assert!(matches!(a.apply_status(&mut c, &st), Transition::None));

        st.is_stopped = false;
        st.is_running = true;
        assert!(matches!(a.apply_status(&mut c, &st), Transition::Running));
        assert_eq!(c.reason, DebugReason::NotHalted);
    }

    /// Seen on hardware: after a step on cpu1 winIDEA reported a debug request,
    /// and a poll then overwrote the reason the step had set.
    #[test]
    fn a_poll_keeps_the_reason_the_halt_came_with() {
        let a = WinIdeaAdapter::new();
        let mut c = core();
        c.state = TargetState::Halted;
        c.reason = DebugReason::SingleStep;
        let mut st = status();
        st.is_stopped = true;
        st.stopped_explicitly = true;
        assert!(matches!(a.apply_status(&mut c, &st), Transition::None));
        assert_eq!(c.reason, DebugReason::SingleStep);
    }

    /// cpu1 before the application starts it: examined as boot-halted, left
    /// alone while it stays that way, and running once it starts.
    #[test]
    fn a_core_not_started_yet_is_boot_halted_until_it_runs() {
        let a = WinIdeaAdapter::new();
        let mut c = core();
        c.state = TargetState::Halted;
        c.reason = DebugReason::BootHalt;
        let mut st = status();
        st.is_attached = false;
        assert!(not_started(&st));
        assert!(matches!(a.apply_status(&mut c, &st), Transition::None));
        assert_eq!((c.state, c.reason), (TargetState::Halted, DebugReason::BootHalt));
        st.is_running = true;
        assert!(matches!(a.apply_status(&mut c, &st), Transition::Running));
        assert_eq!(c.state, TargetState::Running);
    }

    #[test]
    fn a_reset_is_not_reported_as_a_halt() {
        let a = WinIdeaAdapter::new();
        let mut c = core();
        let mut st = status();
        st.is_reset = true;
        a.apply_status(&mut c, &st);
        assert_eq!(c.state, TargetState::Reset);
    }

    #[test]
    fn slots_are_stable_per_address_and_reusable() {
        // The server removes a breakpoint by the slot it was given, so the
        // mapping has to survive a round trip and free up afterwards.
        let mut a = WinIdeaAdapter::new();
        let s0 = a.claim_slot(0x8000_1000);
        let s1 = a.claim_slot(0x8000_2000);
        assert_ne!(s0, s1);
        // Re-planting the same address reuses its slot rather than leaking one.
        assert_eq!(a.claim_slot(0x8000_1000), s0);

        assert_eq!(a.release_slot(s0), Some(0x8000_1000));
        assert_eq!(a.release_slot(s0), None, "releasing twice must not resurrect it");
        // The freed slot is handed out again.
        assert_eq!(a.claim_slot(0x8000_3000), s0);
        assert_eq!(a.release_slot(s1), Some(0x8000_2000));
    }

    #[test]
    fn watch_access_bits_map_to_winidea() {
        use winidea_sdk::HwAccess;
        // WatchAccess encodes store = 1, load = 2.
        assert_eq!(watch_access(1), HwAccess::Write);
        assert_eq!(watch_access(2), HwAccess::Read);
        assert_eq!(watch_access(3), HwAccess::ReadWrite);
        assert_eq!(watch_access(0), HwAccess::Any);
    }

}
