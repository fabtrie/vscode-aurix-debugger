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
pub mod cortex_m;
pub mod crash;
pub mod daq;
pub mod gtm_mcs;
pub mod help;
pub mod ctx;
pub mod error;
pub mod events;
pub mod flash;
pub mod flash_loader;
pub mod image;
pub mod mcds_cmd;
pub mod mcds_trace;
pub mod ops;
pub mod reverse;
pub mod reverse_cmd;
pub mod reset_watch;
pub mod restore;
pub mod scr_monitor;
pub mod server;
pub mod sim;
pub mod sim_dmu;
pub mod sim_mcds;
pub mod sim_mcs;
pub mod sim_scr;
pub mod sim_tricore;
pub mod suspend;
pub mod target;
pub mod tas_adapter;
#[cfg(feature = "bluebox")]
pub mod bluebox_adapter;
#[cfg(feature = "winidea")]
pub mod winidea_adapter;
#[cfg(feature = "winidea")]
pub mod winidea_regs;
#[cfg(feature = "winidea")]
pub mod winidea_session;
pub mod telnet;
pub mod tl1;
pub mod tricore;
pub mod ucb;
pub mod ucb_cmd;
pub mod ucb_write;
pub mod unwind;
pub mod unwind_arm;
pub mod util;
pub mod xc800;

pub use ctx::{Ctx, Cx, Shared};
pub use error::{Error, Result};
pub use events::Event;
pub use server::{Server, ServerHandle};
