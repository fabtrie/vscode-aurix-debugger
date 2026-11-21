//! How a session attaches to the board (`attach_mode`, run by `init`), and the run control a board
//! policy needs around it (`halt_all`, `halt_check_hook`).
//!
//! - `passive`: no register written: the targets are not examined (so OCDS is not switched on and
//!   no trigger is set), the device watch, TL1 sync and peripheral suspend stay off; memory reads
//!   work, resets and halts are refused. The probe's own connection is not covered.
//! - `hot` (default): attach to what runs, without reset or halt: the set-up a reset clears is
//!   written (TL1 routing, peripheral suspend), and what could not be is reported.
//! - `halt`: `hot`, then, if a halt is not safe now, a running reset first (through the board's
//!   `reset_hook`) and `attach_run_ms` of the application running; then all cores halted where the
//!   application is. Not safe: peripheral modules the application locked before their suspend was
//!   written (only a reset gives them another chance), or what the board's `halt_check_hook` says.
//! - `reset_halt`, `reset_run`: `reset halt`, `reset run` (through `reset_hook`).
//!
//! The mode runs after the cfg's `init_board`, so that a board policy has set up its state first.

use std::time::{Duration, Instant};

use crate::arch::{DBGSR, DBGSR_HALTED, DBGSR_HALT_REQ};
use crate::ctx::Cx;
use crate::error::{other, Result};
use crate::suspend::{Select, State, Why};
use crate::target::Core;
use crate::{log_info, log_user, log_warn};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    Passive,
    #[default]
    Hot,
    Halt,
    ResetHalt,
    ResetRun,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "passive" => Some(Mode::Passive),
            "hot" => Some(Mode::Hot),
            "halt" => Some(Mode::Halt),
            "reset_halt" => Some(Mode::ResetHalt),
            "reset_run" => Some(Mode::ResetRun),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Mode::Passive => "passive",
            Mode::Hot => "hot",
            Mode::Halt => "halt",
            Mode::ResetHalt => "reset_halt",
            Mode::ResetRun => "reset_run",
        }
    }
}

/// Default of `attach_run_ms`: the application's start-up before the halt of `attach_mode halt`.
pub const DEFAULT_RUN_MS: u64 = 100;

pub fn passive(cx: &Cx) -> bool {
    cx.with(|c| c.attach_mode == Mode::Passive)
}

/// Refuses an operation that writes the target in a passive attach.
pub fn refuse_passive(cx: &Cx, what: &str) -> Result<()> {
    if passive(cx) {
        return Err(other(format!("passive attach: {what} not allowed (it would write the target)")));
    }
    Ok(())
}

/// Why a halt is not safe now, "" when it is.
pub fn halt_unsafe(cx: &Cx) -> String {
    let mut why = Vec::new();
    let locked = cx.with(|c| c.restore.suspend.borrow().names_in(State::Locked));
    if !locked.is_empty() {
        why.push(format!("{} locked against the debugger before their suspend was written", locked.join(" ")));
    }
    if let Some(hook) = cx.with(|c| c.halt_check_hook.clone()) {
        match cx.interp.eval_global(&hook) {
            Ok(r) if !r.trim().is_empty() => why.push(r.trim().to_string()),
            Ok(_) => {}
            Err(e) => log_warn!("halt_check_hook {hook}: {e}"),
        }
    }
    why.join("; ")
}

/// Halts all cores together, as the debugger's pause does, with the pending peripherals suspended
/// first: TL1 pulsed and cpu0's halt requested (without TL1 sync: every core's), on the registers
/// rather than through the server's state, which can be stale in a handler or after a reset the
/// poll has not seen yet. Waits for cpu0's halt, then polls so that the server sees it. Returns
/// whether cpu0 halted.
pub fn halt_all(cx: &Cx, timeout_ms: u64) -> bool {
    crate::suspend::attempt_now(cx, Why::Pause, Select::Active);
    let (sync, dbgsrs) = cx.with(|c| {
        let p = c.restore.tl1.borrow();
        let all: Vec<u32> = c.targets.iter().filter_map(|e| if let Core::Tricore(t) = &e.core { t.dbgbase.map(|_| t.csfr(DBGSR)) } else { None }).collect();
        (p.on, all)
    });
    let Some(&cpu0) = dbgsrs.first() else { return false };
    let mut writes = Vec::new();
    if sync {
        writes.extend(crate::tl1::force_writes());
        writes.push((cpu0, DBGSR_HALT_REQ));
    } else {
        writes.extend(dbgsrs.iter().map(|&d| (d, DBGSR_HALT_REQ)));
    }
    // One BlueBox message: a failed TL1 pulse or halt request does not hold back the others.
    let wrote = cx.with(|c| c.io().and_then(|mut io| io.write32_seq_packed(&writes)));
    let mut halted = false;
    if wrote.is_ok() {
        let t0 = Instant::now();
        while !halted && t0.elapsed() < Duration::from_millis(timeout_ms) {
            halted = cx.with(|c| c.io().and_then(|mut io| io.read32(cpu0)).is_ok_and(|d| d & 0x6 == DBGSR_HALTED));
            if !halted {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
    if !halted {
        log_warn!("halt_all: the cores did not halt");
        return false;
    }
    // The server sees the halt now: the background poll does not run during scripts and handlers.
    crate::ops::poll_all(cx);
    true
}

fn report(cx: &Cx) {
    let (pending, locked) = cx.with(|c| {
        let s = c.restore.suspend.borrow();
        (s.names_in(State::Pending), s.names_in(State::Locked))
    });
    if !pending.is_empty() {
        log_info!("Attach: no suspend yet for {} (not enabled by the application yet; written once they are)", pending.join(" "));
    }
    if !locked.is_empty() {
        log_warn!("Attach: {} locked against the debugger before their suspend was written - a halt now leaves them running; the next reset suspends them", locked.join(" "));
    }
}

/// The attach mode, at the end of `init`.
pub fn run(cx: &Cx) -> Result<()> {
    let mode = cx.with(|c| c.attach_mode);
    match mode {
        Mode::Passive => log_user!("Passive attach: no register written; memory reads only (no halt, no reset)."),
        Mode::Hot => report(cx),
        Mode::Halt => {
            report(cx);
            let why = halt_unsafe(cx);
            if !why.is_empty() {
                log_user!("Halt not safe ({why}): reset first");
                crate::ops::reset(cx, "run")?;
                let ms = cx.with(|c| c.attach_run_ms);
                crate::commands::wait_watching(cx, Duration::from_millis(ms));
            }
            if halt_all(cx, 1000) {
                log_user!("Halted in the application.");
            }
        }
        Mode::ResetHalt => crate::ops::reset(cx, "halt")?,
        Mode::ResetRun => crate::ops::reset(cx, "run")?,
    }
    Ok(())
}

// --- Commands ----------------------------------------------------------------------------------

/// `attach_mode ?passive|hot|halt|reset_halt|reset_run?`: before `init`.
pub fn cmd_attach_mode(cx: &Cx, a: &[&str]) -> std::result::Result<String, String> {
    if let Some(m) = a.get(1) {
        let m = Mode::parse(m).ok_or("usage: attach_mode ?passive|hot|halt|reset_halt|reset_run?")?;
        if cx.with(|c| c.initialized) {
            return Err("attach_mode: set it before 'init'".into());
        }
        cx.with(|c| c.attach_mode = m);
    }
    Ok(cx.with(|c| c.attach_mode.name()).to_string())
}

/// `attach_run_ms ?ms?`.
pub fn cmd_attach_run_ms(cx: &Cx, a: &[&str]) -> std::result::Result<String, String> {
    if let Some(v) = a.get(1) {
        let ms = crate::util::parse_u64(v)?;
        cx.with(|c| c.attach_run_ms = ms);
    }
    Ok(cx.with(|c| c.attach_run_ms).to_string())
}

/// `halt_check_hook ?proc?`: an empty name removes it.
pub fn cmd_halt_check_hook(cx: &Cx, a: &[&str]) -> std::result::Result<String, String> {
    if let Some(p) = a.get(1) {
        cx.with(|c| c.halt_check_hook = if p.is_empty() { None } else { Some(p.to_string()) });
    }
    Ok(cx.with(|c| c.halt_check_hook.clone().unwrap_or_default()))
}

/// `halt_all ?timeout_ms?`: 1 when halted.
pub fn cmd_halt_all(cx: &Cx, a: &[&str]) -> std::result::Result<String, String> {
    let ms = match a.get(1) {
        Some(v) => crate::util::parse_u64(v)?,
        None => 1000,
    };
    if !cx.with(|c| c.initialized) {
        return Err("halt_all: run 'init' first".into());
    }
    refuse_passive(cx, "halt").map_err(|e| e.to_string())?;
    Ok((if halt_all(cx, ms) { "1" } else { "0" }).into())
}

/// `attach_status`: the mode, the peripheral suspend still missing, and whether a halt is safe.
pub fn cmd_attach_status(cx: &Cx, _a: &[&str]) -> std::result::Result<String, String> {
    let mode = cx.with(|c| c.attach_mode.name());
    let (pending, locked) = cx.with(|c| {
        let s = c.restore.suspend.borrow();
        (s.names_in(State::Pending), s.names_in(State::Locked))
    });
    let why = halt_unsafe(cx);
    Ok(format!(
        "mode {mode} pending {{{}}} locked {{{}}} halt_unsafe {}",
        pending.join(" "),
        locked.join(" "),
        jim::quote(&why)
    ))
}
