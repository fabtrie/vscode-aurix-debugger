//! A hub that serves other machines: clients that are not on loopback need the token (except for
//! `/health`), `/internal/*` stays local, `allow_tcl = false` refuses their Tcl commands, and
//! uploads give `file:` IDs. The "other machine" is this one through its LAN address.

use std::net::{IpAddr, UdpSocket};
use std::time::Duration;

use serde_json::json;

use hub::{http1, HubConfig};

/// This machine's address on the network (no packet is sent), if it has one.
fn lan_ip() -> Option<IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

#[test]
fn remote_clients_need_the_token() {
    let home = std::env::temp_dir().join(format!("aurix-hub-remote-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    // SAFETY: set before the hub's threads start.
    unsafe { std::env::set_var("AURIX_HUB_HOME", &home) };

    // Exposed without a token: refused.
    assert!(hub::start_background(HubConfig { port: 0, bind: "0.0.0.0".into(), ..HubConfig::default() }).is_err());

    let running = hub::start_background(HubConfig { port: 0, bind: "0.0.0.0".into(), token: Some("s3cret".into()), allow_tcl: false, ..HubConfig::default() }).unwrap();
    let local = running.url();
    let t = Duration::from_secs(10);

    // From this machine: no token needed.
    let (status, v) = http1::json(&local, "GET", "/api/v1/hub", &[], None, t).unwrap();
    assert_eq!((status, v["token_required"].as_bool()), (200, Some(true)), "{v}");

    // Uploads: a `file:` ID, the same for the same content.
    let r = http1::request(&local, "POST", "/api/v1/files?name=C%3A%5Cbuild%5Capp.hex", &[("Content-Type", "application/octet-stream")], Some(b":00000001FF\n"), t).unwrap();
    assert_eq!(r.status, 200, "{}", r.text());
    let up = r.json().unwrap();
    let id = up["file"].as_str().unwrap().to_string();
    assert!(id.starts_with("file:") && id.ends_with("/app.hex"), "{id}");
    assert_eq!(std::fs::read(up["path"].as_str().unwrap()).unwrap(), b":00000001FF\n");
    assert_eq!(hub::files::resolve_id(&id).unwrap().to_string_lossy(), up["path"].as_str().unwrap());
    // An unknown ID in a request is refused before anything runs.
    let (status, e) = http1::json(&local, "POST", "/api/v1/sessions", &[], Some(&json!({ "connect": { "config_files": [format!("file:{}/x.cfg", "a".repeat(64))] } })), t).unwrap();
    assert_eq!(status, 400, "{e}");
    assert!(e["message"].as_str().unwrap().contains("unknown file ID"), "{e}");

    let Some(ip) = lan_ip() else {
        eprintln!("no LAN address: skipping the remote half");
        running.stop();
        return;
    };
    let remote = format!("http://{ip}:{}", running.port);
    let (status, v) = http1::json(&remote, "GET", "/health", &[], None, t).unwrap();
    assert_eq!((status, v["role"].as_str()), (200, Some("aurix-hub")), "health needs no token");
    let (status, _) = http1::json(&remote, "GET", "/api/v1/sessions", &[], None, t).unwrap();
    assert_eq!(status, 401, "no token");
    let (status, _) = http1::json(&remote, "GET", "/api/v1/sessions", &[("Authorization", "Bearer wrong")], None, t).unwrap();
    assert_eq!(status, 401, "a wrong token");
    let auth = [("Authorization", "Bearer s3cret")];
    let (status, v) = http1::json(&remote, "GET", "/api/v1/sessions", &auth, None, t).unwrap();
    assert_eq!((status, v["sessions"].as_array().map(Vec::len)), (200, Some(0)), "{v}");
    let (status, _) = http1::json(&remote, "POST", "/internal/register", &auth, Some(&json!({})), t).unwrap();
    assert_eq!(status, 404, "registration is for this machine only");
    // Tcl from another machine is switched off (before any session is looked up).
    let (status, e) = http1::json(&remote, "POST", "/api/v1/session/tcl", &auth, Some(&json!({ "command": "exec calc" })), t).unwrap();
    assert_eq!(status, 400, "{e}");
    assert!(e["message"].as_str().unwrap().contains("allow_tcl"), "{e}");
    // MCP over the network, with the token.
    let r = http1::request(&remote, "POST", "/mcp", &auth, Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18" } }).to_string().as_bytes()), t).unwrap();
    assert_eq!(r.status, 200);
    let sid = r.header("mcp-session-id").unwrap().to_string();
    let h = [("Authorization", "Bearer s3cret"), ("Mcp-Session-Id", sid.as_str()), ("MCP-Protocol-Version", "2025-06-18")];
    let call = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "tcl", "arguments": { "command": "version" } } });
    let r = http1::request(&remote, "POST", "/mcp", &h, Some(call.to_string().as_bytes()), t).unwrap().json().unwrap();
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("allow_tcl"), "{r}");
    running.stop();
    let _ = std::fs::remove_dir_all(&home);
}
