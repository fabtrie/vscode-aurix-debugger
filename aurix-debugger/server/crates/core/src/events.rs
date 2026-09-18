//! Events broadcast from the core thread to frontends.

use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;

use crate::target::DebugReason;

#[derive(Clone, Debug)]
pub enum Event {
    Halted { target: usize, name: String, reason: DebugReason, pc: Option<u32>, trigger: Option<usize> },
    Resumed { target: usize, name: String },
    /// A device reset was noticed (external or requested).
    ResetDetected,
    /// `reset` command finished.
    ResetDone,
    Examined { target: usize, name: String },
    /// A target has not been accessible for [`crate::target::UNREACHABLE_TIME`] (e.g. target not
    /// powered); it is accessible again with the next `Examined`.
    Unreachable { target: usize },
    Log { text: String },
    Shutdown,
}

static SUBSCRIBERS: Mutex<Vec<Sender<Event>>> = Mutex::new(Vec::new());

pub fn subscribe() -> Receiver<Event> {
    let (tx, rx) = channel();
    SUBSCRIBERS.lock().unwrap().push(tx);
    rx
}

pub fn emit(ev: Event) {
    let mut subs = SUBSCRIBERS.lock().unwrap();
    subs.retain(|s| s.send(ev.clone()).is_ok());
}
