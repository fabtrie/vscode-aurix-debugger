//! High-level target operations with OpenOCD event semantics.
//!
//! Every function here interleaves short state accesses with Tcl event
//! handlers, so handlers can call back into the core (e.g. `mww`).

use std::time::{Duration, Instant};

use tas::Batch;

use crate::adapter::{self, Io};
use crate::arch::*;
use crate::cortex_m;
use crate::ctx::{Cx, Mode};
use crate::error::{other, usage, Error, Result};
use crate::events::{emit, Event};
use crate::target::{BpKind, Breakpoint, Core, DebugReason, RunMode, SourceStep, TargetState, Transition, WatchAccess, Watchpoint, FLAP_RETRY, POLL_FAIL_TIME, UNREACHABLE_TIME};
use crate::{log_debug, log_error, log_info, log_warn};

/// Runs a target event handler (`$t configure -event name body`).
pub fn fire(cx: &Cx, t: usize, event: &str) {
    let (body, name) = match cx.with(|c| c.targets.get(t).map(|e| (e.event(event).map(str::to_string), e.name.clone()))) {
        Some((Some(b), n)) => (b, n),
        _ => return,
    };
    let key = (t, event.to_string());
    let entered = cx.with(|c| {
        if c.active_events.insert(key.clone()) {
            c.override_stack.push(t);
            true
        } else {
            false
        }
    });
    if !entered {
        log_debug!("[{name}] event {event} already running, not re-entered");
        return;
    }
    log_debug!("target: {name} event: {event} action: {body}");
    let r = cx.interp.eval_global(&body);
    cx.with(|c| {
        c.override_stack.pop();
        c.active_events.remove(&key);
    });
    if let Err(e) = r {
        log_error!("[{name}] Error executing event {event} on target {name}:\n{e}");
    }
}

pub fn target_name(cx: &Cx, t: usize) -> String {
    cx.with(|c| c.targets.get(t).map(|e| e.name.clone()).unwrap_or_default())
}

/// Enables OCDS through OEC if OSTATE says it is off (after every reset).
pub fn enable_ocds(io: &mut Io) -> Result<bool> {
    let ostate = io.read32(OSTATE)?;
    if ostate & OSTATE_OEN != 0 {
        return Ok(false);
    }
    if ostate & OSTATE_LOCKED != 0 {
        return Err(other("[OCMTS] Device is still locked"));
    }
    io.write32_seq(&[(OEC, 0xA1), (OEC, 0x5E), (OEC, 0xA1), (OEC, 0x5E)])?;
    let after = io.read32(OSTATE)?;
    if after & OSTATE_OEN == 0 {
        return Err(other("[OCMTS] Failed to enable debug"));
    }
    log_info!("[OCMTS] Debug (OCDS) enabled");
    Ok(true)
}

/// OpenOCD `target_examine_one`: examine-start, examine (+ poll), examine-end/fail.
pub fn examine_one(cx: &Cx, t: usize, quiet: bool) -> Result<()> {
    let name = target_name(cx, t);
    fire(cx, t, "examine-start");
    let res = cx.with(|c| -> Result<Transition> {
        let (mut io, e) = c.target_io(t)?;
        // A probe that does its own run control has already enabled debug, and
        // the OEC key write below is one it would refuse: OCDS is its to steer.
        if io.adapter.run_control().is_none() {
            enable_ocds(&mut io)?;
        }
        e.core.examine(&mut io)?;
        let tr = e.core.poll(&mut io)?;
        e.core.restore_triggers(&mut io)?;
        Ok(tr)
    });
    match res {
        Err(err) => {
            cx.with(|c| c.targets[t].core.set_examined(false));
            if !quiet {
                log_error!("[{name}] Examination failed: {err}");
            }
            fire(cx, t, "examine-fail");
            Err(err)
        }
        Ok(tr) => {
            // Accessible again before the halt is reported, so frontends list the core first.
            cx.with(|c| {
                let e = &mut c.targets[t];
                e.down_since = None;
                e.unreachable = false;
            });
            handle_transition(cx, t, tr);
            fire(cx, t, "examine-end");
            if !quiet {
                log_info!("[{name}] Examination succeed");
            }
            emit(Event::Examined { target: t, name });
            Ok(())
        }
    }
}

/// Reports a poll transition: frontend event plus the `halted` Tcl event.
pub fn handle_transition(cx: &Cx, t: usize, tr: Transition) {
    match tr {
        Transition::None => {}
        Transition::Halted { debug, boot } => {
            let (name, reason, pc, trigger) = cx.with(|c| {
                let reason = c.targets[t].core.reason();
                let pc = c.target_io(t).ok().and_then(|(mut io, e)| {
                    let pc = e.core.arch().pc;
                    e.core.read_reg(&mut io, pc).ok()
                });
                let trigger = c.targets[t].core.hit_trigger();
                (c.targets[t].name.clone(), reason, pc, trigger)
            });
            let pc_s = pc.map(|pc| format!(", pc: 0x{pc:08x}")).unwrap_or_default();
            if boot {
                log_info!("[{name}] in boot halt (core not started yet){pc_s}");
            } else {
                log_info!("[{name}] halted due to {}{pc_s}", reason.name());
            }
            emit(Event::Halted { target: t, name, reason, pc, trigger });
            if !boot {
                fire(cx, t, if debug { "debug-halted" } else { "halted" });
            }
        }
        Transition::Running => {
            let name = target_name(cx, t);
            log_debug!("[{name}] running");
            // Released outside a resume (e.g. a raw DBGSR write) while
            // interrupts were masked for a step: give them back.
            let unmasked = cx.with(|c| -> Result<bool> {
                if !c.targets[t].core.irq_masked() {
                    return Ok(false);
                }
                let (mut io, e) = c.target_io(t)?;
                e.core.unmask_irq(&mut io).map(|_| true)
            });
            match unmasked {
                Ok(true) => log_warn!("[{name}] resumed outside the debugger while interrupts were masked; ICR.IE restored"),
                Err(e) => log_warn!("[{name}] cannot restore ICR.IE: {e}"),
                Ok(false) => {}
            }
            emit(Event::Resumed { target: t, name });
        }
    }
}

/// On a run-control probe, each plain-memory core (HSM, MCS channel) is polled
/// every this many periods; see [`poll_all`].
const RC_MEMORY_POLL_SHARE: usize = 8;

/// One background poll of all targets (OpenOCD `handle_target`).
pub fn poll_all(cx: &Cx) {
    let go = cx.with(|c| c.poll_enabled && c.initialized && !c.in_reset && c.adapter.as_ref().is_some_and(|a| a.initialized()));
    if !go {
        return;
    }
    let res = cx.with(|c| -> Result<(Vec<(usize, Transition)>, u64, Vec<usize>)> {
        // A probe that does its own run control answers "is it stopped?" for
        // the TriCore cores by being asked, not by a batch of DBGSR reads.
        // `poll_queue` has no `Io` to redirect, so this is one of the few
        // places that has to branch rather than go through `Core`. The HSM and
        // the MCS channels are plain memory there too, and stay in the batch --
        // but only a share of them per period: such a probe pays one call per
        // access (~5 ms on winIDEA), and all 16 MCS channels took ~220 ms, which
        // kept the core thread busy and delayed every step and pause behind it.
        let rc = c.adapter.as_deref_mut().is_some_and(|a| a.run_control().is_some());
        let (mut out, mut failed) = if rc { poll_run_controlled(c) } else { Default::default() };
        let turn = c.poll_turn;
        c.poll_turn = c.poll_turn.wrapping_add(1);
        let mut b = Batch::new();
        let mut handles = Vec::new();
        for (i, e) in c.targets.iter().enumerate() {
            let skip = rc && (matches!(e.core, Core::Tricore(_)) || i % RC_MEMORY_POLL_SHARE != turn % RC_MEMORY_POLL_SHARE);
            if e.core.examined() && e.poll_failures == 0 && !skip {
                handles.push((i, e.core.poll_queue(&mut b)));
            }
        }
        // Anything the sampler already owes goes into this batch rather than costing its own round
        // trip right behind it. Queued *after* the run-state reads on purpose: a PL0 packet marks
        // every access after a failing one "consequential", so a bad DAQ address must not be able
        // to blank the poll it is riding in. Not on a run-control probe: there every access is its
        // own call either way, and the sampler's own tick handles them.
        let daq_now = Instant::now();
        let daq = (!rc && c.daq_merge && c.daq.next_due().is_some_and(|d| daq_now >= d))
            .then(|| crate::daq::queue_due(c, daq_now, &mut b))
            .flatten();
        let exec = {
            let adapter = c.adapter.as_deref_mut().unwrap();
            let r = if b.is_empty() { Ok(Vec::new()) } else { adapter.execute(&b) };
            r.map(|r| (r, adapter.take_resets()))
        };
        let (r, resets) = match exec {
            Ok(v) => v,
            Err(e) => {
                // The plan mutated nothing, so dropping it leaves the points due; tell the sampler
                // to back off just as its own tick would have.
                if daq.is_some() {
                    crate::daq::batch_failed(c, daq_now);
                }
                return Err(e);
            }
        };
        for (i, ops) in handles {
            if ops.all.iter().all(|&h| r[h].ok()) {
                let vals: Vec<u32> = ops.reads.iter().map(|&h| r[h].u32()).collect();
                let e = &mut c.targets[i];
                out.push((i, e.core.poll_apply(&vals)));
                e.failing_since = None;
                e.flapping = false;
                if let Some(n) = e.recovering.take() {
                    if std::mem::take(&mut e.failure_logged) {
                        log_info!("[{}] Reexamined after {n} attempt(s)", e.name);
                    }
                }
            } else if ops.all.iter().all(|&h| r[h].ok() || r[h].err == tas::PL0_ERR_CONSEQUENTIAL) {
                // Not executed because an access of another target failed before: polled again
                // next period.
            } else {
                let e = &mut c.targets[i];
                if let Some(&h) = ops.all.iter().find(|&&h| !r[h].ok() && r[h].err != tas::PL0_ERR_CONSEQUENTIAL) {
                    log_debug!("[{}] poll access {h} failed: {}", e.name, tas::pl_err_name(r[h].err));
                }
                // A single failed poll keeps the state; only failures lasting POLL_FAIL_TIME count.
                let now = Instant::now();
                if now.duration_since(*e.failing_since.get_or_insert(now)) >= POLL_FAIL_TIME {
                    failed.push(i);
                }
            }
        }
        // Before the fixups: those can fail with `?`, and the samples are already in hand.
        if let Some(plan) = daq {
            crate::daq::apply(c, plan, daq_now, &r, None);
        }
        // Follow-ups needing target access (e.g. MCS breakpoints overwritten by a program reload).
        for i in 0..c.targets.len() {
            if c.targets[i].core.wants_fixup() {
                let (mut io, e) = c.target_io(i)?;
                if let Err(err) = e.core.fixup(&mut io) {
                    log_warn!("[{}] {err}", e.name);
                }
            }
        }
        Ok((out, resets, failed))
    });

    let mut failed_all = false;
    match res {
        Ok((transitions, resets, failed)) => {
            // Report transitions even when a reset was flagged: poll_apply has
            // already updated the state, so skipping them would lose the event.
            for (i, tr) in transitions {
                handle_transition(cx, i, tr);
            }
            if resets > 0 {
                log_warn!("Device reset detected");
                emit(Event::ResetDetected);
                // DAQ points keep their addresses, but their change-detection baselines are now
                // pre-reset values: drop them and back off until the device answers again.
                cx.with(|c| crate::daq::on_reset(c, Instant::now()));
                failed_all = true;
            }
            for i in failed {
                mark_poll_failed(cx, i, "read/write access failed");
            }
        }
        Err(e) => {
            log_debug!("poll failed: {e}");
            failed_all = true;
        }
    }
    if failed_all {
        let n = cx.with(|c| c.targets.len());
        for i in 0..n {
            mark_poll_failed(cx, i, "device reset or access error");
        }
    }

    // Re-examine every target that failed or was never examined (some core types not in every
    // poll period, e.g. MCS channels while the GTM is still disabled).
    let pending: Vec<usize> = cx.with(|c| {
        let now = Instant::now();
        c.targets
            .iter_mut()
            .enumerate()
            .filter(|(_, e)| (e.poll_failures > 0 || !e.core.examined()) && !e.defer_examine)
            .filter_map(|(i, e)| {
                let interval = if e.flapping { FLAP_RETRY.max(e.core.examine_interval()) } else { e.core.examine_interval() };
                let due = e.last_examine.is_none_or(|t| now.duration_since(t) >= interval);
                due.then(|| {
                    e.last_examine = Some(now);
                    i
                })
            })
            .collect()
    });
    for t in pending {
        let attempts = cx.with(|c| c.targets[t].poll_failures);
        match examine_one(cx, t, true) {
            // Recovered once the next background poll succeeds.
            Ok(()) => cx.with(|c| {
                let e = &mut c.targets[t];
                e.poll_failures = 0;
                e.recovering = Some(attempts.max(1));
            }),
            Err(_) => cx.with(|c| c.targets[t].poll_failures = attempts.saturating_add(1).max(1)),
        }
    }

    // Targets inaccessible for UNREACHABLE_TIME (e.g. not powered) are shown as such.
    let unreachable: Vec<usize> = cx.with(|c| {
        let now = Instant::now();
        c.targets
            .iter_mut()
            .enumerate()
            .filter_map(|(i, e)| {
                if (e.core.examined() && e.poll_failures == 0) || e.defer_examine {
                    e.down_since = None;
                    return None;
                }
                let since = *e.down_since.get_or_insert(now);
                (!e.unreachable && now.duration_since(since) >= UNREACHABLE_TIME).then(|| {
                    e.unreachable = true;
                    i
                })
            })
            .collect()
    });
    for target in unreachable {
        emit(Event::Unreachable { target });
    }
}

fn mark_poll_failed(cx: &Cx, t: usize, why: &str) {
    let first = cx.with(|c| {
        let e = &mut c.targets[t];
        let first = e.poll_failures == 0 && e.recovering.is_none();
        // Failed again before a poll succeeded: still the same failure, retried less often.
        if let Some(n) = e.recovering.take() {
            e.poll_failures = n;
            e.flapping = true;
        }
        e.poll_failures = e.poll_failures.max(1);
        e.failing_since = None;
        e.failure_logged |= first;
        e.core.set_examined(false);
        first
    });
    if first {
        log_error!("[{}] Polling failed ({why}), trying to reexamine every poll period", target_name(cx, t));
    }
}

fn require_examined(cx: &Cx, t: usize) -> Result<()> {
    cx.with(|c| {
        let e = c.targets.get(t).ok_or_else(|| usage(format!("invalid target {t}")))?;
        if !e.core.examined() {
            return Err(Error::NotExamined(e.name.clone()));
        }
        Ok(())
    })
}

pub fn halt(cx: &Cx, t: usize) -> Result<()> {
    require_examined(cx, t)?;
    cx.with(|c| {
        let (mut io, e) = c.target_io(t)?;
        e.core.halt(&mut io)
    })
}

pub fn wait_state(cx: &Cx, t: usize, want: TargetState, ms: u64) -> Result<()> {
    let t0 = Instant::now();
    loop {
        poll_all(cx);
        // This loop owns the core thread, so the server loop cannot tick: keep DAQ streams alive
        // through a wait (a source-level step) instead of blanking them.
        let now = Instant::now();
        if cx.with(|c| c.daq.next_due().is_some_and(|d| now >= d)) {
            crate::daq::tick(cx, now);
        }
        if cx.with(|c| c.targets[t].core.state()) == want {
            return Ok(());
        }
        if t0.elapsed() >= Duration::from_millis(ms) {
            return Err(Error::Timeout(format!(
                "[{}] timed out ({ms}ms) while waiting for target {}",
                target_name(cx, t),
                want.name()
            )));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub fn resume(cx: &Cx, t: usize, addr: Option<u32>) -> Result<()> {
    resume_as(cx, t, addr, RunMode::Continue)
}

/// Resume for `mode`; `tricore irq_mask` decides whether the core runs with
/// interrupts masked. The other cores get their interrupts back first, since
/// multi-core cfg handlers (resume-start) release them too.
pub fn resume_as(cx: &Cx, t: usize, addr: Option<u32>, mode: RunMode) -> Result<()> {
    require_examined(cx, t)?;
    unmask_irq_except(cx, t);
    fire(cx, t, "resume-start");
    cx.with(|c| {
        let (mut io, e) = c.target_io(t)?;
        e.core.resume(&mut io, addr, mode)
    })?;
    emit(Event::Resumed { target: t, name: target_name(cx, t) });
    fire(cx, t, "resumed");
    fire(cx, t, "resume-end");
    Ok(())
}

fn unmask_irq_except(cx: &Cx, t: usize) {
    let n = cx.with(|c| c.targets.len());
    for i in (0..n).filter(|&i| i != t) {
        let r = cx.with(|c| -> Result<()> {
            if !c.targets[i].core.irq_masked() {
                return Ok(());
            }
            let (mut io, e) = c.target_io(i)?;
            e.core.unmask_irq(&mut io)
        });
        if let Err(e) = r {
            log_warn!("[{}] cannot restore ICR.IE: {e}", target_name(cx, i));
        }
    }
}

/// `tl1_step`: makes sure TL1 is latched before the step-start handler
/// releases the other cores, so they stay suspended until the step starts.
fn ensure_tl1_hold(io: &mut Io) -> Result<()> {
    if io.read32(CBS_TLS)? & CBS_TLS_TL1 == 0 {
        io.write32_seq(&[(CBS_TLC, 0x30), (CBS_TLC, 0x0)])?;
    }
    Ok(())
}

pub fn step(cx: &Cx, t: usize, addr: Option<u32>) -> Result<()> {
    step_as(cx, t, addr, RunMode::Step)
}

/// Instruction step as part of `mode` (see [`resume_as`]).
pub fn step_as(cx: &Cx, t: usize, addr: Option<u32>, mode: RunMode) -> Result<()> {
    require_examined(cx, t)?;
    let name = target_name(cx, t);
    if cx.with(|c| c.targets[t].core.state()) != TargetState::Halted {
        return Err(Error::NotHalted(name));
    }
    let tl1_step = cx.with(|c| c.targets[t].core.run().tl1_step);
    if tl1_step {
        // All cores run for the instruction: interrupts masked for an earlier
        // step of another core go back first, and TL1 must hold the cores
        // the step-start handler releases.
        unmask_irq_except(cx, t);
        cx.with(|c| {
            let mut io = c.io()?;
            ensure_tl1_hold(&mut io)
        })?;
    }
    fire(cx, t, "step-start");
    let res = cx.with(|c| {
        let (mut io, e) = c.target_io(t)?;
        e.core.step(&mut io, addr, mode)
    });
    if tl1_step {
        // The other cores ran: their cached registers are stale.
        cx.with(|c| {
            for (i, e) in c.targets.iter_mut().enumerate() {
                if i != t {
                    e.core.invalidate();
                }
            }
        });
    }
    fire(cx, t, "resumed");
    let r = match res {
        Ok(true) => {
            handle_transition(cx, t, Transition::Halted { debug: false, boot: false });
            Ok(())
        }
        Ok(false) => {
            emit(Event::Resumed { target: t, name: name.clone() });
            Err(other(format!("[{name}] Target did not halt after step")))
        }
        Err(e) => Err(e),
    };
    fire(cx, t, "step-end");
    r
}

/// `arp_reset assert <halt>`
pub fn arp_reset_assert(cx: &Cx, t: usize, halt: bool) -> Result<()> {
    let has_handler = cx.with(|c| c.targets[t].event("reset-assert").is_some());
    if has_handler {
        fire(cx, t, "reset-assert");
    } else {
        cx.with(|c| -> Result<()> {
            if !c.srst_asserted {
                c.adapter.as_deref_mut().ok_or_else(|| usage("no adapter"))?.assert_srst()?;
                c.srst_asserted = true;
            }
            Ok(())
        })?;
    }
    cx.with(|c| {
        let e = &mut c.targets[t];
        e.core.assert_reset();
        e.reset_halt = halt;
    });
    Ok(())
}

/// `arp_reset deassert <halt>`
pub fn arp_reset_deassert(cx: &Cx, t: usize, halt: bool) -> Result<()> {
    let tr = cx.with(|c| -> Result<Transition> {
        if c.srst_asserted {
            c.adapter.as_deref_mut().ok_or_else(|| usage("no adapter"))?.deassert_srst(halt)?;
            c.srst_asserted = false;
        }
        let (mut io, e) = c.target_io(t)?;
        let name = e.name.clone();
        e.core.after_reset(&mut io, halt, &name)
    })?;
    handle_transition(cx, t, tr);
    Ok(())
}

/// `reset run|halt|init` (OpenOCD `ocd_process_reset_inner`).
pub fn reset(cx: &Cx, mode: &str) -> Result<()> {
    let halt = match mode {
        "halt" | "init" => true,
        "run" => false,
        _ => return Err(usage(format!("Invalid mode: {mode}, must be one of: halt, init, or run"))),
    };
    if cx.with(|c| std::mem::replace(&mut c.in_reset, true)) {
        return Err(usage("'reset' can not be invoked recursively"));
    }
    let r = reset_inner(cx, mode, halt);
    cx.with(|c| c.in_reset = false);
    emit(Event::ResetDone);
    r
}

fn reset_inner(cx: &Cx, mode: &str, halt: bool) -> Result<()> {
    let n = cx.with(|c| c.targets.len());
    let all: Vec<usize> = (0..n).collect();
    for &t in &all {
        fire(cx, t, "reset-start");
    }
    for &t in &all {
        if !cx.with(|c| c.targets[t].core.examined()) {
            // Cores that are only accessible after the startup software ran fail quietly.
            let quiet = cx.with(|c| !c.targets[t].core.examine_interval().is_zero());
            // Failed targets are examined again by the background poll; their recovery is logged.
            if examine_one(cx, t, quiet).is_err() && !quiet {
                cx.with(|c| {
                    c.targets[t].failure_logged = true;
                    c.targets[t].unreachable = true;
                });
            }
        }
    }
    for &t in &all {
        fire(cx, t, "reset-assert-pre");
    }
    for &t in &all {
        arp_reset_assert(cx, t, halt)?;
    }
    for &t in &all {
        fire(cx, t, "reset-assert-post");
    }
    for &t in &all {
        fire(cx, t, "reset-deassert-pre");
    }
    for &t in &all {
        arp_reset_deassert(cx, t, halt)?;
    }
    for &t in &all {
        fire(cx, t, "reset-deassert-post");
    }
    // Resets we triggered ourselves are not "detected" resets.
    cx.with(|c| c.adapter.as_deref_mut().map(|a| a.take_resets()));

    if halt {
        for &t in &all {
            if !cx.with(|c| c.targets[t].core.halts_after_reset()) {
                continue;
            }
            // A core the probe cannot drive (winIDEA without an instance for it)
            // cannot be halted by us either; leave it out rather than fail the
            // reset of the cores that can.
            let drivable = cx.with(|c| c.target_io(t).map(|(mut io, e)| e.core.drivable(&mut io)).unwrap_or(true));
            if !drivable {
                crate::log_warn!("[{}] not driven by this adapter; not waiting for it to halt", target_name(cx, t));
                continue;
            }
            if !cx.with(|c| c.targets[t].core.examined()) {
                if cx.with(|c| c.targets[t].defer_examine) {
                    continue;
                }
                examine_one(cx, t, false).map_err(|_| other(format!("TARGET: {} - Not examined", target_name(cx, t))))?;
            }
            cx.with(|c| c.in_reset = false);
            let w = wait_state(cx, t, TargetState::Halted, 1000);
            cx.with(|c| c.in_reset = true);
            if w.is_err() {
                return Err(other(format!("TARGET: {} - Not halted", target_name(cx, t))));
            }
        }
    }
    if mode == "init" {
        for &t in &all {
            if cx.with(|c| c.targets[t].core.state()) == TargetState::Halted {
                fire(cx, t, "reset-init");
            }
        }
    }
    for &t in &all {
        fire(cx, t, "reset-end");
    }
    Ok(())
}

/// `init`: adapter, targets, examination.
pub fn init(cx: &Cx) -> Result<()> {
    if cx.with(|c| c.initialized) {
        log_info!("'init' has already been called");
        return Ok(());
    }
    if cx.interp.command_exists("init_targets") {
        cx.interp.eval_global("init_targets").map_err(other)?;
    }
    cx.with(|c| -> Result<()> {
        let name = c
            .adapter_override
            .clone()
            .or_else(|| c.adapter_name.clone())
            .ok_or_else(|| usage("Debug Adapter has to be specified, see \"adapter driver\" command"))?;
        if c.adapter.as_ref().map(|a| a.driver_name()) != Some(name.as_str()) {
            c.adapter = Some(adapter::create(&name).ok_or_else(|| usage(format!("The specified debug interface was not found ({name})")))?);
        }
        let settings = c.adapter_settings.clone();
        let info = c.adapter.as_deref_mut().unwrap().init(&settings)?;
        for tap in &c.taps {
            // Device ID 0: not connected yet (target not powered), checked by the examination.
            if info.device_id != 0 && !tap.expected_ids.is_empty() && !tap.expected_ids.contains(&info.device_id) {
                return Err(other(format!(
                    "{}: device reports ID 0x{:08x}, expected {} - wrong target configuration?",
                    tap.name,
                    info.device_id,
                    tap.expected_ids.iter().map(|i| format!("0x{i:08x}")).collect::<Vec<_>>().join(" or ")
                )));
            }
        }
        c.mode = Mode::Exec;
        c.initialized = true;
        // A probe with a costly status query (winIDEA: ~1.8 ms per halted core)
        // asks for a slower poll, unless the cfg chose a period itself.
        if c.poll_period_ms == crate::ctx::DEFAULT_POLL_PERIOD_MS {
            if let Some(ms) = c.adapter.as_deref().and_then(|a| a.default_poll_period_ms()) {
                c.poll_period_ms = ms;
            }
        }
        if c.current.is_none() && !c.targets.is_empty() {
            c.current = Some(0);
        }
        Ok(())
    })?;
    let n = cx.with(|c| c.targets.len());
    for t in 0..n {
        if !cx.with(|c| c.targets[t].defer_examine) {
            let quiet = cx.with(|c| !c.targets[t].core.examine_interval().is_zero());
            // Failed targets are examined again by the background poll; their recovery is logged.
            if examine_one(cx, t, quiet).is_err() && !quiet {
                cx.with(|c| {
                    c.targets[t].failure_logged = true;
                    c.targets[t].unreachable = true;
                });
            }
        }
    }
    // A reset that happened before we attached is not a "detected" reset.
    cx.with(|c| c.adapter.as_deref_mut().map(|a| a.take_resets()));
    if cx.interp.command_exists("init_board") {
        cx.interp.eval_global("init_board").map_err(other)?;
    }
    Ok(())
}

pub fn read_reg(cx: &Cx, t: usize, idx: usize) -> Result<u32> {
    cx.with(|c| {
        let (mut io, e) = c.target_io(t)?;
        e.core.read_reg(&mut io, idx)
    })
}

pub fn write_reg(cx: &Cx, t: usize, idx: usize, value: u32) -> Result<()> {
    cx.with(|c| {
        let (mut io, e) = c.target_io(t)?;
        e.core.write_reg(&mut io, idx, value)
    })
}

pub fn add_breakpoint(cx: &Cx, t: usize, addr: u32, len: u32, kind: BpKind) -> Result<u32> {
    require_examined(cx, t)?;
    if kind == BpKind::Soft {
        return Err(Error::Resource("Only hardware breakpoints are supported".into()));
    }
    cx.with(|c| {
        if let Some(bp) = c.targets[t].breakpoints.iter().find(|b| b.addr == addr) {
            return Err(usage(format!("Duplicate Breakpoint address: 0x{:08x} (BP {})", addr, bp.id)));
        }
        let id = c.next_bp_id;
        let (mut io, e) = c.target_io(t)?;
        let slot = e.core.add_breakpoint(&mut io, addr, len)?;
        e.breakpoints.push(Breakpoint { id, addr, len, kind, slot });
        c.next_bp_id += 1;
        Ok(id)
    })
}

pub fn remove_breakpoint(cx: &Cx, t: usize, addr: u32) -> Result<()> {
    cx.with(|c| {
        let (mut io, e) = c.target_io(t)?;
        let pos = e
            .breakpoints
            .iter()
            .position(|b| b.addr == addr)
            .ok_or_else(|| usage(format!("no breakpoint at address 0x{addr:08x} found")))?;
        let bp = e.breakpoints.remove(pos);
        e.core.remove_trigger(&mut io, bp.slot)
    })
}

pub fn add_watchpoint(cx: &Cx, t: usize, addr: u32, len: u32, access: WatchAccess) -> Result<u32> {
    require_examined(cx, t)?;
    cx.with(|c| {
        let id = c.next_bp_id;
        let (mut io, e) = c.target_io(t)?;
        let slot = e.core.add_watchpoint(&mut io, addr, len, access.bits())?;
        e.watchpoints.push(Watchpoint { id, addr, len, access, slot });
        c.next_bp_id += 1;
        Ok(id)
    })
}

pub fn remove_watchpoint(cx: &Cx, t: usize, addr: u32) -> Result<()> {
    cx.with(|c| {
        let (mut io, e) = c.target_io(t)?;
        let pos = e
            .watchpoints
            .iter()
            .position(|w| w.addr == addr)
            .ok_or_else(|| usage(format!("no watchpoint at address 0x{addr:08x} found")))?;
        let wp = e.watchpoints.remove(pos);
        e.core.remove_trigger(&mut io, wp.slot)
    })
}

pub fn read_memory(cx: &Cx, addr: u32, len: usize) -> Result<Vec<u8>> {
    cx.with(|c| c.io()?.read(addr, len))
}

/// Host memory like [`read_memory`]; unreadable words of the HSM bridge only fail their own bytes.
pub fn read_memory_partial(cx: &Cx, addr: u32, len: usize) -> Result<adapter::PartialRead> {
    cx.with(|c| c.io()?.read_partial(addr, len))
}

pub fn write_memory(cx: &Cx, addr: u32, data: &[u8]) -> Result<()> {
    cx.with(|c| c.io()?.write(addr, data))
}

/// The target that serves a memory access of core `t` to `addr..addr + len` through its own path:
/// `t` itself for addresses it owns (HSM-private memory, MCS program RAM), and the HSM for its
/// peripherals from every core, as they have no host address. `None`: host memory.
pub fn own_access_target(cx: &Cx, t: Option<usize>, addr: u32, len: usize) -> Option<usize> {
    let last = addr.wrapping_add(len.saturating_sub(1) as u32);
    cx.with(|c| {
        t.filter(|&i| c.targets.get(i).is_some_and(|e| e.core.owns_address(addr) || e.core.owns_address(last))).or_else(|| {
            (cortex_m::hsm_peripheral(addr) && cortex_m::hsm_peripheral(last))
                .then(|| c.targets.iter().position(|e| matches!(e.core, Core::CortexM(_))))
                .flatten()
        })
    })
}

/// Memory as a core sees it: HSM-private addresses go through the HSM debug window, MCS
/// program addresses to the module RAM, everything else is host memory.
pub fn read_memory_as(cx: &Cx, t: usize, addr: u32, len: usize) -> Result<Vec<u8>> {
    cx.with(|c| {
        let (mut io, e) = c.target_io(t)?;
        e.core.read_memory(&mut io, addr, len)
    })
}

/// [`read_memory_as`] where unreadable words of HSM-private memory (unused peripheral addresses)
/// only fail their own bytes.
pub fn read_memory_partial_as(cx: &Cx, t: usize, addr: u32, len: usize) -> Result<adapter::PartialRead> {
    cx.with(|c| {
        let (mut io, e) = c.target_io(t)?;
        e.core.read_memory_partial(&mut io, addr, len)
    })
}

pub fn write_memory_as(cx: &Cx, t: usize, addr: u32, data: &[u8]) -> Result<()> {
    cx.with(|c| {
        let (mut io, e) = c.target_io(t)?;
        e.core.write_memory(&mut io, addr, data)
    })
}

pub fn state(cx: &Cx, t: usize) -> (TargetState, DebugReason) {
    cx.with(|c| (c.targets[t].core.state(), c.targets[t].core.reason()))
}

/// The TriCore part of the run-state poll for a probe that performs run control
/// itself: there are no debug registers to read, and asking each core for its
/// status is the whole operation. The other cores stay in [`poll_all`]'s batch.
fn poll_run_controlled(c: &mut crate::ctx::Ctx) -> (Vec<(usize, Transition)>, Vec<usize>) {
    let mut out = Vec::new();
    let mut failed = Vec::new();
    for i in 0..c.targets.len() {
        let e = &c.targets[i];
        if !(matches!(e.core, Core::Tricore(_)) && e.core.examined() && e.poll_failures == 0) {
            continue;
        }
        let (mut io, e) = match c.target_io(i) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match e.core.poll(&mut io) {
            Ok(Transition::None) => {}
            Ok(tr) => out.push((i, tr)),
            Err(_) => failed.push(i),
        }
    }
    (out, failed)
}

/// A whole source-level step done by the probe, when it offers one for this
/// core and has `elf` loaded (see [`crate::target::RunControl::source_step`]).
///
/// `None` means it does not, and the caller steps instruction by instruction
/// with [`step_as`]. On winIDEA that difference is large: one source step over
/// measured ~200-350 ms, while every instruction step costs ~250 ms of its own.
///
/// `Some(Ok(true))`: halted at the new line, and the halt is announced here as
/// `step_as` does. `Some(Ok(false))`: still stepping (a step over a slow call);
/// the poll announces the halt.
pub fn source_step(cx: &Cx, t: usize, kind: SourceStep, elf: &std::path::Path) -> Option<Result<()>> {
    if let Err(e) = require_examined(cx, t) {
        return Some(Err(e));
    }
    let name = target_name(cx, t);
    if cx.with(|c| c.targets[t].core.state()) != TargetState::Halted {
        return Some(Err(Error::NotHalted(name)));
    }
    let res = cx.with(|c| {
        let (mut io, e) = c.target_io(t).ok()?;
        e.core.source_step(&mut io, kind, elf)
    })?;
    fire(cx, t, "step-start");
    fire(cx, t, "resumed");
    let r = match res {
        Ok(true) => {
            handle_transition(cx, t, Transition::Halted { debug: false, boot: false });
            Ok(())
        }
        Ok(false) => {
            emit(Event::Resumed { target: t, name });
            Ok(())
        }
        Err(e) => Err(e),
    };
    fire(cx, t, "step-end");
    Some(r)
}
