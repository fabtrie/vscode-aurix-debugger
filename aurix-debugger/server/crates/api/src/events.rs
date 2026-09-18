//! Core and session events for the API: the SSE stream and waiting for a target state.
//!
//! The API subscribes to the core events exactly once: a thread drains them, so the unbounded
//! core channel never grows, and fans them out through a bounded broadcast channel (a slow SSE
//! client misses events instead of using memory).

use std::collections::HashMap;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;

use tokio::sync::broadcast;

use api_spec::types::ApiEvent;
use aurix_core::{Event, ServerHandle};

pub struct Hub {
    tx: broadcast::Sender<ApiEvent>,
    /// Woken by run state changes (`wait_state`).
    waiters: Mutex<Vec<SyncSender<()>>>,
}

impl Hub {
    pub fn start(handle: &ServerHandle, session: &Arc<tcf::Shared>) -> Arc<Hub> {
        let (tx, _) = broadcast::channel(1024);
        let hub = Arc::new(Hub { tx, waiters: Mutex::new(Vec::new()) });

        let core = handle.subscribe();
        let (h, handle) = (hub.clone(), handle.clone());
        thread::Builder::new()
            .name("api-events".into())
            .spawn(move || {
                let mut names = HashMap::new();
                for ev in core {
                    if matches!(ev, Event::Shutdown) {
                        break;
                    }
                    let wakes = matches!(
                        ev,
                        Event::Halted { .. } | Event::Resumed { .. } | Event::ResetDone | Event::ResetDetected | Event::Examined { .. } | Event::Unreachable { .. }
                    );
                    if wakes {
                        h.wake();
                    }
                    if h.tx.receiver_count() > 0 {
                        if let Some(e) = convert(&handle, &mut names, ev) {
                            let _ = h.tx.send(e);
                        }
                    }
                }
            })
            .expect("spawn api-events");

        let changes = session.subscribe();
        let h = hub.clone();
        thread::Builder::new()
            .name("api-session-events".into())
            .spawn(move || {
                for ev in changes {
                    let tcf::SessionEvent::Breakpoints { added, changed, removed } = ev;
                    let _ = h.tx.send(ApiEvent::BreakpointsChanged { added, changed, removed });
                }
            })
            .expect("spawn api-session-events");
        hub
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ApiEvent> {
        self.tx.subscribe()
    }

    /// A receiver woken (at least once) by every following run state change.
    pub fn waiter(&self) -> Receiver<()> {
        let (tx, rx) = sync_channel(1);
        self.waiters.lock().unwrap().push(tx);
        rx
    }

    fn wake(&self) {
        self.waiters.lock().unwrap().retain(|w| !matches!(w.try_send(()), Err(TrySendError::Disconnected(_))));
    }
}

/// Target name by index; looked up on the core thread when an event carries only the index.
fn name(handle: &ServerHandle, names: &mut HashMap<usize, String>, t: usize) -> String {
    if !names.contains_key(&t) {
        if let Some(all) = handle.call(|cx| cx.with(|c| c.targets.iter().map(|e| e.name.clone()).collect::<Vec<_>>())) {
            names.extend(all.into_iter().enumerate());
        }
    }
    names.get(&t).cloned().unwrap_or_else(|| t.to_string())
}

fn convert(handle: &ServerHandle, names: &mut HashMap<usize, String>, ev: Event) -> Option<ApiEvent> {
    Some(match ev {
        Event::Halted { target, name, reason, pc, .. } => {
            names.insert(target, name.clone());
            ApiEvent::Halted { target: name, reason: reason.name().into(), pc }
        }
        Event::Resumed { target, name } => {
            names.insert(target, name.clone());
            ApiEvent::Resumed { target: name }
        }
        Event::Examined { target, name } => {
            names.insert(target, name.clone());
            ApiEvent::Examined { target: name }
        }
        Event::ResetDetected => ApiEvent::ResetDetected,
        Event::ResetDone => ApiEvent::ResetDone,
        Event::Unreachable { target } => ApiEvent::Unreachable { target: name(handle, names, target) },
        Event::MemoryChanged { target, addr, len } => {
            ApiEvent::MemoryChanged { target: target.map(|t| name(handle, names, t)), address: addr, length: len }
        }
        Event::RegisterChanged { target, name: reg } => ApiEvent::RegisterChanged { target: name(handle, names, target), name: reg },
        Event::Log { text } => ApiEvent::Log { text },
        Event::Shutdown => return None,
    })
}
