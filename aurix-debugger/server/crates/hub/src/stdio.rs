//! `aurix-hub stdio`: MCP over stdio for clients that start their servers as processes.
//!
//! A thin wrapper: each JSON-RPC line from stdin goes to the hub's `/mcp`, each answer back to
//! stdout. It starts the hub of this machine when none answers (a hub it starts keeps running
//! for other clients after the wrapper ends), survives a hub restart by initializing again, and
//! ends its MCP session when stdin closes, which removes this client's breakpoints at once.
//!
//! Requests are forwarded concurrently (a long flash must not hold up a `ping`); answers are
//! written whole, one per line.

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config::HubConfig;
use crate::{hlog, http1, paths, spawn};

const CALL_TIMEOUT: Duration = Duration::from_secs(3700);

struct Link {
    url: String,
    token: Option<String>,
    local: bool,
    /// `Mcp-Session-Id` and protocol version of the current initialization.
    session: Mutex<Option<String>>,
    version: Mutex<Option<String>>,
    /// The client's `initialize` request, replayed after a hub restart.
    init: Mutex<Option<Value>>,
    out: Mutex<std::io::Stdout>,
}

fn is_loopback(url: &str) -> bool {
    http1::split_url(url).map(|(a, _)| a.starts_with("127.") || a.starts_with("localhost:") || a.starts_with("[::1]:")).unwrap_or(false)
}

fn healthy(url: &str) -> bool {
    http1::json(url, "GET", "/health", &[], None, Duration::from_secs(2)).is_ok_and(|(s, v)| s == 200 && v["role"] == "aurix-hub")
}

/// Starts the hub next to this executable and waits until it answers.
fn start_hub(url: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    hlog!("no hub at {url}: starting {}", exe.display());
    spawn::spawn_detached(&exe, &["serve"]).map_err(|e| format!("cannot start the hub: {e}"))?;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(15) {
        if healthy(url) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!("the hub did not start (see {})", paths::hub_log().display()))
}

impl Link {
    fn post(&self, msg: &Value, session: Option<&str>) -> Result<http1::Response, String> {
        let body = msg.to_string();
        let mut h: Vec<(&str, String)> = vec![("Accept", "application/json, text/event-stream".into())];
        if let Some(s) = session {
            h.push(("Mcp-Session-Id", s.to_string()));
        }
        if let Some(v) = self.version.lock().unwrap().clone() {
            h.push(("MCP-Protocol-Version", v));
        }
        if let Some(t) = &self.token {
            h.push(("Authorization", format!("Bearer {t}")));
        }
        let hr: Vec<(&str, &str)> = h.iter().map(|(k, v)| (*k, v.as_str())).collect();
        http1::request(&self.url, "POST", "/mcp", &hr, Some(body.as_bytes()), CALL_TIMEOUT)
    }

    fn write(&self, v: &Value) {
        let mut out = self.out.lock().unwrap();
        let _ = writeln!(out, "{v}");
        let _ = out.flush();
    }

    /// Initializes again with the client's `initialize` (after a hub restart).
    fn reinit(&self) -> Result<(), String> {
        if self.local && !healthy(&self.url) {
            start_hub(&self.url)?;
        }
        let Some(init) = self.init.lock().unwrap().clone() else { return Err("not initialized".into()) };
        let r = self.post(&init, None)?;
        if r.status != 200 {
            return Err(format!("initialize: HTTP {}", r.status));
        }
        *self.session.lock().unwrap() = r.header("mcp-session-id").map(str::to_string);
        let _ = self.post(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }), self.session.lock().unwrap().clone().as_deref());
        hlog!("initialized again with the hub (session {:?})", self.session.lock().unwrap());
        Ok(())
    }

    /// Forwards one message; returns the answer to write, if any.
    fn forward(&self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let is_init = msg.get("method").and_then(Value::as_str) == Some("initialize");
        if is_init {
            *self.init.lock().unwrap() = Some(msg.clone());
        }
        let session = if is_init { None } else { self.session.lock().unwrap().clone() };
        let mut r = self.post(msg, session.as_deref());
        let retry = match &r {
            Err(_) => true,
            Ok(resp) => resp.status == 404 && session.is_some(),
        };
        if retry && !is_init && self.reinit().is_ok() {
            let s = self.session.lock().unwrap().clone();
            r = self.post(msg, s.as_deref());
        } else if retry && is_init && self.local && start_hub(&self.url).is_ok() {
            r = self.post(msg, None);
        }
        match r {
            Ok(resp) if resp.status == 200 => {
                if is_init {
                    *self.session.lock().unwrap() = resp.header("mcp-session-id").map(str::to_string);
                }
                let v: Value = resp.json().ok()?;
                if is_init {
                    if let Some(ver) = v["result"]["protocolVersion"].as_str() {
                        *self.version.lock().unwrap() = Some(ver.to_string());
                    }
                }
                Some(v)
            }
            Ok(resp) if resp.status == 202 => None,
            Ok(resp) => id.map(|id| json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": format!("aurix-hub answered HTTP {}: {}", resp.status, resp.text()) } })),
            Err(e) => id.map(|id| json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32603, "message": format!("aurix-hub at {} is not reachable: {e}", self.url) } })),
        }
    }
}

pub fn run(url: Option<String>, token: Option<String>) -> i32 {
    crate::log::quiet();
    crate::log::to_file(&paths::logs_dir().join("stdio.log"));
    let cfg = HubConfig::load().unwrap_or_default();
    let url = url.unwrap_or_else(|| format!("http://127.0.0.1:{}", cfg.port)).trim_end_matches('/').to_string();
    let local = is_loopback(&url);
    let token = token.or_else(|| std::env::var("AURIX_HUB_TOKEN").ok().filter(|t| !t.is_empty())).or_else(|| if local { None } else { cfg.token.clone() });
    hlog!("stdio wrapper for {url}");
    if local && !healthy(&url) {
        if let Err(e) = start_hub(&url) {
            hlog!("{e}");
        }
    }
    let link = Arc::new(Link {
        url,
        token,
        local,
        session: Mutex::new(None),
        version: Mutex::new(None),
        init: Mutex::new(None),
        out: Mutex::new(std::io::stdout()),
    });
    let mut workers = Vec::new();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                link.write(&json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": format!("Parse error: {e}") } }));
                continue;
            }
        };
        // `initialize` and notifications in order; requests concurrently.
        let concurrent = msg.get("id").is_some() && msg.get("method").and_then(Value::as_str).is_some_and(|m| m != "initialize");
        if concurrent {
            let l = link.clone();
            workers.push(thread::spawn(move || {
                if let Some(a) = l.forward(&msg) {
                    l.write(&a);
                }
            }));
            workers.retain(|w| !w.is_finished());
        } else if let Some(a) = link.forward(&msg) {
            link.write(&a);
        }
    }
    for w in workers {
        let _ = w.join();
    }
    // End the MCP session: the hub removes this client's breakpoints and lease.
    if let Some(s) = link.session.lock().unwrap().clone() {
        let mut h = vec![("Mcp-Session-Id", s.clone())];
        if let Some(t) = &link.token {
            h.push(("Authorization", format!("Bearer {t}")));
        }
        let hr: Vec<(&str, &str)> = h.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let _ = http1::request(&link.url, "DELETE", "/mcp", &hr, None, Duration::from_secs(10));
        hlog!("ended MCP session {s}");
    }
    0
}
