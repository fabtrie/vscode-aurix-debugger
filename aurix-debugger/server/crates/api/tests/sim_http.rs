//! End to end against the simulated TC37x: the REST/MCP API and a TCF client (as VS Code) on
//! one session. Actions of one frontend must reach the other, and waiting in the API must not
//! delay TCF requests.
//!
//! One test function: the core events are process-global, parallel servers would see each
//! other's events.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use aurix_core::{Server, ServerHandle};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize().unwrap()
}

fn tcl_path(p: PathBuf) -> String {
    p.to_string_lossy().trim_start_matches(r"\\?\").replace('\\', "/")
}

fn server() -> ServerHandle {
    let (tx, rx) = channel();
    thread::spawn(move || {
        let s = Server::new();
        s.cx().with(|c| {
            c.adapter_override = Some("sim".into());
            c.search_dirs.push(repo().join("third_party/openocd/tcl"));
        });
        s.interp.eval_global("script board/infineon/kit_a2g_tc377tx_3v3_trb.cfg").unwrap();
        s.interp
            .eval_global(&format!("script {}", jim::quote(&tcl_path(repo().join("tc37x_ocds_init.cfg")))))
            .unwrap();
        s.interp.eval_global("set tc3xx_hsm_ready_ms 0").unwrap();
        s.interp.eval_global("init").unwrap();
        tx.send(s.handle()).unwrap();
        s.run();
    });
    rx.recv().unwrap()
}

// --- A minimal HTTP/1.1 client ---------------------------------------------------------------

struct Http(u16);

impl Http {
    fn request(&self, method: &str, path: &str, body: Option<Value>, headers: &[(&str, &str)]) -> (u16, Value) {
        let mut s = TcpStream::connect(("127.0.0.1", self.0)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
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
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        let status: u16 = resp.split(' ').nth(1).unwrap().parse().unwrap();
        let text = resp.split_once("\r\n\r\n").map_or("", |(_, b)| b);
        (status, serde_json::from_str(text).unwrap_or(Value::Null))
    }

    fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (status, v) = self.request(method, path, body, &[]);
        assert_eq!(status, 200, "{method} {path}: {v}");
        v
    }

    /// Server-sent events of `/api/v1/events`, as JSON values.
    fn events(&self) -> Receiver<Value> {
        let mut s = TcpStream::connect(("127.0.0.1", self.0)).unwrap();
        write!(s, "GET /api/v1/events HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n", self.0).unwrap();
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

// --- A minimal TCF client (the VS Code side) -------------------------------------------------

struct Tcf {
    w: TcpStream,
    rx: Receiver<Vec<String>>,
    events: Vec<Vec<String>>,
    token: u32,
}

impl Tcf {
    fn connect(port: u16) -> Tcf {
        let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let r = s.try_clone().unwrap();
        let (tx, rx) = channel();
        thread::spawn(move || {
            let mut r = BufReader::new(r);
            loop {
                let mut buf = Vec::new();
                // Messages end with \x03\x01.
                loop {
                    let mut b = [0u8; 1];
                    if r.read_exact(&mut b).is_err() {
                        return;
                    }
                    buf.push(b[0]);
                    if buf.ends_with(&[3, 1]) {
                        break;
                    }
                }
                let body = &buf[..buf.len() - 2];
                let mut fields: Vec<String> = body.split(|&b| b == 0).map(|f| String::from_utf8_lossy(f).into_owned()).collect();
                if fields.last().is_some_and(|f| f.is_empty()) {
                    fields.pop();
                }
                if tx.send(fields).is_err() {
                    return;
                }
            }
        });
        Tcf { w: s, rx, events: Vec::new(), token: 0 }
    }

    fn command(&mut self, service: &str, command: &str, args: &[Value]) -> Vec<String> {
        self.token += 1;
        let token = self.token.to_string();
        let mut msg = Vec::new();
        for f in ["C", &token, service, command].into_iter().map(str::to_string).chain(args.iter().map(Value::to_string)) {
            msg.extend_from_slice(f.as_bytes());
            msg.push(0);
        }
        msg.extend_from_slice(&[3, 1]);
        self.w.write_all(&msg).unwrap();
        loop {
            let m = self.rx.recv_timeout(Duration::from_secs(10)).expect("TCF reply");
            if m[0] == "R" && m[1] == token {
                return m[2..].to_vec();
            }
            if m[0] == "E" {
                self.events.push(m);
            }
        }
    }

    /// Waits for an event `service.name` whose first argument contains `arg`.
    fn event(&mut self, service: &str, name: &str, arg: &str) -> Vec<String> {
        let matches = |m: &Vec<String>| m.len() > 3 && m[1] == service && m[2] == name && m[3].contains(arg);
        if let Some(i) = self.events.iter().position(matches) {
            return self.events.remove(i);
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let m = self.rx.recv_timeout(left).unwrap_or_else(|_| panic!("no TCF event {service}.{name} {arg}; got {:?}", self.events));
            if matches(&m) {
                return m;
            }
            self.events.push(m);
        }
    }
}

fn core_breakpoints(h: &ServerHandle) -> Vec<u32> {
    h.call(|cx| cx.with(|c| c.targets[0].breakpoints.iter().map(|b| b.addr).collect())).unwrap()
}

#[test]
fn rest_mcp_and_tcf_share_one_session() {
    let h = server();
    let session = tcf::session(h.clone());
    let tcf_port = tcf::listen(&session, "127.0.0.1", 0).unwrap();
    let http = Http(api::start(h.clone(), session.clone(), "127.0.0.1", 0).unwrap());
    let mut vs = Tcf::connect(tcf_port);
    let sse = http.events();
    let c0 = tcf::cores(&h).into_iter().find(|c| c.idx == 0).unwrap().id;
    const CPU0: &str = "tc37x.cpu0";

    // Generated routes and the spec.
    let targets = http.ok("GET", "/api/v1/targets", None);
    assert_eq!(targets["targets"][0]["name"], CPU0);
    let spec = http.ok("GET", "/openapi.json", None);
    assert_eq!(spec["paths"]["/api/v1/memory"]["get"]["operationId"], "read_memory");
    let (status, e) = http.request("GET", "/api/v1/targets/nope", None, &[]);
    assert_eq!((status, e["code"].as_str()), (404, Some("not_found")));

    // REST halt -> VS Code sees the core suspended.
    let st = http.ok("POST", &format!("/api/v1/targets/{CPU0}/halt"), None);
    assert_eq!(st["state"], "halted", "{st}");
    vs.event("RunControl", "contextSuspended", &c0);

    // Registers, and a register write the TCF client is told about.
    let pc = http.ok("GET", &format!("/api/v1/targets/{CPU0}/registers/pc"), None);
    assert_eq!(pc["value"], st["pc"]);
    let d4 = http.ok("PUT", &format!("/api/v1/targets/{CPU0}/registers/d4"), Some(json!({ "value": "0x12345678" })));
    assert_eq!(d4["value"], 0x1234_5678);
    vs.event("Registers", "registerChanged", &format!("{c0}.R.d4"));

    // Memory round trip; the TCF client is told about the write.
    http.ok("PUT", "/api/v1/memory", Some(json!({ "address": "0x70000100", "data": "deadbeef" })));
    let m = http.ok("GET", "/api/v1/memory?address=0x70000100&length=4", None);
    assert_eq!(m["data"], "deadbeef");
    vs.event("Memory", "memoryChanged", "S0.D0");
    let (status, e) = http.request("GET", "/api/v1/memory?address=0x70000000&length=70000", None, &[]);
    assert_eq!((status, e["code"].as_str()), (400, Some("bad_request")));

    // A breakpoint from the API shows up in VS Code; VS Code on the same address shares the
    // core breakpoint, so removing the API's keeps VS Code's armed.
    const BP: u32 = 0x8000_2000;
    let bp = http.ok("POST", "/api/v1/breakpoints", Some(json!({ "address": BP, "targets": [CPU0] })));
    let api_id = bp["id"].as_str().unwrap().to_string();
    assert_eq!(bp["owner"], "api");
    assert_eq!(bp["instances"][0]["target"], CPU0);
    vs.event("Breakpoints", "contextAdded", &api_id);
    vs.command("Breakpoints", "add", &[json!({ "ID": "vs-1", "Enabled": true, "Address": BP, "ContextIds": [c0] })]);
    assert_eq!(core_breakpoints(&h), vec![BP]);
    let list = http.ok("DELETE", &format!("/api/v1/breakpoints/{api_id}"), None);
    assert_eq!(list["breakpoints"].as_array().unwrap().len(), 1);
    assert_eq!(list["breakpoints"][0]["owner"], "client");
    vs.event("Breakpoints", "contextRemoved", &api_id);
    assert_eq!(core_breakpoints(&h), vec![BP]);
    // VS Code replaces its list: an API breakpoint survives.
    let bp2 = http.ok("POST", "/api/v1/breakpoints", Some(json!({ "address": BP + 0x10, "targets": [CPU0] })));
    vs.command("Breakpoints", "set", &[json!([])]);
    let list = http.ok("GET", "/api/v1/breakpoints", None);
    assert_eq!(list["breakpoints"].as_array().unwrap().len(), 1);
    assert_eq!(list["breakpoints"][0]["id"], bp2["id"]);
    assert_eq!(core_breakpoints(&h), vec![BP + 0x10]);
    http.ok("DELETE", &format!("/api/v1/breakpoints/{}", bp2["id"].as_str().unwrap()), None);

    // REST step: VS Code sees the core run and stop again.
    vs.events.clear();
    http.ok("POST", &format!("/api/v1/targets/{CPU0}/step"), None);
    vs.event("RunControl", "contextResumed", &c0);
    vs.event("RunControl", "contextSuspended", &c0);

    // VS Code resumes: the API sees it (wait_state, event stream).
    vs.command("RunControl", "resume", &[json!(c0), json!(0), json!(1), json!({})]);
    let st = http.ok("GET", &format!("/api/v1/targets/{CPU0}/wait?state=running&timeout_ms=3000"), None);
    assert_eq!(st["state"], "running");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let ev = sse.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("resumed event on the SSE stream");
        if ev["type"] == "resumed" && ev["target"] == CPU0 {
            break;
        }
    }

    // A waiting API client does not delay TCF requests.
    let waiter = {
        let port = http.0;
        thread::spawn(move || Http(port).request("GET", &format!("/api/v1/targets/{CPU0}/wait?state=halted&timeout_ms=1500"), None, &[]))
    };
    thread::sleep(Duration::from_millis(100));
    let t0 = Instant::now();
    for _ in 0..10 {
        vs.command("RunControl", "getState", &[json!(c0)]);
    }
    assert!(t0.elapsed() < Duration::from_millis(500), "TCF requests took {:?} while an API client waited", t0.elapsed());
    let (status, e) = waiter.join().unwrap();
    assert_eq!((status, e["code"].as_str()), (504, Some("timeout")));

    // MCP: the tools are the operations.
    let init = http.ok("POST", "/mcp", Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "1" } } })));
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    let (status, _) = http.request("POST", "/mcp", Some(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })), &[]);
    assert_eq!(status, 202);
    let v = [("MCP-Protocol-Version", "2025-06-18")];
    let (_, tools) = http.request("POST", "/mcp", Some(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })), &v);
    let tools = tools["result"]["tools"].as_array().unwrap().clone();
    assert_eq!(tools.len(), api_spec::OPS.len());
    let rm = tools.iter().find(|t| t["name"] == "read_memory").unwrap();
    assert_eq!(rm["inputSchema"]["type"], "object");
    assert!(rm["description"].as_str().unwrap().starts_with("Read memory"));
    assert_eq!(rm["annotations"]["readOnlyHint"], true);
    let (_, r) = http.request(
        "POST",
        "/mcp",
        Some(json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "read_memory", "arguments": { "address": "0x70000100", "length": 4 } } })),
        &v,
    );
    assert_eq!(r["result"]["isError"], false, "{r}");
    assert_eq!(r["result"]["structuredContent"]["data"], "deadbeef");
    let (_, r) = http.request("POST", "/mcp", Some(json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "halt", "arguments": { "target": "nope" } } })), &v);
    assert_eq!(r["result"]["isError"], true);

    // Tcl console.
    let out = http.ok("POST", "/api/v1/tcl", Some(json!({ "command": "targets" })));
    assert!(out["output"].as_str().unwrap().contains(CPU0));

    // Web pages of other sites must not drive the debugger.
    let (status, _) = http.request("POST", &format!("/api/v1/targets/{CPU0}/halt"), None, &[("Origin", "http://evil.example")]);
    assert_eq!(status, 400);
    let (status, _) = http.request("GET", "/api/v1/targets", None, &[("Origin", "http://localhost:8080")]);
    assert_eq!(status, 200);
}
