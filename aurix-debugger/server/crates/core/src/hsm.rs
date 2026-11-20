//! The HSM after a reset (`hsm ...`): after a halting reset (the debugger's halt requested, OCDS on
//! during boot) the boot ROM sets a breakpoint on the HSM's first instruction, so the HSM stays
//! halted there, and a bootloader that waits for it (HSM2HTS, PROCONHSMCFG.SSWWAIT) hangs. The server
//! deletes the breakpoint and runs the HSM (`hsm autostart`, default on), and waits until it reports
//! ready before it latches TL1 or resumes the cores.
//!
//! No HSM target is needed for that (`tc3xx_hsm_target` only adds the HSM as a core to debug). The
//! server reads once per session whether the HSM boots (PROCONHSMCFG.HSMBOOTEN, a host register):
//! only then, or with an HSM target, does it look at the HSM after a reset, through the HSM debug
//! window, and releases it only when it is halted at the boot ROM breakpoint (FP_COMP0 enabled). A
//! board whose HSM does not boot gets no HSM access at all. If the HSM boots but its firmware has
//! closed the debug window, the HSM cannot be released: after a halting reset the server says so
//! (a running reset does not leave the HSM held). Not where the probe owns OCDS (winIDEA), nor in a
//! passive attach.

use std::time::{Duration, Instant};

use crate::adapter::Io;
use crate::cortex_m::{BRIDGE_BASE, C_DEBUGEN, DFSR, DFSR_ALL, DHCSR, DHCSR_KEY, S_HALT};
use crate::ctx::{Ctx, Cx};
use crate::target::{Core, TargetState};
use crate::{log_info, log_warn};

/// HSM2HTS (bridge + 0x34): READY | ACTIVE.
const HSM2HTS_OFF: u32 = 0x34;
const HSM2HTS_READY: u32 = 0x3;
const DBGBASE_OFF: u32 = 0x1010;
const FP_COMP0: u32 = 0xE000_2008;
/// DMU_SP_PROCONHSMCFG (TC3xx): HSMBOOTEN (bit 0), SSWWAIT (bit 1), installed from the UCB.
const PROCONHSMCFG: u32 = crate::ucb::SP_PROCONHSMCFG;
const HSMBOOTEN: u32 = 1 << 0;
const SSWWAIT: u32 = 1 << 1;

/// The HSM boot configuration, read once per session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootCfg {
    pub boots: bool,
    /// The SSW waits for the HSM (HSM2HTS) before it starts the application.
    pub ssw_waits: bool,
}

/// What the server knows of the HSM in this session (`hsm status`).
#[derive(Clone, Copy, Debug, Default)]
pub struct State {
    pub boot: Option<BootCfg>,
    pub last: Option<Release>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// `hsm autostart`: release the HSM from its boot ROM breakpoint after a reset.
    pub autostart: bool,
    /// `hsm ready_ms`: how long to wait for HSM2HTS READY|ACTIVE after the release.
    pub ready_ms: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { autostart: true, ready_ms: 2000 }
    }
}

/// The HSM's bridge, as the restore after a reset needs it (shared with the device watch).
#[derive(Clone, Copy, Debug, Default)]
pub struct Bridge {
    pub base: Option<u32>,
}

impl Bridge {
    fn window(base: u32, addr: u32) -> u32 {
        base + 0x1_0000 + (addr & 0xFFFF)
    }

    /// One access to an HSM debug register through the window (DBGBASE first).
    fn read(io: &mut Io, base: u32, addr: u32) -> crate::error::Result<u32> {
        io.write32(base + DBGBASE_OFF, addr & 0xFFFF_0000)?;
        io.read32(Self::window(base, addr))
    }

    fn write(io: &mut Io, base: u32, writes: &[(u32, u32)]) -> crate::error::Result<()> {
        let mut seq = Vec::new();
        for &(a, v) in writes {
            seq.push((base + DBGBASE_OFF, a & 0xFFFF_0000));
            seq.push((Self::window(base, a), v));
        }
        io.write32_seq(&seq)
    }

    /// Halting debug on for a running HSM (C_DEBUGEN), cleared by a power-on reset: without it the
    /// HSM does not stop with the cores. Only if it is off (an HSM held at the boot ROM breakpoint
    /// has it on, and a DHCSR write without C_HALT would release it). Quiet: right after a reset
    /// the window may not answer yet.
    pub fn debug_enable(&self, io: &mut Io) {
        let Some(base) = self.base else { return };
        if let Ok(d) = Self::read(io, base, DHCSR) {
            if d & C_DEBUGEN == 0 {
                let _ = Self::write(io, base, &[(DHCSR, DHCSR_KEY | C_DEBUGEN)]);
            }
        }
    }
}

/// The HSM target's bridge base, for the restore.
pub fn bridge(c: &Ctx) -> Bridge {
    let base = c.targets.iter().find_map(|e| if let Core::CortexM(m) = &e.core { Some(m.bridge_base()) } else { None });
    Bridge { base }
}

/// The bridge the release goes through: the HSM target's, else the TC3xx default.
fn release_base(c: &Ctx) -> u32 {
    bridge(c).base.unwrap_or(BRIDGE_BASE)
}

/// The HSM boot configuration (read once; `None` while it cannot be read).
pub fn boot_cfg(cx: &Cx) -> Option<BootCfg> {
    if let Some(b) = cx.with(|c| c.hsm_state.boot) {
        return Some(b);
    }
    let v = cx.with(|c| c.io().ok().and_then(|mut io| io.read32(PROCONHSMCFG).ok()))?;
    let b = BootCfg { boots: v & HSMBOOTEN != 0, ssw_waits: v & SSWWAIT != 0 };
    cx.with(|c| c.hsm_state.boot = Some(b));
    log_info!(
        "HSM boot: {}",
        match (b.boots, b.ssw_waits) {
            (false, _) => "off (no HSM access after resets)",
            (true, true) => "on, the startup waits for it (released after a halting reset)",
            (true, false) => "on (released after a halting reset)",
        }
    );
    Some(b)
}

/// What the release found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Release {
    /// Autostart off, no OCDS of ours, or neither an HSM that boots nor an HSM target.
    Off,
    /// Not halted at its boot ROM breakpoint (not held, or halted for another reason).
    NotHeld,
    /// The debug window does not answer (HSM debugging closed by its firmware).
    Blocked,
    Released,
    /// Still halted after the release.
    Failed,
}

impl Release {
    pub fn name(self) -> &'static str {
        match self {
            Release::Off => "off",
            Release::NotHeld => "not_held",
            Release::Blocked => "blocked",
            Release::Released => "released",
            Release::Failed => "failed",
        }
    }
}

/// After a reset: the HSM out of its boot ROM breakpoint (FP_COMP0 cleared, run) if it is held
/// there. A halted HSM that stays halted keeps its BRKOUT active: unrouted from TL1 then. `halting`:
/// after a halting reset, where an HSM that boots is held, so a closed window is worth a warning.
pub fn release(cx: &Cx, halting: bool) -> Release {
    let r = release_inner(cx, halting);
    cx.with(|c| c.hsm_state.last = Some(r));
    r
}

fn release_inner(cx: &Cx, halting: bool) -> Release {
    let on = cx.with(|c| c.hsm.autostart && c.restore.suspend.borrow().active);
    if !on {
        return Release::Off;
    }
    let target = cx.with(|c| bridge(c).base.is_some());
    let boot = boot_cfg(cx);
    let boots = boot.is_some_and(|b| b.boots);
    if !target && !boots {
        return Release::Off;
    }
    let base = cx.with(|c| release_base(c));
    // Right after a reset single window accesses may fail: a few tries before it counts as closed.
    let mut dhcsr = None;
    for i in 0..3 {
        dhcsr = cx.with(|c| c.io().ok().and_then(|mut io| Bridge::read(&mut io, base, DHCSR).ok()));
        if dhcsr.is_some() {
            break;
        }
        if i < 2 {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    let Some(d) = dhcsr else {
        if halting && boots {
            log_warn!(
                "HSM: it boots, but its debug access is closed (HSM firmware): it cannot be released from the boot ROM breakpoint \
                 after a halting reset. If the application hangs in its startup, use a running reset instead (reset run, or \
                 attach_mode halt to halt in the application)."
            );
        }
        return Release::Blocked;
    };
    if d & S_HALT == 0 {
        return Release::NotHeld;
    }
    let r = cx.with(|c| -> Release {
        let Ok(mut io) = c.io() else { return Release::Off };
        // Halted for another reason (a debugger halting the HSM itself): not ours to run.
        if Bridge::read(&mut io, base, FP_COMP0).is_ok_and(|fp| fp & 1 == 0) {
            return Release::NotHeld;
        }
        let run = DHCSR_KEY | C_DEBUGEN;
        if Bridge::write(&mut io, base, &[(FP_COMP0, 0), (DFSR, DFSR_ALL), (DHCSR, run)]).is_err() {
            return Release::Failed;
        }
        match Bridge::read(&mut io, base, DHCSR) {
            Ok(d) if d & S_HALT == 0 => Release::Released,
            _ => Release::Failed,
        }
    });
    match r {
        Release::Released => log_info!("HSM released from its boot ROM breakpoint"),
        Release::Failed => {
            log_warn!("HSM still halted after the release: unrouted from TL1 (a halted HSM keeps TL1 active)");
            let routed = cx.with(|c| c.restore.tl1.borrow().hsm.is_some());
            if routed {
                cx.with(|c| c.io().map(|mut io| io.write32(crate::tl1::CBS_TRHSM, 0)).ok());
            }
        }
        _ => {}
    }
    r
}

/// Waits for HSM2HTS READY|ACTIVE: the bootloader may start the application only then. Returns
/// whether the HSM reported ready.
pub fn wait_ready(cx: &Cx, ms: u64) -> bool {
    let base = cx.with(|c| release_base(c));
    let t0 = Instant::now();
    let mut v = 0;
    loop {
        if let Some(x) = cx.with(|c| c.io().ok().and_then(|mut io| io.read32(base + HSM2HTS_OFF).ok())) {
            v = x;
            if v & HSM2HTS_READY == HSM2HTS_READY {
                return true;
            }
        }
        if t0.elapsed() >= Duration::from_millis(ms) {
            log_warn!("HSM not ready after {ms} ms (HSM2HTS=0x{v:08x})");
            return false;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// The server's reset sequence, after the deassert (and for a halting reset, once cpu0 halted):
/// the HSM released, and given the time to report ready (~32 ms) before anything else: before a
/// resume that follows (`run_after`), and before the TL1 latch of a halting reset that stays
/// halted, which keeps the cores and the suspended modules stopped until continue. The latch halts
/// the HSM too: latched before it was ready, it reported ready only after continue, too late for a
/// bootloader that checks once, which then looped (TC375, measured).
pub fn after_own_reset(cx: &Cx, halt: bool, run_after: bool) {
    let released = release(cx, halt) == Release::Released;
    if released {
        let ms = cx.with(|c| c.hsm.ready_ms);
        wait_ready(cx, ms);
    }
    if halt && !run_after {
        let latch = cx.with(|c| {
            let p = c.restore.tl1.borrow();
            p.on && p.cores.first().is_some_and(|&(i, _)| c.targets[i].core.state() == TargetState::Halted)
        });
        if latch {
            cx.with(|c| c.io().map(|mut io| io.write32_seq(&crate::tl1::force_writes())).ok());
        }
    }
}

// --- Command -----------------------------------------------------------------------------------

const USAGE: &str = "usage: hsm autostart ?on|off? | hsm ready_ms ?ms? | hsm release | hsm wait_ready ?ms? | hsm status";

pub fn cmd_hsm(cx: &Cx, a: &[&str]) -> Result<String, String> {
    match (a.get(1).copied(), a.get(2).copied()) {
        (Some("autostart"), v) => {
            if let Some(v) = v {
                let on = crate::util::parse_bool(v)?;
                cx.with(|c| c.hsm.autostart = on);
            }
            Ok((if cx.with(|c| c.hsm.autostart) { "on" } else { "off" }).into())
        }
        (Some("ready_ms"), v) => {
            if let Some(v) = v {
                let ms = crate::util::parse_u64(v)?;
                cx.with(|c| c.hsm.ready_ms = ms);
            }
            Ok(cx.with(|c| c.hsm.ready_ms).to_string())
        }
        (Some("release"), None) => {
            if !cx.with(|c| c.initialized) {
                return Err("hsm release: run 'init' first".into());
            }
            Ok((if release(cx, true) == Release::Released { "1" } else { "0" }).into())
        }
        (Some("status"), None) => {
            let b = if cx.with(|c| c.initialized) { boot_cfg(cx) } else { None };
            let (target, last) = cx.with(|c| (bridge(c).base.is_some(), c.hsm_state.last));
            let flag = |v: Option<bool>| v.map_or("{}".to_string(), |v| (v as u8).to_string());
            Ok(format!(
                "boots {} ssw_waits {} target {} last {}",
                flag(b.map(|b| b.boots)),
                flag(b.map(|b| b.ssw_waits)),
                target as u8,
                last.map_or("{}", |l| l.name())
            ))
        }
        (Some("wait_ready"), v) => {
            let ms = match v {
                Some(v) => crate::util::parse_u64(v)?,
                None => cx.with(|c| c.hsm.ready_ms),
            };
            Ok((if wait_ready(cx, ms) { "1" } else { "0" }).into())
        }
        _ => Err(USAGE.into()),
    }
}
