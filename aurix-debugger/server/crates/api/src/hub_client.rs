//! Registration of a worker with the hub that started it.
//!
//! After `init` the worker posts its ports to the hub's loopback-only `/internal/register`, then
//! checks the hub every two seconds. When the hub's instance ID changes (it crashed and was
//! started again, or a newer one took over) or it was gone, the worker registers again, so a hub
//! restart keeps the sessions. A hub that refuses the registration (a worker of another
//! installation) makes the worker shut down.
//!
//! A tiny blocking HTTP/1.1 client: one request per connection, JSON bodies, loopback only.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::thread;
use std::time::Duration;

use serde_json::Value;

pub use api_spec::types::{RegistrationReply, WorkerRegistration};
use aurix_core::{log_info, log_warn};

const PERIOD: Duration = Duration::from_secs(2);

/// `http://host:port[/...]` -> `host:port`.
fn authority(url: &str) -> Option<String> {
    let rest = url.trim().strip_prefix("http://")?;
    let a = rest.split('/').next()?;
    if a.is_empty() {
        return None;
    }
    Some(if a.contains(':') { a.to_string() } else { format!("{a}:80") })
}

/// One request; returns the status and the body.
pub fn request(url: &str, method: &str, path: &str, body: Option<&Value>, timeout: Duration) -> std::io::Result<(u16, String)> {
    let addr = authority(url).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("not an http:// URL: {url}")))?;
    let sock = addr
        .parse::<std::net::SocketAddr>()
        .or_else(|_| std::net::ToSocketAddrs::to_socket_addrs(&addr).map_err(|e| e.to_string()).and_then(|mut a| a.next().ok_or_else(|| "no address".to_string())))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{addr}: {e}")))?;
    let mut s = TcpStream::connect_timeout(&sock, timeout)?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes())?;
    let mut resp = Vec::new();
    s.read_to_end(&mut resp)?;
    let text = String::from_utf8_lossy(&resp);
    let status = text.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    // The hub answers small JSON bodies; a chunked one is decoded crudely.
    let body = if head.to_ascii_lowercase().contains("transfer-encoding: chunked") { dechunk(body) } else { body.to_string() };
    Ok((status, body))
}

fn dechunk(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some((size, tail)) = rest.split_once("\r\n") {
        let Ok(n) = usize::from_str_radix(size.trim(), 16) else { break };
        if n == 0 || tail.len() < n {
            break;
        }
        out.push_str(&tail[..n]);
        rest = tail[n..].strip_prefix("\r\n").unwrap_or(&tail[n..]);
    }
    out
}

/// The ID of the hub's current run, if it answers.
fn hub_instance(url: &str) -> Option<String> {
    let (status, body) = request(url, "GET", "/health", None, Duration::from_secs(2)).ok()?;
    if status != 200 {
        return None;
    }
    let v: Value = serde_json::from_str(&body).ok()?;
    (v.get("role")?.as_str()? == "aurix-hub").then(|| v.get("instance")?.as_str().map(str::to_string)).flatten()
}

fn register(url: &str, reg: &WorkerRegistration) -> Result<RegistrationReply, String> {
    let body = serde_json::to_value(reg).map_err(|e| e.to_string())?;
    let (status, text) = request(url, "POST", "/internal/register", Some(&body), Duration::from_secs(5)).map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| format!("HTTP {status}: {e}: {text}"))
}

/// Registers with the hub at `url` and keeps the registration alive on a thread. `refused` is
/// called (once) when the hub refuses the worker.
pub fn start(url: String, reg: WorkerRegistration, refused: impl FnOnce(String) + Send + 'static) {
    thread::Builder::new()
        .name("hub-registration".into())
        .spawn(move || {
            let mut registered_with: Option<String> = None;
            let mut warned = false;
            loop {
                match hub_instance(&url) {
                    Some(inst) if registered_with.as_ref() != Some(&inst) => match register(&url, &reg) {
                        Ok(r) if r.ok => {
                            log_info!("Registered with the hub at {url} as session {}", reg.session);
                            registered_with = Some(r.instance);
                            warned = false;
                        }
                        Ok(r) => {
                            let why = r.error.unwrap_or_else(|| "refused".into());
                            log_warn!("The hub at {url} refused this worker: {why}");
                            refused(why);
                            return;
                        }
                        Err(e) => log_warn!("Registering with the hub at {url} failed: {e}"),
                    },
                    Some(_) => {}
                    None => {
                        if registered_with.take().is_some() || !warned {
                            log_warn!("The hub at {url} does not answer; registering again when it is back");
                            warned = true;
                        }
                    }
                }
                thread::sleep(PERIOD);
            }
        })
        .expect("spawn hub-registration");
}

/// Tells the hub that the worker ends (best effort).
pub fn deregister(url: &str, session: &str) {
    let body = serde_json::json!({ "session": session, "pid": std::process::id() });
    let _ = request(url, "POST", "/internal/deregister", Some(&body), Duration::from_secs(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(authority("http://127.0.0.1:3001").as_deref(), Some("127.0.0.1:3001"));
        assert_eq!(authority("http://localhost/x").as_deref(), Some("localhost:80"));
        assert_eq!(authority("https://x"), None);
        assert_eq!(dechunk("5\r\nhello\r\n0\r\n\r\n"), "hello");
    }
}
