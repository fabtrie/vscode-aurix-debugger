//! Reading ORTI attributes through the debug core.
//!
//! The planning and the loaded-file state live in the `orti` crate, so the TCF frontend can use
//! them too. What is left here is the adapter that turns an expression evaluation into real bus
//! accesses on the core thread.

use aurix_core::{ops, ServerHandle};
use symbols::expr::{SymbolRef, Target};

/// A `Target` that reads the live system bus through the core thread.
///
/// `core` selects the core's own view of memory (HSM-private RAM, MCS program RAM); `None` is
/// the plain system bus, which is what OS data always lives on.
pub struct CoreTarget<'a> {
    pub handle: &'a ServerHandle,
    pub core: Option<usize>,
    /// Bus accesses made, so a caller can report what a refresh cost.
    pub reads: u32,
}

impl<'a> CoreTarget<'a> {
    pub fn new(handle: &'a ServerHandle) -> CoreTarget<'a> {
        CoreTarget { handle, core: None, reads: 0 }
    }
}

impl Target for CoreTarget<'_> {
    fn pc(&self) -> Option<u32> {
        None
    }
    fn reg(&self, _: u16) -> Option<u32> {
        None
    }
    fn read(&mut self, addr: u32, len: usize) -> Result<Vec<u8>, String> {
        self.reads += 1;
        let core = self.core;
        self.handle
            .call(move |cx| match core {
                Some(t) => ops::read_memory_as(cx, t, addr, len).map_err(|e| e.to_string()),
                None => ops::read_memory(cx, addr, len).map_err(|e| e.to_string()),
            })
            .unwrap_or_else(|| Err("server stopped".into()))
    }
    fn symbol(&mut self, _: &str) -> Option<SymbolRef> {
        None
    }
}

