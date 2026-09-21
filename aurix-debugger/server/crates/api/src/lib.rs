//! REST, MCP and Swagger frontend of aurix-dbg.
//!
//! Everything is generated from the API definition in `api-spec`: the REST routes, the MCP
//! tools, the OpenAPI document behind Swagger UI (`/docs`) and the Python client. The operations
//! share the session (breakpoints, stepping state, symbols) with the TCF frontend, so all
//! clients see each other's actions.
//!
//! The server runs on its own thread with a single-threaded async runtime; operations run on
//! blocking threads that talk to the core thread through the [`ServerHandle`].

mod core_impl;
mod daq_resolve;
mod orti_state;
mod daq_ws;
mod events;
mod http;
mod mcp;

use std::net::TcpListener;
use std::sync::Arc;
use std::thread;

use aurix_core::{log_error, ServerHandle};

pub use core_impl::CoreApi;

/// Starts the HTTP server for `session`. Returns the bound port.
pub fn start(handle: ServerHandle, session: Arc<tcf::Shared>, bind: &str, port: u16) -> std::io::Result<u16> {
    let listener = TcpListener::bind((aurix_core::telnet::bind_address(bind), port))?;
    let actual = listener.local_addr()?.port();
    listener.set_nonblocking(true)?;
    let rt = tokio::runtime::Builder::new_current_thread().enable_io().enable_time().max_blocking_threads(16).build()?;
    let hub = events::Hub::start(&handle, &session);
    let api = Arc::new(CoreApi { handle, session, hub });
    thread::Builder::new().name("api-http".into()).spawn(move || {
        rt.block_on(async move {
            let served = match tokio::net::TcpListener::from_std(listener) {
                Ok(l) => axum::serve(l, http::router(api)).await,
                Err(e) => Err(e),
            };
            if let Err(e) = served {
                log_error!("API server stopped: {e}");
            }
        })
    })?;
    Ok(actual)
}
