//! The stdio wrapper as an MCP client runs it: JSON-RPC lines in and out, an MCP session that
//! ends with stdin, a hub restart in between, and starting the hub when none answers.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use hub::HubConfig;

struct Wrapper {
    child: Child,
    stdin: Option<ChildStdin>,
    out: Receiver<Value>,
}

impl Wrapper {
    fn start(url: &str, home: &std::path::Path, port: Option<u16>) -> Wrapper {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_aurix-hub"));
        cmd.args(["stdio", "--url", url]).env("AURIX_HUB_HOME", home).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        if let Some(p) = port {
            cmd.env("AURIX_HUB_PORT", p.to_string());
        }
        let mut child = cmd.spawn().unwrap();
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, out) = channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let v: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON on stdout ({e}): {line}"));
                if tx.send(v).is_err() {
                    break;
                }
            }
        });
        Wrapper { child, stdin, out }
    }

    fn send(&mut self, v: Value) {
        let s = self.stdin.as_mut().unwrap();
        writeln!(s, "{v}").unwrap();
        s.flush().unwrap();
    }

    fn reply(&self) -> Value {
        self.out.recv_timeout(Duration::from_secs(30)).expect("an answer")
    }

    fn initialize(&mut self) -> Value {
        self.send(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } } }));
        let r = self.reply();
        self.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        r
    }

    fn finish(mut self) -> i32 {
        drop(self.stdin.take());
        let t0 = Instant::now();
        loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                return s.code().unwrap_or(-1);
            }
            assert!(t0.elapsed() < Duration::from_secs(20), "the wrapper did not end with stdin");
            thread::sleep(Duration::from_millis(50));
        }
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[test]
fn stdio_wrapper() {
    let home = std::env::temp_dir().join(format!("aurix-hub-stdio-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    // SAFETY: set before the hub's threads start; the hub of this test reads its paths from it.
    unsafe { std::env::set_var("AURIX_HUB_HOME", &home) };

    // --- Against a running hub -------------------------------------------------------------------
    let running = hub::start_background(HubConfig { port: 0, ..HubConfig::default() }).unwrap();
    let port = running.port;
    let url = running.url();
    let mut w = Wrapper::start(&url, &home, None);
    let init = w.initialize();
    assert_eq!(init["result"]["serverInfo"]["name"], "aurix-hub", "{init}");
    assert_eq!(running.hub.lock().mcp.len(), 1, "one MCP session");
    w.send(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": { "name": "list_sessions", "arguments": {} } }));
    let r = w.reply();
    assert_eq!(r["id"], 2);
    assert_eq!(r["result"]["structuredContent"]["sessions"], json!([]), "{r}");
    // Requests are answered concurrently, each on its own line.
    w.send(json!({ "jsonrpc": "2.0", "id": 3, "method": "ping" }));
    w.send(json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/list" }));
    let mut ids: Vec<u64> = vec![w.reply()["id"].as_u64().unwrap(), w.reply()["id"].as_u64().unwrap()];
    ids.sort();
    assert_eq!(ids, [3, 4]);

    // The hub restarts: the wrapper initializes again and the call still succeeds.
    running.stop();
    let running = hub::start_background(HubConfig { port, ..HubConfig::default() }).unwrap();
    w.send(json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": { "name": "hub_info", "arguments": {} } }));
    let r = w.reply();
    assert_eq!(r["result"]["structuredContent"]["server"], "aurix-hub", "{r}");
    assert_eq!(running.hub.lock().mcp.len(), 1);

    // stdin closes: the wrapper ends the MCP session and exits.
    assert_eq!(w.finish(), 0);
    let t0 = Instant::now();
    while !running.hub.lock().mcp.is_empty() {
        assert!(t0.elapsed() < Duration::from_secs(5), "the MCP session was not ended");
        thread::sleep(Duration::from_millis(50));
    }
    running.stop();

    // --- No hub: the wrapper starts one (detached, it keeps running) ------------------------------
    let port = free_port();
    let url = format!("http://127.0.0.1:{port}");
    let mut w = Wrapper::start(&url, &home, Some(port));
    let init = w.initialize();
    assert_eq!(init["result"]["serverInfo"]["name"], "aurix-hub", "{init}");
    assert_eq!(w.finish(), 0);
    let (status, health) = hub::http1::json(&url, "GET", "/health", &[], None, Duration::from_secs(5)).unwrap();
    assert_eq!((status, health["role"].as_str()), (200, Some("aurix-hub")), "the started hub outlives the wrapper");
    let pid = health["pid"].as_u64().unwrap() as u32;
    let _ = hub::http1::json(&url, "POST", "/shutdown", &[], Some(&json!({})), Duration::from_secs(5));
    let t0 = Instant::now();
    while hub::spawn::alive(pid) {
        assert!(t0.elapsed() < Duration::from_secs(10), "the started hub did not stop");
        thread::sleep(Duration::from_millis(100));
    }
    let log = std::fs::read_to_string(home.join("local").join("logs").join("hub.log")).unwrap_or_default();
    assert!(log.contains("aurix-hub") && log.contains("stopped"), "{log}");
    let _ = std::fs::remove_dir_all(&home);
}
