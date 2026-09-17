//! Source-level stepping: TCF resume modes 3 (step over line), 4 (step into
//! line) and 5 (step out).
//!
//! Built from instruction steps and temporary hardware breakpoints at return
//! addresses. Intermediate suspend/resume events of the stepping core are not
//! forwarded; one contextSuspended is sent when the step completes.
//!
//! How calls are detected depends on the core:
//! - TriCore: through the CSA chain. After a CALL, the new PCXI links to a CSA
//!   whose first word is the caller's PCXI.
//! - Cortex-M (HSM): after a BL/BLX, LR holds the address of the instruction
//!   following the call. The stack pointer identifies the frame (recursion);
//!   step out runs to the return address of the unwound caller frame.
//! - GTM MCS channel: one instruction per line, see [`run_mcs`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

use aurix_core::arch::{CM_REG_LR, CM_REG_PC, CM_REG_SP, MCS_REG_PC, MCS_REG_R7, REG_A11, REG_PC, REG_PCXI};
use aurix_core::target::{BpKind, DebugReason, RunMode, TargetState};
use aurix_core::unwind::csa_address;
use aurix_core::unwind_arm::is_thumb32;
use aurix_core::{log_warn, ops, ServerHandle};

use crate::contexts::{find_core, read_pc};
use crate::forwarder;
use crate::symstore::SymbolSet;
use crate::wire;
use crate::Shared;

const STEP_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug)]
struct Regs {
    pc: u32,
    /// Return address register: A11 (TriCore), LR (Cortex-M, Thumb bit cleared).
    ra: u32,
    /// Frame identity: PCXI (TriCore), SP (Cortex-M).
    frame: u32,
}

/// Register roles of the stepped core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    TriCore,
    Arm,
    Mcs,
}

enum Stop {
    Done,
    /// Halted somewhere else (e.g. a user breakpoint).
    Elsewhere,
    Cancelled,
}

pub fn start(shared: Arc<Shared>, idx: usize, ctx: String, mode: u64) {
    let cancel = Arc::new(AtomicBool::new(false));
    shared.stepping.lock().unwrap().insert(idx, cancel.clone());
    shared.invalidate_stack(idx);
    shared.broadcast(&wire::event("RunControl", "contextResumed", &[json!(ctx).to_string()]));
    thread::spawn(move || {
        let arch = find_core(&shared.handle, &ctx).map(|c| c.arch.name);
        let result = match arch {
            Some("cortex_m") => run_arm(&shared, idx, &ctx, mode, &cancel),
            Some("gtm_mcs") => run_mcs(&shared, idx, mode, &cancel),
            _ => run(&shared, idx, &ctx, mode, &cancel),
        };
        let h = shared.handle.clone();
        let halted = h.call(move |cx| ops::state(cx, idx).0) == Some(TargetState::Halted);
        if !halted {
            let _ = h.call(move |cx| {
                let _ = ops::halt(cx, idx);
                ops::wait_state(cx, idx, TargetState::Halted, 2000)
            });
        }
        shared.stepping.lock().unwrap().remove(&idx);
        shared.invalidate_stack(idx);
        let reason = match result {
            Ok(Stop::Done) => DebugReason::SingleStep,
            Ok(Stop::Elsewhere) => h.call(move |cx| ops::state(cx, idx).1).unwrap_or(DebugReason::DbgRq),
            Ok(Stop::Cancelled) => DebugReason::DbgRq,
            Err(e) => {
                log_warn!("source step failed: {e}");
                DebugReason::DbgRq
            }
        };
        let pc = read_pc(&h, idx);
        forwarder::halted(&shared, idx, reason, pc);
        forwarder::sync_others(&shared, idx);
    });
}

fn call<R: Send + 'static>(h: &ServerHandle, f: impl for<'a, 'b> FnOnce(&'b aurix_core::Cx<'a>) -> Result<R, String> + Send + 'static) -> Result<R, String> {
    h.call(f).unwrap_or_else(|| Err("server stopped".into()))
}

fn read_regs(h: &ServerHandle, idx: usize, kind: Kind) -> Result<Regs, String> {
    let idxs = match kind {
        Kind::Arm => [CM_REG_PC, CM_REG_LR, CM_REG_SP],
        Kind::TriCore => [REG_PC, REG_A11, REG_PCXI],
        // The MCS has no return address register: the frame is the stack pointer R7.
        Kind::Mcs => [MCS_REG_PC, MCS_REG_PC, MCS_REG_R7],
    };
    let v = call(h, move |cx| {
        cx.with(|c| {
            let (mut io, e) = c.target_io(idx)?;
            e.core.read_regs(&mut io, &idxs)
        })
        .map_err(|e| e.to_string())
    })?;
    Ok(if kind == Kind::Arm { Regs { pc: v[0] & !1, ra: v[1] & !1, frame: v[2] } } else { Regs { pc: v[0], ra: v[1], frame: v[2] } })
}

/// First word (saved PCXI) of the CSA a PCXI value links to.
fn csa_word0(h: &ServerHandle, pcxi: u32) -> Option<u32> {
    let addr = csa_address(pcxi)?;
    call(h, move |cx| cx.with(|c| c.io().and_then(|mut io| io.read32(addr))).map_err(|e| e.to_string())).ok()
}

fn step_instruction(h: &ServerHandle, idx: usize, rm: RunMode) -> Result<(), String> {
    call(h, move |cx| ops::step_as(cx, idx, None, rm).map_err(|e| e.to_string()))
}

/// Run mode of a TCF resume mode; selects the core's `irq_mask` setting for every step and run.
fn run_mode(mode: u64) -> RunMode {
    match mode {
        3 => RunMode::StepOver,
        5 => RunMode::StepOut,
        _ => RunMode::Step,
    }
}

fn run(shared: &Shared, idx: usize, ctx: &str, mode: u64, cancel: &AtomicBool) -> Result<Stop, String> {
    let h = &shared.handle;
    let rm = run_mode(mode);
    let deadline = Instant::now() + STEP_TIMEOUT;
    let Some(syms) = shared.symbols_for(ctx) else {
        step_instruction(h, idx, rm)?;
        return Ok(Stop::Done);
    };
    let regs = |h| read_regs(h, idx, Kind::TriCore);
    let mut r = regs(h)?;

    if mode == 5 {
        let caller_pcxi = csa_word0(h, r.frame);
        return run_to(h, idx, Kind::TriCore, r.ra, caller_pcxi, rm, cancel, deadline);
    }

    let (Some(f), Some(li)) = (syms.function_at(r.pc), syms.line_at(r.pc)) else {
        step_instruction(h, idx, rm)?;
        return Ok(Stop::Done);
    };
    let mut func = (f.low, f.high);
    let mut line = (li.file, li.line);
    let mut frame_pcxi = r.frame;
    let mut caller_pcxi = csa_word0(h, r.frame);

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(Stop::Cancelled);
        }
        if Instant::now() > deadline {
            return Err("step timed out".into());
        }
        let prev = r;
        step_instruction(h, idx, rm)?;
        r = regs(h)?;
        let in_func = |pc: u32| pc >= func.0 && pc < func.1;
        // Back in the stepped frame at the first instruction of another line.
        let new_line = |r: &Regs| {
            in_func(r.pc)
                && r.frame == frame_pcxi
                && syms.line_at(r.pc).is_some_and(|li| li.start == r.pc && (li.file.as_str(), li.line) != (line.0.as_str(), line.1))
        };

        if in_func(r.pc) && r.frame == frame_pcxi {
            if new_line(&r) {
                return Ok(Stop::Done);
            }
            continue;
        }

        if r.frame != frame_pcxi && csa_word0(h, r.frame) == Some(frame_pcxi) {
            // CALL into another function.
            if mode == 4 && syms.line_at(r.pc).is_some() {
                return Ok(Stop::Done);
            }
            match run_to(h, idx, Kind::TriCore, r.ra, Some(frame_pcxi), rm, cancel, deadline)? {
                Stop::Done => {
                    // The return address often starts the next line (a call is
                    // commonly the last instruction of a statement).
                    r = regs(h)?;
                    if new_line(&r) {
                        return Ok(Stop::Done);
                    }
                    continue;
                }
                other => return Ok(other),
            }
        }

        if Some(r.frame) == caller_pcxi {
            // Returned: continue in the caller until the next line starts.
            let (Some(f), Some(li)) = (syms.function_at(r.pc), syms.line_at(r.pc)) else {
                return Ok(Stop::Done);
            };
            if li.start == r.pc {
                return Ok(Stop::Done);
            }
            func = (f.low, f.high);
            line = (li.file, li.line);
            frame_pcxi = r.frame;
            caller_pcxi = csa_word0(h, r.frame);
            continue;
        }

        // Left the function without a CSA change (JL/JI fast call).
        if r.frame == frame_pcxi && in_func(prev.pc) && in_func(r.ra) && (mode == 3 || syms.line_at(r.pc).is_none()) {
            match run_to(h, idx, Kind::TriCore, r.ra, Some(frame_pcxi), rm, cancel, deadline)? {
                Stop::Done => {
                    r = regs(h)?;
                    if new_line(&r) {
                        return Ok(Stop::Done);
                    }
                    continue;
                }
                other => return Ok(other),
            }
        }
        return Ok(Stop::Done);
    }
}

/// Length of the Thumb instruction at `pc`.
fn thumb_len(h: &ServerHandle, idx: usize, pc: u32) -> Result<u32, String> {
    let b = call(h, move |cx| ops::read_memory_as(cx, idx, pc, 2).map_err(|e| e.to_string()))?;
    Ok(if is_thumb32(u16::from_le_bytes([b[0], b[1]])) { 4 } else { 2 })
}

fn function_and_line(syms: &SymbolSet, pc: u32) -> Option<((u32, u32), (String, u32))> {
    let f = syms.function_at(pc)?;
    let li = syms.line_at(pc)?;
    Some(((f.low & !1, f.high), (li.file, li.line)))
}

fn run_arm(shared: &Shared, idx: usize, ctx: &str, mode: u64, cancel: &AtomicBool) -> Result<Stop, String> {
    let h = &shared.handle;
    let rm = run_mode(mode);
    let deadline = Instant::now() + STEP_TIMEOUT;
    let Some(syms) = shared.symbols_for(ctx) else {
        step_instruction(h, idx, rm)?;
        return Ok(Stop::Done);
    };

    if mode == 5 {
        let frames = crate::stack::unwind_core(shared, idx)?;
        let Some(caller) = frames.get(1) else {
            // No caller known: a single instruction step.
            step_instruction(h, idx, rm)?;
            return Ok(Stop::Done);
        };
        return run_to(h, idx, Kind::Arm, caller.pc, caller.sp(), rm, cancel, deadline);
    }

    let mut r = read_regs(h, idx, Kind::Arm)?;
    let Some((mut func, mut line)) = function_and_line(&syms, r.pc) else {
        step_instruction(h, idx, rm)?;
        return Ok(Stop::Done);
    };

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(Stop::Cancelled);
        }
        if Instant::now() > deadline {
            return Err("step timed out".into());
        }
        let prev = r;
        let next = prev.pc + thumb_len(h, idx, prev.pc)?;
        step_instruction(h, idx, rm)?;
        r = read_regs(h, idx, Kind::Arm)?;
        let in_func = |pc: u32| pc >= func.0 && pc < func.1;
        let new_line = |r: &Regs| {
            in_func(r.pc) && syms.line_at(r.pc).is_some_and(|li| li.start == r.pc && (li.file.as_str(), li.line) != (line.0.as_str(), line.1))
        };

        if r.pc != next && r.ra == next {
            // BL/BLX: a call, returning to the next instruction with the same SP.
            if mode == 4 && syms.line_at(r.pc).is_some() {
                return Ok(Stop::Done);
            }
            match run_to(h, idx, Kind::Arm, next, Some(prev.frame), rm, cancel, deadline)? {
                Stop::Done => {
                    r = read_regs(h, idx, Kind::Arm)?;
                    if new_line(&r) {
                        return Ok(Stop::Done);
                    }
                    continue;
                }
                other => return Ok(other),
            }
        }

        if in_func(r.pc) {
            if new_line(&r) {
                return Ok(Stop::Done);
            }
            continue;
        }

        // Returned (or branched) into other code: continue there until a line starts.
        match function_and_line(&syms, r.pc) {
            Some((f, l)) => {
                if syms.line_at(r.pc).is_some_and(|li| li.start == r.pc) {
                    return Ok(Stop::Done);
                }
                func = f;
                line = l;
            }
            None => return Ok(Stop::Done),
        }
    }
}

/// GTM MCS channel: every source line is one instruction. Step over and into are one instruction
/// step (the core steps over a CALL for step over); step out runs to the return address of the
/// caller frame. An instruction waiting for an event (WURM, ARU) completes later: the channel stops
/// after it on its own, so the step waits for that halt.
fn run_mcs(shared: &Shared, idx: usize, mode: u64, cancel: &AtomicBool) -> Result<Stop, String> {
    let h = &shared.handle;
    let rm = run_mode(mode);
    let deadline = Instant::now() + STEP_TIMEOUT;
    if mode == 5 {
        let frames = crate::stack::unwind_core(shared, idx)?;
        if let Some(caller) = frames.get(1) {
            return run_to(h, idx, Kind::Mcs, caller.pc, None, rm, cancel, deadline);
        }
    }
    if step_instruction(h, idx, rm).is_ok() {
        return Ok(Stop::Done);
    }
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(Stop::Cancelled);
        }
        if Instant::now() > deadline {
            return Err("step timed out: the MCS channel waits for an event".into());
        }
        let state = h.call(move |cx| {
            ops::poll_all(cx);
            ops::state(cx, idx).0
        });
        if state == Some(TargetState::Halted) {
            return Ok(Stop::Done);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Runs until `addr` is reached in the expected frame (recursion guard: PCXI on TriCore, SP
/// on Cortex-M).
#[allow(clippy::too_many_arguments)]
fn run_to(h: &ServerHandle, idx: usize, kind: Kind, addr: u32, expect_frame: Option<u32>, rm: RunMode, cancel: &AtomicBool, deadline: Instant) -> Result<Stop, String> {
    let temp = match call(h, move |cx| ops::add_breakpoint(cx, idx, addr, 4, BpKind::Hard).map_err(|e| e.to_string())) {
        Ok(_) => true,
        Err(e) if e.contains("Duplicate") => false,
        Err(e) => return Err(e),
    };
    let result = (|| -> Result<Stop, String> {
        loop {
            call(h, move |cx| ops::resume_as(cx, idx, None, rm).map_err(|e| e.to_string()))?;
            loop {
                if cancel.load(Ordering::Relaxed) {
                    let _ = call(h, move |cx| ops::halt(cx, idx).map_err(|e| e.to_string()));
                    return Ok(Stop::Cancelled);
                }
                if Instant::now() > deadline {
                    let _ = call(h, move |cx| ops::halt(cx, idx).map_err(|e| e.to_string()));
                    return Err("step timed out waiting for return".into());
                }
                let state = h.call(move |cx| {
                    ops::poll_all(cx);
                    ops::state(cx, idx).0
                });
                if state == Some(TargetState::Halted) {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            let r = read_regs(h, idx, kind)?;
            if r.pc == addr {
                if expect_frame.is_none_or(|p| p == r.frame) {
                    return Ok(Stop::Done);
                }
                continue; // recursive call hit the return address
            }
            return Ok(Stop::Elsewhere);
        }
    })();
    if temp {
        let _ = h.call(move |cx| ops::remove_breakpoint(cx, idx, addr));
    }
    result
}
