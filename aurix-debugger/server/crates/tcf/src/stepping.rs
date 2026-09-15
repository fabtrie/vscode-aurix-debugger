//! Source-level stepping: TCF resume modes 3 (step over line), 4 (step into
//! line) and 5 (step out).
//!
//! Built from instruction steps and temporary hardware breakpoints at return
//! addresses. Calls are detected through the CSA chain: after a CALL, the new
//! PCXI links to a CSA whose first word is the caller's PCXI. Intermediate
//! suspend/resume events of the stepping core are not forwarded; one
//! contextSuspended is sent when the step completes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::json;

use aurix_core::arch::{REG_A11, REG_PC, REG_PCXI};
use aurix_core::target::{BpKind, DebugReason, RunMode, TargetState};
use aurix_core::unwind::csa_address;
use aurix_core::{log_warn, ops, ServerHandle};

use crate::contexts::read_pc;
use crate::forwarder;
use crate::wire;
use crate::Shared;

const STEP_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug)]
struct Regs {
    pc: u32,
    a11: u32,
    pcxi: u32,
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
        let result = run(&shared, idx, mode, &cancel);
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

fn regs(h: &ServerHandle, idx: usize) -> Result<Regs, String> {
    let v = call(h, move |cx| {
        cx.with(|c| {
            let (mut io, e) = c.target_io(idx)?;
            e.core.read_regs(&mut io, &[REG_PC, REG_A11, REG_PCXI])
        })
        .map_err(|e| e.to_string())
    })?;
    Ok(Regs { pc: v[0], a11: v[1], pcxi: v[2] })
}

/// First word (saved PCXI) of the CSA a PCXI value links to.
fn csa_word0(h: &ServerHandle, pcxi: u32) -> Option<u32> {
    let addr = csa_address(pcxi)?;
    call(h, move |cx| cx.with(|c| c.io().and_then(|mut io| io.read32(addr))).map_err(|e| e.to_string())).ok()
}

fn step_instruction(h: &ServerHandle, idx: usize, rm: RunMode) -> Result<(), String> {
    call(h, move |cx| ops::step_as(cx, idx, None, rm).map_err(|e| e.to_string()))
}

fn run(shared: &Shared, idx: usize, mode: u64, cancel: &AtomicBool) -> Result<Stop, String> {
    let h = &shared.handle;
    // Selects the core's `tricore irq_mask` setting for every step and run below.
    let rm = match mode {
        3 => RunMode::StepOver,
        5 => RunMode::StepOut,
        _ => RunMode::Step,
    };
    let deadline = Instant::now() + STEP_TIMEOUT;
    let Some(syms) = shared.active_symbols() else {
        step_instruction(h, idx, rm)?;
        return Ok(Stop::Done);
    };
    let mut r = regs(h, idx)?;

    if mode == 5 {
        let caller_pcxi = csa_word0(h, r.pcxi);
        return run_to(h, idx, r.a11, caller_pcxi, rm, cancel, deadline);
    }

    let (Some(f), Some(li)) = (syms.function_at(r.pc), syms.line_at(r.pc)) else {
        step_instruction(h, idx, rm)?;
        return Ok(Stop::Done);
    };
    let mut func = (f.low, f.high);
    let mut line = (li.file, li.line);
    let mut frame_pcxi = r.pcxi;
    let mut caller_pcxi = csa_word0(h, r.pcxi);

    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(Stop::Cancelled);
        }
        if Instant::now() > deadline {
            return Err("step timed out".into());
        }
        let prev = r;
        step_instruction(h, idx, rm)?;
        r = regs(h, idx)?;
        let in_func = |pc: u32| pc >= func.0 && pc < func.1;
        // Back in the stepped frame at the first instruction of another line.
        let new_line = |r: &Regs| {
            in_func(r.pc)
                && r.pcxi == frame_pcxi
                && syms.line_at(r.pc).is_some_and(|li| li.start == r.pc && (li.file.as_str(), li.line) != (line.0.as_str(), line.1))
        };

        if in_func(r.pc) && r.pcxi == frame_pcxi {
            if new_line(&r) {
                return Ok(Stop::Done);
            }
            continue;
        }

        if r.pcxi != frame_pcxi && csa_word0(h, r.pcxi) == Some(frame_pcxi) {
            // CALL into another function.
            if mode == 4 && syms.line_at(r.pc).is_some() {
                return Ok(Stop::Done);
            }
            match run_to(h, idx, r.a11, Some(frame_pcxi), rm, cancel, deadline)? {
                Stop::Done => {
                    // The return address often starts the next line (a call is
                    // commonly the last instruction of a statement).
                    r = regs(h, idx)?;
                    if new_line(&r) {
                        return Ok(Stop::Done);
                    }
                    continue;
                }
                other => return Ok(other),
            }
        }

        if Some(r.pcxi) == caller_pcxi {
            // Returned: continue in the caller until the next line starts.
            let (Some(f), Some(li)) = (syms.function_at(r.pc), syms.line_at(r.pc)) else {
                return Ok(Stop::Done);
            };
            if li.start == r.pc {
                return Ok(Stop::Done);
            }
            func = (f.low, f.high);
            line = (li.file, li.line);
            frame_pcxi = r.pcxi;
            caller_pcxi = csa_word0(h, r.pcxi);
            continue;
        }

        // Left the function without a CSA change (JL/JI fast call).
        if r.pcxi == frame_pcxi && in_func(prev.pc) && in_func(r.a11) && (mode == 3 || syms.line_at(r.pc).is_none()) {
            match run_to(h, idx, r.a11, Some(frame_pcxi), rm, cancel, deadline)? {
                Stop::Done => {
                    r = regs(h, idx)?;
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

/// Runs until `addr` is reached with the expected PCXI (recursion guard).
fn run_to(h: &ServerHandle, idx: usize, addr: u32, expect_pcxi: Option<u32>, rm: RunMode, cancel: &AtomicBool, deadline: Instant) -> Result<Stop, String> {
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
            let r = regs(h, idx)?;
            if r.pc == addr {
                if expect_pcxi.is_none_or(|p| p == r.pcxi) {
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
