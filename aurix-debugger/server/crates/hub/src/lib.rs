//! aurix-hub: the always-on front door of aurix-dbg.
//!
//! One process per machine, on a fixed port (3001). It serves MCP, REST and Swagger, opens
//! sessions (one `aurix-dbg` worker process per board) and forwards the session operations to
//! them. It never touches a probe itself, so it can restart while the workers keep their boards;
//! they register again. See the server README.

pub mod config;
#[cfg(windows)]
pub mod detach;
pub mod files;
pub mod http1;
pub mod install;
pub mod log;
pub mod mcp;
pub mod paths;
pub mod server;
pub mod spawn;
pub mod state;
pub mod stdio;

use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

pub use config::HubConfig;
pub use state::Hub;

/// Binds the hub's port. With `wait`, retries for 20 s (a hub that is stopping still holds it).
fn bind(cfg: &HubConfig, wait: bool) -> Result<TcpListener, String> {
    let host = if cfg.bind == "localhost" { "127.0.0.1" } else { cfg.bind.as_str() };
    let t0 = Instant::now();
    loop {
        match TcpListener::bind((host, cfg.port)) {
            Ok(l) => return Ok(l),
            Err(e) if wait && t0.elapsed() < Duration::from_secs(20) => {
                let _ = e;
                thread::sleep(Duration::from_millis(200));
            }
            Err(e) => return Err(format!("cannot listen on {host}:{}: {e}", cfg.port)),
        }
    }
}

/// Whether an aurix-hub answers on `port`.
pub fn hub_answers(port: u16) -> bool {
    http1::json(&format!("http://127.0.0.1:{port}"), "GET", "/health", &[], None, Duration::from_secs(2)).is_ok_and(|(s, v)| s == 200 && v["role"] == "aurix-hub")
}

async fn run(hub: Arc<Hub>, listener: TcpListener) -> std::io::Result<()> {
    listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;
    let app = server::router(hub.clone()).into_make_service_with_connect_info::<SocketAddr>();
    let stop = hub.clone();
    tokio::select! {
        r = axum::serve(listener, app) => r,
        _ = async move { stop.shutdown.notified().await } => Ok(()),
        _ = tokio::signal::ctrl_c() => {
            hlog!("Ctrl-C: stopping");
            Ok(())
        }
    }
}

/// Serves until stopped (the `serve` command). Returns quietly when another hub already serves
/// the port.
pub fn serve(cfg: HubConfig, wait_port: bool) -> Result<(), String> {
    cfg.check()?;
    let listener = match bind(&cfg, wait_port) {
        Ok(l) => l,
        Err(e) => {
            if hub_answers(cfg.port) {
                hlog!("another aurix-hub serves port {}; exiting", cfg.port);
                return Ok(());
            }
            return Err(format!("{e} (another program holds the port; set `port` in {})", paths::hub_toml().display()));
        }
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(cfg.port);
    hlog!("aurix-hub {} on {}:{port} (MCP: http://127.0.0.1:{port}/mcp, API docs: http://127.0.0.1:{port}/docs)", env!("CARGO_PKG_VERSION"), cfg.bind);
    let hub = Hub::new(cfg, port);
    hub.start_monitor();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().max_blocking_threads(64).build().map_err(|e| e.to_string())?;
    let r = rt.block_on(run(hub.clone(), listener));
    hub.stop();
    rt.shutdown_timeout(Duration::from_secs(2));
    hlog!("aurix-hub stopped");
    r.map_err(|e| e.to_string())
}

/// A hub serving on a thread (tests, embedding).
pub struct Running {
    pub hub: Arc<Hub>,
    pub port: u16,
    thread: Option<thread::JoinHandle<()>>,
}

impl Running {
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Stops serving and waits until the port is free. The workers keep running.
    pub fn stop(mut self) {
        self.hub.stop();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Starts a hub on a thread; `cfg.port` 0 picks a free port.
pub fn start_background(cfg: HubConfig) -> Result<Running, String> {
    cfg.check()?;
    let listener = bind(&cfg, cfg.port != 0)?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let hub = Hub::new(cfg, port);
    hub.start_monitor();
    let h = hub.clone();
    let thread = thread::Builder::new()
        .name("aurix-hub".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("runtime");
            let _ = rt.block_on(run(h.clone(), listener));
            h.stop();
            rt.shutdown_timeout(Duration::from_secs(1));
        })
        .map_err(|e| e.to_string())?;
    Ok(Running { hub, port, thread: Some(thread) })
}
