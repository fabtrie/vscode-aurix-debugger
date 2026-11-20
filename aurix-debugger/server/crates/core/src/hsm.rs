//! The HSM after a reset (`hsm ...`): with a debugger connected the boot ROM sets a breakpoint on the
//! HSM's first instruction, so after a reset the HSM stays halted there, and a bootloader that waits
//! for it (HSM2HTS, PROCONHSMCFG.SSWWAIT) hangs. The server deletes the breakpoint and runs the HSM
//! (`hsm autostart`, default on), and before it resumes the cores after a halting reset waits until
//! the HSM reports ready.
//!
//! Only with an HSM target (`cortex_m`), and only on what the HSM shows: an HSM that does not boot
//! (no firmware, HSM boot off) is not halted, and one whose firmware closed HSM debugging does not
//! answer; both are left alone without a warning. Not where the probe owns OCDS (winIDEA).

use std::time::{Duration, Instant};

use crate::adapter::Io;
use crate::cortex_m::{C_DEBUGEN, DFSR, DFSR_ALL, DHCSR, DHCSR_KEY, S_HALT};
use crate::ctx::{Ctx, Cx};
use crate::target::{Core, TargetState};
use crate::{log_info, log_warn};

/// HSM2HTS (bridge + 0x34): READY | ACTIVE.
const HSM2HTS_OFF: u32 = 0x34;
const HSM2HTS_READY: u32 = 0x3;
const DBGBASE_OFF: u32 = 0x1010;
const FP_COMP0: u32 = 0xE000_2008;

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

/// What the release found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Release {
    /// No HSM target, autostart off, or no OCDS of ours.
    Off,
    /// The window does not answer (HSM debugging closed by its firmware) or the HSM is not halted
    /// (not booting, or not held): nothing to do.
    NotHeld,
    Released,
    /// Still halted after the release.
    Failed,
}

/// After a reset: the HSM out of its boot ROM breakpoint (FP_COMP0 cleared, run) if it is held
/// there. A halted HSM that stays halted keeps its BRKOUT active: unrouted from TL1 then.
pub fn release(cx: &Cx) -> Release {
    let (base, on) = cx.with(|c| (bridge(c).base, c.hsm.autostart && c.restore.suspend.borrow().active));
    let (Some(base), true) = (base, on) else { return Release::Off };
    let r = cx.with(|c| -> Release {
        let Ok(mut io) = c.io() else { return Release::Off };
        let Ok(d) = Bridge::read(&mut io, base, DHCSR) else { return Release::NotHeld };
        if d & S_HALT == 0 {
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
    let Some(base) = cx.with(|c| bridge(c).base) else { return true };
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
/// the HSM released; before a resume that follows (`run_after`) it reports ready first. A halting
/// reset that stays halted latches TL1, so that the cores and the suspended modules stay stopped
/// until continue (the edge halts the released HSM with them; continue releases it again).
pub fn after_own_reset(cx: &Cx, halt: bool, run_after: bool) {
    let released = release(cx) == Release::Released;
    if released && run_after {
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

const USAGE: &str = "usage: hsm autostart ?on|off? | hsm ready_ms ?ms? | hsm release | hsm wait_ready ?ms?";

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
            Ok((if release(cx) == Release::Released { "1" } else { "0" }).into())
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
