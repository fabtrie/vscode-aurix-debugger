//! The API client an operation runs for (`X-Aurix-Client`): the owner of the breakpoints it
//! sets and the holder of the control lease it takes.
//!
//! Operations run on blocking threads, one call at a time per thread, so the client is a
//! thread-local set around the call instead of an argument of every trait method.

use std::cell::RefCell;

thread_local! {
    static CLIENT: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// The hub's own client name (closing a session ignores the lease).
pub const HUB: &str = "aurix-hub";

/// Runs `f` as client `client`.
pub fn with_client<R>(client: Option<String>, f: impl FnOnce() -> R) -> R {
    let old = CLIENT.with(|c| c.replace(client));
    let r = f();
    CLIENT.with(|c| *c.borrow_mut() = old);
    r
}

/// The client of the running operation; none for a client that did not name itself.
pub fn current() -> Option<String> {
    CLIENT.with(|c| c.borrow().clone())
}

/// A client name as it may appear in the header: printable, without spaces, at most 128 bytes.
pub fn valid(name: &str) -> bool {
    !name.is_empty() && name.len() <= 128 && name.bytes().all(|b| b.is_ascii_graphic())
}
