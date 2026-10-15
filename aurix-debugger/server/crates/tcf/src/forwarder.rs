//! Turns core events into TCF events for all connected clients.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread;

use serde_json::{json, Value};

use aurix_core::target::DebugReason;
use aurix_core::Event;

use crate::contexts::{context_json, cores, read_pc, reason_text, CoreInfo, CONTAINER};
use crate::wire;
use crate::Shared;

pub fn start(shared: Arc<Shared>) {
    let rx = shared.handle.subscribe();
    thread::spawn(move || {
        let mut listed: BTreeMap<String, bool> = cores(&shared.handle).into_iter().filter(|c| c.visible).map(|c| (c.id, c.reachable)).collect();
        for ev in rx {
            if !matches!(ev, Event::Shutdown | Event::MemoryChanged { .. } | Event::RegisterChanged { .. }) {
                sync_listed(&shared, &mut listed);
            }
            match ev {
                // Writes of other frontends: the call stacks may have changed, clients re-read.
                Event::MemoryChanged { target, addr, len } => {
                    shared.stacks.lock().unwrap().clear();
                    let ctx = target.and_then(|t| core_by_index(&shared, t)).map_or_else(|| CONTAINER.to_string(), |c| c.id);
                    let ranges = json!([{ "addr": addr, "size": len }]);
                    shared.broadcast(&wire::event("Memory", "memoryChanged", &[json!(ctx).to_string(), ranges.to_string()]));
                }
                Event::RegisterChanged { target, name } => {
                    shared.invalidate_stack(target);
                    if let Some(c) = core_by_index(&shared, target) {
                        shared.broadcast(&wire::event("Registers", "registerChanged", &[json!(format!("{}.R.{name}", c.id)).to_string()]));
                    }
                }
                // A source-level step reports once, when it is done. Its
                // instruction steps also release and re-halt the other cores
                // (cfg step-start / TL1), which is not reported either.
                Event::Halted { target, reason, pc, .. } => {
                    // Any core may have run (TL1-gated steps run all cores).
                    shared.stacks.lock().unwrap().clear();
                    if !shared.any_stepping() {
                        halted(&shared, target, reason, pc);
                    }
                }
                Event::Resumed { target, .. } => {
                    shared.invalidate_stack(target);
                    if shared.any_stepping() {
                        continue;
                    }
                    if let Some(c) = core_by_index(&shared, target).filter(|c| c.visible) {
                        shared.broadcast(&wire::event("RunControl", "contextResumed", &[json!(c.id).to_string()]));
                    }
                }
                Event::ResetDone => {
                    shared.stacks.lock().unwrap().clear();
                    for c in cores(&shared.handle).into_iter().filter(|c| c.visible) {
                        if c.suspended() {
                            let pc = read_pc(&shared.handle, c.idx);
                            suspended_event(&shared, &c, pc, "Reset", &[], DebugReason::Reset);
                        } else {
                            shared.broadcast(&wire::event("RunControl", "contextResumed", &[json!(c.id).to_string()]));
                        }
                    }
                }
                Event::ResetDetected => shared.stacks.lock().unwrap().clear(),
                Event::Shutdown => break,
                _ => {}
            }
        }
    });
}

/// Announces cores that became visible (e.g. an MCS channel that started running) or were hidden
/// again (a reset) as RunControl contextAdded / contextRemoved. A core whose accessibility changed
/// is removed and added again, so the frontend shows its new name.
fn sync_listed(shared: &Shared, listed: &mut BTreeMap<String, bool>) {
    let all: Vec<CoreInfo> = cores(&shared.handle).into_iter().filter(|c| c.visible).collect();
    let changed = |c: &CoreInfo| listed.get(&c.id).is_some_and(|&r| r != c.reachable);
    let added: Vec<Value> = all.iter().filter(|c| !listed.contains_key(&c.id) || changed(c)).map(context_json).collect();
    let removed: Vec<String> = listed
        .keys()
        .filter(|id| !all.iter().any(|c| &c.id == *id))
        .cloned()
        .chain(all.iter().filter(|c| changed(c)).map(|c| c.id.clone()))
        .collect();
    if !removed.is_empty() {
        shared.broadcast(&wire::event("RunControl", "contextRemoved", &[json!(removed).to_string()]));
    }
    if !added.is_empty() {
        shared.broadcast(&wire::event("RunControl", "contextAdded", &[Value::from(added.clone()).to_string()]));
    }
    *listed = all.into_iter().map(|c| (c.id, c.reachable)).collect();
}

fn core_by_index(shared: &Shared, idx: usize) -> Option<CoreInfo> {
    cores(&shared.handle).into_iter().find(|c| c.idx == idx)
}

/// Reports a halt: breakpoint status (hit counts) first, then contextSuspended.
pub fn halted(shared: &Shared, target: usize, reason: DebugReason, pc: Option<u32>) {
    let Some(c) = core_by_index(shared, target).filter(|c| c.visible) else { return };
    let mut bp_ids = Vec::new();
    if reason == DebugReason::Breakpoint {
        if let Some(pc) = pc {
            let mut table = shared.bps.lock().unwrap();
            bp_ids = table.record_hit(target, pc);
            // Status (HitCount) first: the client then treats the following
            // contextSuspended as the one breakpoint stop.
            for id in &bp_ids {
                let st = table.status(id);
                shared.broadcast(&wire::event("Breakpoints", "status", &[json!(id).to_string(), st.to_string()]));
            }
        }
    }
    suspended_event(shared, &c, pc, reason_text(reason), &bp_ids, reason);
}

/// Reports the current state of the cores other than `except`, whose events
/// were held back during a source-level step.
pub fn sync_others(shared: &Shared, except: usize) {
    for c in cores(&shared.handle).into_iter().filter(|c| c.visible) {
        if c.idx == except || shared.is_stepping(c.idx) {
            continue;
        }
        shared.invalidate_stack(c.idx);
        if c.suspended() {
            let pc = read_pc(&shared.handle, c.idx);
            suspended_event(shared, &c, pc, reason_text(DebugReason::DbgRq), &[], DebugReason::DbgRq);
        } else {
            shared.broadcast(&wire::event("RunControl", "contextResumed", &[json!(c.id).to_string()]));
        }
    }
}

pub fn suspended_event(shared: &Shared, c: &CoreInfo, pc: Option<u32>, reason: &str, bps: &[String], r: DebugReason) {
    let mut data = json!({ "StateName": r.name(), "Context": c.id });
    if !bps.is_empty() {
        data["BPs"] = Value::from(bps.to_vec());
    }
    shared.broadcast(&wire::event(
        "RunControl",
        "contextSuspended",
        &[json!(c.id).to_string(), pc.unwrap_or(0).to_string(), json!(reason).to_string(), data.to_string()],
    ));
}
