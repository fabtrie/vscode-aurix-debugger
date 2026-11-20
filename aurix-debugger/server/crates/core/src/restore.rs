//! What a reset clears and the debugger sets up again, in one place: after a reset the device
//! watch saw ([`crate::reset_watch`]), after the server's own reset sequence, and at the attach.
//!
//! The device watch runs inside an access of whoever accessed the device, with the server state
//! borrowed: what it needs is shared through [`RestoreSet`] (never borrowed across an access, so
//! that the watch can act in the middle of an attempt of the poll or a command).

use std::cell::RefCell;
use std::rc::Rc;

use crate::adapter::Io;
use crate::ctx::Cx;
use crate::suspend::{self, Select, SuspendSet, Why};

/// The set-up a reset clears, as the server state describes it.
#[derive(Debug, Default)]
pub struct RestoreSet {
    pub suspend: RefCell<SuspendSet>,
    pub tl1: RefCell<crate::tl1::Plan>,
    pub hsm: std::cell::Cell<crate::hsm::Bridge>,
}

pub type SharedRestore = Rc<RestoreSet>;

/// A reset was reported: everything is to be set up again (before the device answers).
pub fn mark_reset(rs: &RestoreSet) {
    rs.suspend.borrow_mut().mark_reset();
}

/// After a reset, once OCDS is on again: the set-up it cleared.
pub fn after_reset(io: &mut Io, rs: &RestoreSet) {
    mark_reset(rs);
    let plan = rs.tl1.borrow().clone();
    crate::tl1::write_config(io, &plan);
    suspend::attempt(io, &rs.suspend, Why::Reset, Select::All);
    rs.hsm.get().debug_enable(io);
}

/// After the server's own reset sequence (cores halted at the reset vector, or running for `reset
/// run`): before the reset handlers run.
pub fn after_own_reset(cx: &Cx) {
    let rs = cx.with(|c| c.restore.clone());
    cx.with(|c| {
        if let Ok(mut io) = c.io() {
            after_reset(&mut io, &rs);
        }
    });
}

/// At `init`, after the examination (which switched OCDS on): the attach starts like a reset. Not
/// when no TriCore core was examined (a passive attach writes nothing).
pub fn on_attach(cx: &Cx) {
    let rs = cx.with(|c| c.restore.clone());
    mark_reset(&rs);
    let examined = cx.with(|c| c.targets.iter().any(|e| matches!(e.core, crate::target::Core::Tricore(_)) && e.core.examined()));
    if !examined {
        return;
    }
    crate::tl1::on_attach(cx);
    cx.with(|c| {
        if let Ok(mut io) = c.io() {
            suspend::attempt(&mut io, &rs.suspend, Why::Attach, Select::All);
        }
    });
}
