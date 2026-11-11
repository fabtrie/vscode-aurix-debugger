//! End to end: the hub with real worker processes on the simulated TC37x. MCP and REST through
//! the hub, sessions opened from a profile, the launch steps, two sessions, the TCF relay, a hub
//! restart that keeps the workers, and closing.
//!
//! One test function: it sets `AURIX_HUB_HOME` for the whole process.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use hub::{HubConfig, Running};

const CPU0: &str = "tc37x.cpu0";

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize().unwrap()
}

fn tcl_path(p: PathBuf) -> String {
    p.to_string_lossy().trim_start_matches(r"\\?\").replace('\\', "/")
}

struct Http(u16);

impl Http {
    fn request(&self, method: &str, path: &str, body: Option<Value>, headers: &[(&str, &str)]) -> (u16, Value, Vec<(String, String)>) {
        let mut s = TcpStream::connect(("127.0.0.1", self.0)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let mut req = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
            self.0,
            body.len()
        );
        for (k, v) in headers {
            req += &format!("{k}: {v}\r\n");
        }
        req += "\r\n";
        req += &body;
        s.write_all(req.as_bytes()).unwrap();
        let mut resp = Vec::new();
        s.read_to_end(&mut resp).unwrap();
        let resp = String::from_utf8_lossy(&resp).into_owned();
        let status: u16 = resp.split(' ').nth(1).unwrap().parse().unwrap();
        let (head, text) = resp.split_once("\r\n\r\n").unwrap_or((&resp, ""));
        let headers = head.lines().skip(1).filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect::<Vec<_>>();
        let text = if headers.iter().any(|(k, v)| k == "transfer-encoding" && v == "chunked") { dechunk(text) } else { text.to_string() };
        (status, serde_json::from_str(&text).unwrap_or(Value::Null), headers)
    }

    fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (status, v, _) = self.request(method, path, body, &[]);
        assert_eq!(status, 200, "{method} {path}: {v}");
        v
    }

    fn sse(&self, path: &str) -> Receiver<Value> {
        let mut s = TcpStream::connect(("127.0.0.1", self.0)).unwrap();
        write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n", self.0).unwrap();
        let (tx, rx) = channel();
        thread::spawn(move || {
            for line in BufReader::new(s).lines() {
                let Ok(line) = line else { break };
                if let Some(data) = line.strip_prefix("data:") {
                    if tx.send(serde_json::from_str(data.trim()).unwrap()).is_err() {
                        break;
                    }
                }
            }
        });
        rx
    }
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

struct Mcp<'a> {
    http: &'a Http,
    session: String,
    id: u32,
}

impl Mcp<'_> {
    fn open(http: &Http) -> Mcp<'_> {
        let (status, init, headers) = http.request(
            "POST",
            "/mcp",
            Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "1" } } })),
            &[],
        );
        assert_eq!(status, 200, "{init}");
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(init["result"]["serverInfo"]["name"], "aurix-hub");
        let session = headers.iter().find(|(k, _)| k == "mcp-session-id").map(|(_, v)| v.clone()).expect("Mcp-Session-Id");
        Mcp { http, session, id: 1 }
    }

    fn rpc(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let h = [("Mcp-Session-Id", self.session.as_str()), ("MCP-Protocol-Version", "2025-06-18")];
        let (status, r, _) = self.http.request("POST", "/mcp", Some(json!({ "jsonrpc": "2.0", "id": self.id, "method": method, "params": params })), &h);
        assert_eq!(status, 200, "{method}: {r}");
        r
    }

    /// A tool call: the structured result, or the text of guidance / an error.
    fn call(&mut self, name: &str, args: Value) -> (bool, Value) {
        let r = self.rpc("tools/call", json!({ "name": name, "arguments": args }));
        let res = &r["result"];
        let err = res["isError"] == true;
        let v = if res.get("structuredContent").is_some() {
            res["structuredContent"].clone()
        } else {
            let text = res["content"][0]["text"].as_str().unwrap_or_default();
            serde_json::from_str(text).unwrap_or(Value::String(text.to_string()))
        };
        (err, v)
    }

    fn ok(&mut self, name: &str, args: Value) -> Value {
        let (err, v) = self.call(name, args);
        assert!(!err, "{name}: {v}");
        v
    }
}

fn wait_until(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < timeout, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(100));
    }
}

fn start(port: u16) -> Running {
    let cfg = HubConfig {
        port,
        worker: Some(PathBuf::from(env!("CARGO_BIN_EXE_aurix-dbg"))),
        tcl_dir: Some(PathBuf::from(openocd_tcl::TCL_DIR)),
        ..HubConfig::default()
    };
    hub::start_background(cfg).unwrap()
}

/// Closes the sessions of the hub if the test fails half-way, so no worker is left running.
struct Cleanup(u16);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = hub::http1::json(&format!("http://127.0.0.1:{}", self.0), "POST", "/shutdown", &[], Some(&json!({ "now": true })), Duration::from_secs(30));
    }
}

#[test]
fn hub_sessions_end_to_end() {
    let home = std::env::temp_dir().join(format!("aurix-hub-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    // SAFETY: set before any thread of this test reads it.
    unsafe { std::env::set_var("AURIX_HUB_HOME", &home) };
    let profiles = hub::paths::profiles_dir();
    std::fs::create_dir_all(&profiles).unwrap();
    let commands = vec![
        "script board/infineon/kit_a2g_tc375_lite.cfg".to_string(),
        aurix_core::sim::SIM_BOARD_SETTINGS.to_string(),
        format!("script {}", tcl_path(repo().join("tc37x_ocds_init.cfg"))),
        "set tc3xx_hsm_ready_ms 0".to_string(),
    ];
    let profile = json!({ "connect": { "commands": commands.clone(), "adapter": "sim" }, "launch": { "reset": { "halt": true }, "stop_on_entry": true } });
    let toml_text = format!(
        "[connect]\ncommands = {}\nadapter = \"sim\"\n\n[launch]\nreset = {{ halt = true }}\nstop_on_entry = true\n",
        serde_json::to_string(&commands).unwrap()
    );
    std::fs::write(profiles.join("sim.toml"), toml_text).unwrap();
    let _ = profile;

    let running = start(0);
    let port = running.port;
    let _cleanup = Cleanup(port);
    let http = Http(port);
    let health = http.ok("GET", "/health", None);
    assert_eq!(health["role"], "aurix-hub");
    assert_eq!(http.ok("GET", "/api/v1/hub", None)["sessions"], 0);
    assert!(http.ok("GET", "/openapi.json", None)["paths"]["/api/v1/sessions/{session}/targets"].is_object());
    let hub_events = http.sse("/api/v1/events");

    // --- MCP without a session ---------------------------------------------------------------
    let mut mcp = Mcp::open(&http);
    let tools = mcp.rpc("tools/list", json!({}));
    let tools = tools["result"]["tools"].as_array().unwrap().clone();
    assert_eq!(tools.len(), api_spec::OPS.len() + api_spec::HUB_OPS.len());
    let halt = tools.iter().find(|t| t["name"] == "halt").unwrap();
    assert_eq!(halt["inputSchema"]["properties"]["session"]["type"], "string");
    let (err, g) = mcp.call("halt", json!({ "target": CPU0 }));
    assert!(!err, "guidance is not an error: {g}");
    assert_eq!(g["status"], "no_session", "{g}");
    assert_eq!(g["profiles"], json!(["sim"]));
    let profiles = mcp.ok("list_profiles", json!({}));
    assert_eq!(profiles["profiles"][0]["name"], "sim", "{profiles}");
    assert_eq!(profiles["profiles"][0]["connect"]["adapter"], "sim");

    // --- Open a session from the profile: connect + launch -------------------------------------
    let s1 = mcp.ok("open_session", json!({ "profile": "sim" }));
    assert_eq!(s1["state"], "running", "{s1}");
    assert_eq!(s1["launch"]["phase"], "ready", "{s1}");
    let steps: Vec<&str> = s1["launch"]["steps"].as_array().unwrap().iter().map(|s| s["step"].as_str().unwrap()).collect();
    assert_eq!(steps, ["connect", "reset", "stop_on_entry"], "{s1}");
    assert!(s1["targets"].as_array().unwrap().iter().any(|t| t == CPU0), "{s1}");
    assert_eq!(s1["opened_by"], format!("mcp:{}", mcp.session));
    let id1 = s1["session"].as_str().unwrap().to_string();
    let tcf1 = s1["tcf_port"].as_u64().unwrap() as u16;
    let ev = hub_events.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!((ev["type"].as_str(), ev["session"].as_str()), (Some("session_opened"), Some(id1.as_str())), "{ev}");

    // Session operations without `session`: the only one.
    let t = mcp.ok("targets", json!({}));
    assert!(t["targets"].as_array().unwrap().iter().any(|t| t["name"] == CPU0 && t["state"] == "halted"), "{t}");
    let bp = mcp.ok("add_breakpoint", json!({ "address": "0x80000100" }));
    assert_eq!(bp["client"], format!("mcp:{}", mcp.session), "{bp}");
    let bp_id = bp["id"].as_str().unwrap().to_string();

    // REST through the hub: the only session, by ID and by name.
    let bps = http.ok("GET", "/api/v1/session/breakpoints", None);
    assert!(bps["breakpoints"].as_array().unwrap().iter().any(|b| b["id"] == bp_id.as_str()), "{bps}");
    assert_eq!(http.ok("GET", &format!("/api/v1/sessions/{id1}/info"), None)["session"], id1.as_str());
    assert_eq!(http.ok("GET", "/api/v1/sessions/sim/info", None)["session"], id1.as_str());
    let (status, e, _) = http.request("GET", "/api/v1/sessions/nope/info", None, &[]);
    assert_eq!(status, 404, "{e}");

    // --- A second session, by REST --------------------------------------------------------------
    let (status, s2, _) = http.request("POST", "/api/v1/sessions", Some(json!({ "profile": "sim", "name": "second", "launch": { "stop_on_entry": false, "reset": "none" } })), &[("X-Aurix-Client", "rest-test")]);
    assert_eq!(status, 200, "{s2}");
    assert_eq!((s2["name"].as_str(), s2["opened_by"].as_str()), (Some("second"), Some("rest-test")), "{s2}");
    let id2 = s2["session"].as_str().unwrap().to_string();
    let (err, g) = mcp.call("targets", json!({}));
    assert!(!err);
    assert_eq!(g["status"], "multiple_sessions", "{g}");
    assert_eq!(g["sessions"].as_array().unwrap().len(), 2);
    mcp.ok("targets", json!({ "session": "second" }));
    let (status, e, _) = http.request("GET", "/api/v1/session/targets", None, &[]);
    assert_eq!((status, e["status"].as_str()), (409, Some("multiple_sessions")), "{e}");
    let list = http.ok("GET", "/api/v1/sessions", None);
    assert_eq!(list["sessions"].as_array().unwrap().len(), 2);

    // --- The TCF relay: a TCF client over the hub's WebSocket gets the worker's hello -------------
    {
        let (mut ws, _) = tungstenite::connect(format!("ws://127.0.0.1:{port}/api/v1/sessions/{id1}/tcf")).unwrap();
        let mut got = Vec::new();
        let t0 = Instant::now();
        while !String::from_utf8_lossy(&got).contains("Hello") && t0.elapsed() < Duration::from_secs(5) {
            if let Ok(tungstenite::Message::Binary(b)) = ws.read() {
                got.extend_from_slice(&b);
            }
        }
        let text = String::from_utf8_lossy(&got);
        assert!(text.contains("Locator") && text.contains("Hello"), "{text}");
        let _ = ws.close(None);
        // The relay goes to the same TCF server a local client reaches directly.
        let mut direct = TcpStream::connect(("127.0.0.1", tcf1)).unwrap();
        direct.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = [0u8; 256];
        let n = direct.read(&mut buf).unwrap();
        assert!(String::from_utf8_lossy(&buf[..n]).contains("Hello"));
    }

    // --- Ending the MCP session removes its breakpoints -----------------------------------------
    let (status, _, _) = http.request("DELETE", "/mcp", None, &[("Mcp-Session-Id", mcp.session.as_str())]);
    assert_eq!(status, 204);
    let bps = http.ok("GET", &format!("/api/v1/sessions/{id1}/breakpoints"), None);
    assert!(!bps["breakpoints"].as_array().unwrap().iter().any(|b| b["id"] == bp_id.as_str()), "{bps}");
    let (status, _, _) = http.request("POST", "/mcp", Some(json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/list" })), &[("Mcp-Session-Id", mcp.session.as_str())]);
    assert_eq!(status, 404, "an ended MCP session must initialize again");

    // --- A hub restart keeps the sessions: the workers register with the new hub ----------------
    let pids: Vec<u64> = list["sessions"].as_array().unwrap().iter().map(|s| s["pid"].as_u64().unwrap()).collect();
    running.stop();
    assert!(hub::http1::json(&format!("http://127.0.0.1:{port}"), "GET", "/health", &[], None, Duration::from_secs(1)).is_err());
    let running = start(port);
    wait_until("the workers to register again", Duration::from_secs(15), || {
        http.ok("GET", "/api/v1/sessions", None)["sessions"].as_array().unwrap().iter().filter(|s| s["state"] == "running").count() == 2
    });
    let list = http.ok("GET", "/api/v1/sessions", None);
    let mut again: Vec<u64> = list["sessions"].as_array().unwrap().iter().map(|s| s["pid"].as_u64().unwrap()).collect();
    again.sort();
    let mut before = pids.clone();
    before.sort();
    assert_eq!(again, before, "the same worker processes");
    let t = http.ok("GET", &format!("/api/v1/sessions/{id2}/targets"), None);
    assert!(t["targets"].as_array().unwrap().iter().any(|t| t["name"] == CPU0));
    let log = http.ok("GET", &format!("/api/v1/sessions/{id1}/log?lines=50"), None);
    assert!(log["text"].as_str().unwrap().contains("Registered with the hub"), "{log}");

    // --- An uploaded file by its ID, and DAQ through the hub ----------------------------------------
    {
        let asm = home.join("daq.asm");
        std::fs::write(&asm, ".org 0x400\ng_daq: .var 0\n").unwrap();
        let data = std::fs::read(&asm).unwrap();
        let r = hub::http1::request(&format!("http://127.0.0.1:{port}"), "POST", "/api/v1/files?name=daq.asm", &[("Content-Type", "application/octet-stream")], Some(&data), Duration::from_secs(10)).unwrap();
        let file = r.json().unwrap()["file"].as_str().unwrap().to_string();
        let info = http.ok("POST", &format!("/api/v1/sessions/{id2}/symbols"), Some(json!({ "files": [file], "targets": [CPU0] })));
        let loaded = info["targets"][0]["files"][0].as_str().unwrap();
        assert!(loaded.contains("cache") && loaded.ends_with("daq.asm"), "the ID became the cached path: {info}");
        let (mut ws, _) = tungstenite::connect(format!("ws://127.0.0.1:{port}/api/v1/sessions/{id2}/daq")).unwrap();
        ws.send(tungstenite::Message::text(json!({ "op": "subscribe", "id": 1, "points": [{ "expression": "g_daq", "rate_hz": 10 }] }).to_string())).unwrap();
        let cfg = loop {
            if let tungstenite::Message::Text(t) = ws.read().unwrap() {
                break serde_json::from_str::<Value>(t.as_str()).unwrap();
            }
        };
        assert_eq!(cfg["points"][0]["address"], 0x400, "{cfg}");
        let _ = ws.close(None);
    }

    // --- A start that returns early, then wait_session -------------------------------------------
    let mut mcp = Mcp::open(&http);
    let s3 = mcp.ok("open_session", json!({ "profile": "sim", "name": "third", "wait_s": 0 }));
    assert_eq!(s3["launch"]["phase"], "starting", "{s3}");
    let id3 = s3["session"].as_str().unwrap().to_string();
    let w = mcp.ok("wait_session", json!({ "session": id3, "timeout_s": 60 }));
    assert_eq!(w["phase"], "ready", "{w}");

    // --- A board that does not come up: the error carries the log ---------------------------------
    let (err, e) = mcp.call("open_session", json!({ "connect": { "commands": ["script no/such/board.cfg"], "adapter": "sim" } }));
    assert!(err, "{e}");
    let msg = e.as_str().unwrap_or_default();
    assert!(msg.contains("ended before it was connected") && msg.contains("no/such/board.cfg"), "{msg}");

    // --- Closing ---------------------------------------------------------------------------------------
    for id in [&id1, &id2, &id3] {
        mcp.ok("close_session", json!({ "session": id }));
    }
    let list = http.ok("GET", "/api/v1/sessions", None);
    assert!(list["sessions"].as_array().unwrap().iter().all(|s| s["state"] == "exited"), "{list}");
    for pid in pids {
        wait_until("the worker to end", Duration::from_secs(10), || !hub::spawn::alive(pid as u32));
    }
    running.stop();
    let _ = std::fs::remove_dir_all(&home);
}
