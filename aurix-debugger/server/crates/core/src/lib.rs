//! Protocol-neutral debug core.
//!
//! Layering follows OpenOCD: an [`adapter::Adapter`] provides memory access
//! (TAS), [`tricore::Tricore`] implements the target, and the Jim Tcl
//! interpreter runs the unchanged OpenOCD cfg scripts through the commands in
//! [`commands`]. All target access and Tcl evaluation happen on one thread
//! ([`server::Server`]); frontends talk to it through [`server::ServerHandle`].

pub mod log;

pub mod adapter;
pub mod arch;
pub mod commands;
pub mod ctx;
pub mod error;
pub mod events;
pub mod flash;
pub mod image;
pub mod ops;
pub mod server;
pub mod sim;
pub mod target;
pub mod tas_adapter;
pub mod telnet;
pub mod tricore;
pub mod unwind;
pub mod util;

pub use ctx::{Ctx, Cx, Shared};
pub use error::{Error, Result};
pub use events::Event;
pub use server::{Server, ServerHandle};
