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
            c.search_dirs.push(PathBuf::from(openocd_tcl::TCL_DIR));
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

// --- A minimal WebSocket client (the DAQ side) -----------------------------------------------

/// Text-frame-only RFC 6455 client. Outgoing frames are masked with an all-zero key, which is
/// legal and makes the XOR a no-op.
struct Ws(TcpStream);

impl Ws {
    fn connect(port: u16, extra: &[(&str, &str)]) -> Result<Ws, u16> {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut req = format!(
            "GET /api/v1/daq HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n"
        );
        for (k, v) in extra {
            req += &format!("{k}: {v}\r\n");
        }
        req += "\r\n";
        s.write_all(req.as_bytes()).unwrap();
        // Read just the status line and headers, byte by byte: the body must stay in the socket.
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            if s.read(&mut b).unwrap_or(0) == 0 {
                break;
            }
            head.push(b[0]);
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        let status: u16 = head.split(' ').nth(1).unwrap_or("0").parse().unwrap_or(0);
        if status != 101 {
            return Err(status);
        }
        Ok(Ws(s))
    }

    fn send(&mut self, v: Value) {
        let payload = v.to_string().into_bytes();
        let mut f = vec![0x81u8];
        match payload.len() {
            n if n < 126 => f.push(0x80 | n as u8),
            n if n < 65536 => {
                f.push(0x80 | 126);
                f.extend((n as u16).to_be_bytes());
            }
            n => {
                f.push(0x80 | 127);
                f.extend((n as u64).to_be_bytes());
            }
        }
        f.extend([0u8; 4]); // masking key: zero, so the payload is unchanged
        f.extend(&payload);
        self.0.write_all(&f).unwrap();
    }

    /// The next text frame, or `None` on timeout or close.
    fn recv(&mut self) -> Option<Value> {
        loop {
            let mut h = [0u8; 2];
            self.0.read_exact(&mut h).ok()?;
            let opcode = h[0] & 0x0f;
            let mut len = (h[1] & 0x7f) as usize;
            if len == 126 {
                let mut e = [0u8; 2];
                self.0.read_exact(&mut e).ok()?;
                len = u16::from_be_bytes(e) as usize;
            } else if len == 127 {
                let mut e = [0u8; 8];
                self.0.read_exact(&mut e).ok()?;
                len = u64::from_be_bytes(e) as usize;
            }
            let mut payload = vec![0u8; len];
            self.0.read_exact(&mut payload).ok()?;
            match opcode {
                0x1 => return serde_json::from_slice(&payload).ok(),
                0x8 => return None,
                _ => continue, // ping/pong
            }
        }
    }

    /// Frames of type `kind` until `n` of them arrived or the socket goes quiet.
    fn collect(&mut self, kind: &str, n: usize) -> Vec<Value> {
        let mut out = Vec::new();
        while out.len() < n {
            match self.recv() {
                Some(v) if v["type"] == kind => out.push(v),
                Some(_) => continue,
                None => break,
            }
        }
        out
    }
}

fn core_breakpoints(h: &ServerHandle) -> Vec<u32> {
    h.call(|cx| cx.with(|c| c.targets[0].breakpoints.iter().map(|b| b.addr).collect())).unwrap()
}

/// Percent-encodes a query parameter value (expressions carry `+`, `[`, `$`, `{` and spaces).
fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// `info` and the variable operations before anything is loaded.
fn variables_without_symbols(http: &Http, h: &ServerHandle) {
    const CPU0: &str = "tc37x_ext.cpu0";
    let info = http.ok("GET", "/api/v1/info", None);
    assert_eq!(info["server"], "aurix-dbg");
    assert_eq!(info["api_version"], api_spec::API_VERSION);
    assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
    // The server runs in this test process.
    assert_eq!(info["pid"], std::process::id());
    assert_eq!((info["adapter"].as_str(), info["connected"].as_bool(), info["examined"].as_bool()), (Some("sim"), Some(true), Some(true)), "{info}");
    let t0 = info["targets"].as_array().unwrap().iter().find(|t| t["name"] == CPU0).unwrap_or_else(|| panic!("{info}"));
    assert_eq!((t0["examined"].as_bool(), t0["reachable"].as_bool()), (Some(true), Some(true)));
    assert!(info["symbol_files"].as_array().unwrap().is_empty(), "{info}");
    assert_eq!(info["symbols_loading"], false);

    // A liveness probe must answer while the core thread is held by something long (flashing, a
    // reset, a Tcl script), and say so: it reads what the core published, not a job queued behind.
    // Held well under a second: after a longer stall the 1 kHz DAQ check below ran at the 15.6 ms
    // Windows timer tick on this machine.
    let busy = {
        let h = h.clone();
        thread::spawn(move || h.call(|_| thread::sleep(Duration::from_millis(600))))
    };
    thread::sleep(Duration::from_millis(200));
    let t = Instant::now();
    let info = http.ok("GET", "/api/v1/info", None);
    assert!(t.elapsed() < Duration::from_millis(150), "info waited for the core thread: {:?}", t.elapsed());
    assert!(info["core_lag_ms"].as_u64().unwrap() >= 150, "{info}");
    busy.join().unwrap();
    let info = http.ok("GET", "/api/v1/info", None);
    assert!(info["core_lag_ms"].as_u64().unwrap() < 150, "{info}");

    // Without symbols every variable operation is a 404 that names the missing call.
    for (method, path, body) in [
        ("GET", "/api/v1/globals".to_string(), None),
        ("GET", "/api/v1/expressions?expression=g_x".to_string(), None),
        ("GET", "/api/v1/expressions/children?expression=g_x".to_string(), None),
        ("PUT", "/api/v1/expressions".to_string(), Some(json!({ "expression": "g_x", "value": 1 }))),
    ] {
        let (status, e) = http.request(method, &path, body, &[]);
        assert_eq!((status, e["code"].as_str()), (404, Some("not_found")), "{method} {path}: {e}");
        assert!(e["message"].as_str().unwrap().contains("load_symbols"), "{e}");
    }
}

/// Variables by name against two symbol files written here: MCS assembler sources, which the
/// symbol store loads without debug information, so their data labels are 32-bit globals. That
/// is enough for names, addresses, values, writes and shadowing; struct and array children need
/// a real ELF (`AURIX_TEST_ELF`).
fn variables(http: &Http, vs: &mut Tcf) {
    let dir = std::env::temp_dir().join(format!("aurix-vars-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (app, boot) = (dir.join("app.asm"), dir.join("boot.asm"));
    std::fs::write(&app, ".org 0x100\ng_counter: .var 0\ng_flags: .var 0\ng_Motor_speed: .var 0\n").unwrap();
    // A second file that defines `g_counter` again: the application's is the one a name finds.
    std::fs::write(&boot, ".org 0x200\ng_counter: .var 0\ng_boot: .var 0\n").unwrap();
    let info = http.ok("POST", "/api/v1/symbols", Some(json!({ "files": [tcl_path(app.clone()), tcl_path(boot.clone())] })));
    assert_eq!(info["targets"][0]["globals"], 5, "{info}");

    // `info` lists the files and the targets they are loaded for.
    let info = http.ok("GET", "/api/v1/info", None);
    let files = info["symbol_files"].as_array().unwrap();
    assert_eq!(files.len(), 2, "{info}");
    assert!(files[0]["path"].as_str().unwrap().ends_with("app.asm"), "{info}");
    assert!(files[0]["targets"].as_array().unwrap().iter().any(|t| t == "tc37x_ext.cpu0"), "{info}");

    // Sorted by name, case-insensitively; each says how to name and sample it.
    let all = http.ok("GET", "/api/v1/globals", None);
    assert_eq!(all["total"], 5, "{all}");
    let items = all["items"].as_array().unwrap();
    let names: Vec<&str> = items.iter().map(|i| i["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["g_boot", "g_counter", "g_counter", "g_flags", "g_Motor_speed"]);
    let counter = &items[1];
    assert_eq!(counter["expression"], "g_counter");
    assert_eq!(counter["address"], 0x100);
    assert_eq!((counter["class"].as_str(), counter["size"].as_u64(), counter["type_name"].as_str()), (Some("uint"), Some(4), Some("unsigned int")));
    assert_eq!((counter["sampleable"].as_bool(), counter["has_children"].as_bool()), (Some(true), Some(false)));
    // The shadowed one gets an expression that still names exactly it.
    let shadowed = items[2]["expression"].as_str().unwrap().to_string();
    assert!(shadowed.starts_with("${E") && shadowed.ends_with("/GV:0}"), "{shadowed}");
    assert_eq!(items[2]["address"], 0x200);

    // Glob search, case-insensitive, and paging.
    let q = http.ok("GET", &format!("/api/v1/globals?query={}", enc("*MOTOR*")), None);
    assert_eq!((q["total"].as_u64(), q["items"][0]["name"].as_str()), (Some(1), Some("g_Motor_speed")), "{q}");
    assert_eq!(http.ok("GET", &format!("/api/v1/globals?query={}", enc("g_?lags")), None)["total"], 1);
    assert_eq!(http.ok("GET", "/api/v1/globals?query=motor", None)["total"], 0, "a pattern matches the whole name");
    let p = http.ok("GET", "/api/v1/globals?offset=3&limit=1", None);
    assert_eq!((p["total"].as_u64(), p["offset"].as_u64(), p["items"].as_array().unwrap().len()), (Some(5), Some(3), 1), "{p}");
    assert_eq!(p["items"][0]["name"], "g_flags");

    // Evaluate: values decoded as DAQ decodes them, with the raw bytes beside.
    http.ok("PUT", "/api/v1/memory", Some(json!({ "address": "0x100", "data": "2a000000" })));
    let v = http.ok("GET", "/api/v1/expressions?expression=g_counter", None);
    assert_eq!((v["value"].as_u64(), v["raw_hex"].as_str(), v["address"].as_u64()), (Some(42), Some("2a000000"), Some(0x100)), "{v}");
    assert_eq!((v["class"].as_str(), v["sampleable"].as_bool()), (Some("uint"), Some(true)));
    let v = http.ok("GET", &format!("/api/v1/expressions?expression={}", enc("g_counter + 1")), None);
    assert_eq!(v["value"], 43, "{v}");
    assert!(v["address"].is_null(), "a computed value has no address: {v}");
    let v = http.ok("GET", &format!("/api/v1/expressions?expression={}", enc(&shadowed)), None);
    assert_eq!(v["address"], 0x200, "{v}");
    let (status, e) = http.request("GET", "/api/v1/expressions?expression=no_such_global", None, &[]);
    assert_eq!((status, e["code"].as_str()), (404, Some("not_found")), "{e}");
    let (status, e) = http.request("GET", &format!("/api/v1/expressions?expression={}", enc("g_counter +")), None, &[]);
    assert_eq!((status, e["code"].as_str()), (400, Some("bad_request")), "{e}");
    let (status, e) = http.request("GET", "/api/v1/expressions?expression=g_counter&target=nope", None, &[]);
    assert_eq!(status, 404, "{e}");
    assert_eq!(http.ok("GET", "/api/v1/expressions?expression=g_counter&target=tc37x_ext.cpu0", None)["value"], 42);

    // Write: numbers, exact 64-bit hex strings, and range checks. VS Code is told to refresh.
    vs.events.clear();
    let w = http.ok("PUT", "/api/v1/expressions", Some(json!({ "expression": "g_flags", "value": "0xdeadbeef" })));
    assert_eq!((w["value"].as_u64(), w["raw_hex"].as_str()), (Some(0xdead_beef), Some("efbeadde")), "{w}");
    assert_eq!(http.ok("GET", "/api/v1/memory?address=0x104&length=4", None)["data"], "efbeadde");
    vs.event("Memory", "memoryChanged", "S0.D0");
    assert_eq!(http.ok("PUT", "/api/v1/expressions", Some(json!({ "expression": "g_counter", "value": 7 })))["value"], 7);
    for bad in [json!(-1), json!(4_294_967_296u64), json!("seven"), json!(1.5), json!({ "a": 1 })] {
        let (status, e) = http.request("PUT", "/api/v1/expressions", Some(json!({ "expression": "g_counter", "value": bad })), &[]);
        assert_eq!((status, e["code"].as_str()), (400, Some("bad_request")), "{bad}: {e}");
    }
    let (status, e) = http.request("PUT", "/api/v1/expressions", Some(json!({ "expression": "g_counter + 1", "value": 1 })), &[]);
    assert_eq!(status, 400, "a computed value cannot be written: {e}");
    assert_eq!(http.ok("GET", "/api/v1/expressions?expression=g_counter", None)["value"], 7, "the failed writes changed nothing");

    // A scalar has no children.
    let c = http.ok("GET", "/api/v1/expressions/children?expression=g_counter", None);
    assert_eq!((c["total"].as_u64(), c["type_name"].as_str()), (Some(0), Some("unsigned int")), "{c}");
    assert!(c["items"].as_array().unwrap().is_empty());

    // MCP: the same operations are tools.
    let v = [("MCP-Protocol-Version", "2025-06-18")];
    let (_, tools) = http.request("POST", "/mcp", Some(json!({ "jsonrpc": "2.0", "id": 10, "method": "tools/list" })), &v);
    let tools = tools["result"]["tools"].as_array().unwrap().clone();
    let tool = |n: &str| tools.iter().find(|t| t["name"] == n).unwrap_or_else(|| panic!("no tool {n}")).clone();
    assert_eq!(tool("evaluate")["annotations"]["readOnlyHint"], true);
    assert_eq!(tool("write_variable")["annotations"]["readOnlyHint"], false);
    assert_eq!(tool("info")["inputSchema"]["type"], "object");
    let call = |id: u32, name: &str, args: Value| {
        let (_, r) = http.request("POST", "/mcp", Some(json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": name, "arguments": args } })), &v);
        assert_eq!(r["result"]["isError"], false, "{r}");
        r["result"]["structuredContent"].clone()
    };
    assert_eq!(call(11, "evaluate", json!({ "expression": "g_counter" }))["value"], 7);
    assert_eq!(call(12, "list_globals", json!({ "query": "g_c*" }))["total"], 2);
    assert_eq!(call(13, "write_variable", json!({ "expression": "g_boot", "value": 5 }))["value"], 5);
    assert_eq!(call(14, "info", json!({}))["server"], "aurix-dbg");

    // A DAQ point on the expression `list_globals` handed out samples the same variable.
    let mut ws = Ws::connect(http.0, &[]).unwrap();
    ws.send(json!({ "op": "subscribe", "id": 1, "points": [{ "expression": shadowed, "rate_hz": 10 }, { "expression": "g_counter", "rate_hz": 10 }] }));
    let cfg = ws.recv().expect("config frame");
    assert_eq!(cfg["points"][0]["address"], 0x200, "{cfg}");
    assert_eq!(cfg["points"][1]["address"], 0x100, "{cfg}");
    drop(ws);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A TriCore ELF of nothing but DWARF declaring `(name, address, is_pair)` variables of type
/// `unsigned int` or `struct pair_s { unsigned int a, b; }`. Written by hand because the symbol
/// store reads variables from DWARF only, and the MCS assembler cannot place data at
/// `0xD000_0000`.
fn dwarf_elf(vars: &[(&str, u32, bool)]) -> Vec<u8> {
    // (code, tag, has children, [(attribute, form)]): compile_unit, base_type, structure_type,
    // member, variable.
    let abbrevs: [(u8, u8, bool, &[(u8, u8)]); 5] = [
        (1, 0x11, true, &[(0x03, 0x08)]),
        (2, 0x24, false, &[(0x03, 0x08), (0x3e, 0x0b), (0x0b, 0x0b)]),
        (3, 0x13, true, &[(0x03, 0x08), (0x0b, 0x0b)]),
        (4, 0x0d, false, &[(0x03, 0x08), (0x49, 0x13), (0x38, 0x0b)]),
        (5, 0x34, false, &[(0x03, 0x08), (0x49, 0x13), (0x3f, 0x19), (0x02, 0x18)]),
    ];
    let mut abbrev = Vec::new();
    for (code, tag, children, attrs) in abbrevs {
        abbrev.extend([code, tag, children as u8]);
        attrs.iter().for_each(|&(a, f)| abbrev.extend([a, f]));
        abbrev.extend([0, 0]);
    }
    abbrev.push(0);

    fn cstr(v: &mut Vec<u8>, s: &str) {
        v.extend(s.as_bytes());
        v.push(0);
    }
    // DWARF 4 unit header: length (patched below), version, abbreviation offset, address size.
    let mut info = vec![0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 4, 1];
    cstr(&mut info, "local.c");
    let uint = info.len() as u32;
    info.push(2);
    cstr(&mut info, "unsigned int");
    info.extend([0x08, 4]); // DW_ATE_unsigned, 4 bytes
    let pair = info.len() as u32;
    info.push(3);
    cstr(&mut info, "pair_s");
    info.push(8);
    for (name, offset) in [("a", 0u8), ("b", 4)] {
        info.push(4);
        cstr(&mut info, name);
        info.extend(uint.to_le_bytes());
        info.push(offset);
    }
    info.push(0);
    for &(name, addr, is_pair) in vars {
        info.push(5);
        cstr(&mut info, name);
        info.extend(if is_pair { pair } else { uint }.to_le_bytes());
        info.extend([5, 0x03]); // a 5-byte location: DW_OP_addr
        info.extend(addr.to_le_bytes());
    }
    info.push(0);
    let len = (info.len() - 4) as u32;
    info[..4].copy_from_slice(&len.to_le_bytes());

    // ELF32 little-endian, EM_TRICORE: header, section contents, section headers.
    let shstrtab: &[u8] = b"\0.debug_abbrev\0.debug_info\0.shstrtab\0";
    let mut elf = vec![0u8; 52];
    let mut sections = Vec::new();
    for (name, ty, data) in [(1u32, 1u32, &abbrev[..]), (15, 1, &info[..]), (27, 3, shstrtab)] {
        sections.push([name, ty, 0, 0, elf.len() as u32, data.len() as u32, 0, 0, 1, 0]);
        elf.extend(data);
    }
    elf.resize(elf.len().next_multiple_of(4), 0);
    let shoff = elf.len() as u32;
    elf.extend([0u8; 40]);
    for sh in sections {
        sh.iter().for_each(|w| elf.extend(w.to_le_bytes()));
    }
    elf[..16].copy_from_slice(&[0x7f, b'E', b'L', b'F', 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let half = |elf: &mut Vec<u8>, at: usize, v: u16| elf[at..at + 2].copy_from_slice(&v.to_le_bytes());
    half(&mut elf, 16, 2); // ET_EXEC
    half(&mut elf, 18, 44); // EM_TRICORE
    elf[20..24].copy_from_slice(&1u32.to_le_bytes());
    elf[32..36].copy_from_slice(&shoff.to_le_bytes());
    half(&mut elf, 40, 52);
    half(&mut elf, 42, 32);
    half(&mut elf, 46, 40);
    half(&mut elf, 48, 4);
    half(&mut elf, 50, 3);
    elf
}

/// Variables in core-local RAM (`0xD000_0000`), which exists once per TriCore core: a name means
/// the copy of the core `@N` names or that the file is assigned to, never a guess.
fn core_local_variables(http: &Http, vs: &mut Tcf) {
    let dir = std::env::temp_dir().join(format!("aurix-local-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let vars = [("CRASH_state", 0xD000_0100, false), ("g_pair", 0xD000_0200, true), ("g_shared", 0x9000_0300, false)];
    let (elf, copy) = (dir.join("local.elf"), dir.join("copy.elf"));
    std::fs::write(&elf, dwarf_elf(&vars)).unwrap();
    // The same variables again, to shadow the first file's: they are named by `${E<n>/GV:<i>}`.
    std::fs::write(&copy, dwarf_elf(&vars)).unwrap();
    let elf = tcl_path(elf);
    let get = |path: &str| http.ok("GET", path, None);
    let eval = |e: &str| http.request("GET", &format!("/api/v1/expressions?expression={}", enc(e)), None, &[]);
    let children = |e: &str| get(&format!("/api/v1/expressions/children?expression={}", enc(e)));
    let globals = || {
        let l = get("/api/v1/globals");
        l["items"].as_array().unwrap().clone()
    };
    let named = |items: &[Value], n: &str| items.iter().find(|i| i["name"] == n).unwrap_or_else(|| panic!("no {n} in {items:?}")).clone();
    // Each core's copy in its own DSPR, as the system bus sees it.
    for (dspr, v) in [(0x7000_0100u32, "01000000"), (0x6000_0100, "02000000"), (0x5000_0100, "03000000")] {
        http.ok("PUT", "/api/v1/memory", Some(json!({ "address": format!("0x{dspr:08x}"), "data": v })));
    }
    http.ok("PUT", "/api/v1/memory", Some(json!({ "address": "0x50000204", "data": "07000000" })));

    // --- Assigned to exactly one core: plain names mean its copy ---
    let (status, e) = http.request("POST", "/api/v1/symbols", Some(json!({ "files": [format!("{elf}@7")] })), &[]);
    assert_eq!((status, e["code"].as_str()), (400, Some("bad_request")), "a core the chip does not have: {e}");
    http.ok("POST", "/api/v1/symbols", Some(json!({ "files": [format!("{elf}@1")] })));
    let info = get("/api/v1/info");
    assert_eq!(info["symbol_files"][0]["local_cores"], json!([1]), "{info}");
    let items = globals();
    let crash = named(&items, "CRASH_state");
    assert_eq!((crash["expression"].as_str(), crash["address"].as_u64()), (Some("CRASH_state"), Some(0x6000_0100)), "{crash}");
    assert_eq!((crash["sampleable"].as_bool(), crash["has_children"].as_bool()), (Some(true), Some(false)), "{crash}");
    assert!(crash.get("per_core").is_none(), "{crash}");
    assert_eq!(named(&items, "g_pair")["address"], 0x6000_0200);
    // Memory that is not core-local is not touched.
    assert_eq!(named(&items, "g_shared")["address"], 0x9000_0300u32);
    let (status, v) = eval("CRASH_state");
    assert_eq!((status, v["value"].as_u64(), v["address"].as_u64()), (200, Some(2), Some(0x6000_0100)), "{v}");
    // An `@N` beats the assignment.
    let (_, v) = eval("CRASH_state@2");
    assert_eq!((v["value"].as_u64(), v["address"].as_u64()), (Some(3), Some(0x5000_0100)), "{v}");
    let (_, v) = eval("g_shared@1");
    assert_eq!(v["address"], 0x9000_0300u32, "a suffix on other memory is harmless: {v}");
    let c = children("g_pair");
    let m: Vec<(&str, u64)> = c["items"].as_array().unwrap().iter().map(|i| (i["expression"].as_str().unwrap(), i["address"].as_u64().unwrap())).collect();
    assert_eq!(m, [("g_pair.a", 0x6000_0200), ("g_pair.b", 0x6000_0204)], "{c}");
    let w = http.ok("PUT", "/api/v1/expressions", Some(json!({ "expression": "CRASH_state", "value": 5 })));
    assert_eq!(w["address"], 0x6000_0100, "{w}");
    assert_eq!(get("/api/v1/memory?address=0x60000100&length=4")["data"], "05000000");
    http.ok("PUT", "/api/v1/memory", Some(json!({ "address": "0x60000100", "data": "02000000" })));
    // DAQ reads the translated address over the system bus, in the shared batch.
    let mut ws = Ws::connect(http.0, &[]).unwrap();
    ws.send(json!({ "op": "subscribe", "id": 1, "points": [{ "expression": "CRASH_state", "rate_hz": 100 }, { "expression": "g_pair.b@2", "rate_hz": 100 }] }));
    let cfg = ws.recv().expect("config frame");
    assert_eq!(cfg["points"][0]["address"], 0x6000_0100, "{cfg}");
    assert_eq!(cfg["points"][1]["address"], 0x5000_0204, "{cfg}");
    let s = ws.collect("samples", 1);
    let value = |id: &Value| s[0]["points"].as_array().unwrap().iter().position(|p| p == id).map(|i| s[0]["values"][i].clone());
    assert_eq!((value(&cfg["points"][0]["id"]), value(&cfg["points"][1]["id"])), (Some(json!(2)), Some(json!(7))), "{s:?}");
    let out = http.ok("POST", "/api/v1/tcl", Some(json!({ "command": "daq" })));
    let out = out["output"].as_str().unwrap();
    for a in ["0x60000100", "0x50000204"] {
        let line = out.lines().find(|l| l.contains(a)).unwrap_or_else(|| panic!("{a} not sampled: {out}"));
        assert!(line.contains("via bus"), "{line}");
    }
    drop(ws);

    // --- Assigned to several cores: a group, and a bare name is refused ---
    let copy = tcl_path(copy);
    http.ok("POST", "/api/v1/symbols", Some(json!({ "files": [format!("{elf}@0,2"), format!("{copy} @ 0, 2")] })));
    let items = globals();
    let crash = named(&items, "CRASH_state");
    assert_eq!(crash["expression"], "CRASH_state");
    assert_eq!((crash["per_core"].as_bool(), crash["has_children"].as_bool(), crash["sampleable"].as_bool()), (Some(true), Some(true), Some(false)), "{crash}");
    assert!(crash["address"].is_null(), "{crash}");
    assert_eq!((crash["class"].as_str(), crash["type_name"].as_str(), crash["size"].as_u64()), (Some("uint"), Some("unsigned int"), Some(4)));
    assert_eq!(named(&items, "g_shared")["address"], 0x9000_0300u32, "not core-local, not a group");
    assert!(named(&items, "g_shared").get("per_core").is_none());
    let c = children("CRASH_state");
    let cores: Vec<(&str, &str, u64, bool)> = c["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| (i["name"].as_str().unwrap(), i["expression"].as_str().unwrap(), i["address"].as_u64().unwrap(), i["sampleable"] == true))
        .collect();
    assert_eq!(cores, [("core 0", "CRASH_state@0", 0x7000_0100, true), ("core 2", "CRASH_state@2", 0x5000_0100, true)], "{c}");
    assert_eq!((c["total"].as_u64(), c["type_name"].as_str()), (Some(2), Some("unsigned int")), "{c}");
    let (status, e) = eval("CRASH_state");
    assert_eq!((status, e["code"].as_str()), (400, Some("bad_request")), "{e}");
    let msg = e["message"].as_str().unwrap();
    for want in ["0xD0000100", "local.elf", "cores 0 and 2", "`CRASH_state@0` or `CRASH_state@2`"] {
        assert!(msg.contains(want), "{want} missing: {msg}");
    }
    let (status, e) = eval("CRASH_state + 1");
    assert_eq!(status, 400, "a computed value reads the RAM too: {e}");
    assert!(e["message"].as_str().unwrap().contains("reads core-local RAM"), "{e}");
    assert_eq!(eval("CRASH_state@2").1["value"], 3);
    // A struct copy's members carry its core; the group's children are the copies.
    let c = children("g_pair");
    assert_eq!(c["items"][1]["expression"], "g_pair@2", "{c}");
    assert_eq!(c["items"][1]["has_children"], true, "{c}");
    let c = children("g_pair@2");
    let m: Vec<(&str, u64)> = c["items"].as_array().unwrap().iter().map(|i| (i["expression"].as_str().unwrap(), i["address"].as_u64().unwrap())).collect();
    assert_eq!(m, [("g_pair.a@2", 0x5000_0200), ("g_pair.b@2", 0x5000_0204)], "{c}");
    assert_eq!(eval("g_pair.b@2").1["value"], 7);
    // The shadowed copy's symbol reference takes a core as well.
    let shadowed = items.iter().filter(|i| i["name"] == "CRASH_state").nth(1).unwrap()["expression"].as_str().unwrap().to_string();
    assert!(shadowed.starts_with("${E") && shadowed.ends_with("/GV:0}"), "{shadowed}");
    let c = children(&shadowed);
    assert_eq!(c["items"][0]["expression"], format!("{shadowed}@0"), "{c}");
    let (_, v) = eval(&format!("{shadowed}@2"));
    assert_eq!((v["value"].as_u64(), v["address"].as_u64()), (Some(3), Some(0x5000_0100)), "{v}");
    let (status, e) = eval(&shadowed);
    assert_eq!(status, 400, "{e}");
    // Writes: refused bare, the copy named otherwise.
    let (status, e) = http.request("PUT", "/api/v1/expressions", Some(json!({ "expression": "CRASH_state", "value": 9 })), &[]);
    assert_eq!((status, e["code"].as_str()), (400, Some("bad_request")), "{e}");
    assert_eq!(get("/api/v1/memory?address=0x70000100&length=4")["data"], "01000000", "the refused write changed nothing");
    vs.events.clear();
    let w = http.ok("PUT", "/api/v1/expressions", Some(json!({ "expression": "CRASH_state@0", "value": 9 })));
    assert_eq!((w["value"].as_u64(), w["address"].as_u64()), (Some(9), Some(0x7000_0100)), "{w}");
    assert_eq!(get("/api/v1/memory?address=0x70000100&length=4")["data"], "09000000");
    vs.event("Memory", "memoryChanged", "S0.D0.C0");
    // `@N` must name a TriCore core that exists, and agree with `target`.
    let (status, e) = eval("CRASH_state@5");
    assert_eq!((status, e["code"].as_str()), (404, Some("not_found")), "{e}");
    assert!(e["message"].as_str().unwrap().contains("no TriCore core 5"), "{e}");
    let (status, e) = eval("CRASH_state@6");
    assert_eq!(status, 404, "core 6 is the HSM, not a TriCore: {e}");
    let (status, e) = http.request("GET", &format!("/api/v1/expressions?expression={}&target=tc37x_ext.cpu0", enc("CRASH_state@2")), None, &[]);
    assert_eq!((status, e["code"].as_str()), (400, Some("bad_request")), "{e}");
    assert_eq!(get(&format!("/api/v1/expressions?expression={}&target=tc37x_ext.cpu2", enc("CRASH_state@2")))["value"], 3);
    // DAQ: the bare name is refused per point, a copy samples over the system bus.
    let mut ws = Ws::connect(http.0, &[]).unwrap();
    ws.send(json!({ "op": "subscribe", "id": 1, "points": [{ "expression": "CRASH_state", "rate_hz": 100 }, { "expression": "CRASH_state@2", "rate_hz": 100 }] }));
    let cfg = ws.recv().expect("config frame");
    let pts = cfg["points"].as_array().unwrap();
    assert!(pts[0]["error"].as_str().is_some_and(|m| m.contains("`CRASH_state@0` or `CRASH_state@2`")), "{cfg}");
    assert!(pts[0]["id"].is_null(), "{cfg}");
    assert_eq!(pts[1]["address"], 0x5000_0100, "{cfg}");
    assert_eq!(ws.collect("samples", 1)[0]["values"][0], 3);
    drop(ws);
    // A live watch has a scope, so it keeps reading the copy of the core it is evaluated for.
    let r = vs.command("Expressions", "create", &[json!("S0.D0.C2"), Value::Null, json!("CRASH_state")]);
    assert_eq!(r[0], "", "{r:?}");
    let id = serde_json::from_str::<Value>(&r[1]).unwrap()["ID"].as_str().unwrap().to_string();
    let r = vs.command("Expressions", "evaluate", &[json!(id)]);
    assert_eq!(r[0], json!("AwAAAA==").to_string(), "core 2's copy: {r:?}");

    // --- Assigned to no core: the copies are every TriCore core's, not the HSM's ---
    http.ok("POST", "/api/v1/symbols", Some(json!({ "files": [elf] })));
    assert!(get("/api/v1/info")["symbol_files"][0].get("local_cores").is_none(), "loading without a suffix clears it");
    assert_eq!(named(&globals(), "CRASH_state")["per_core"], true);
    let c = children("CRASH_state");
    let exprs: Vec<&str> = c["items"].as_array().unwrap().iter().map(|i| i["expression"].as_str().unwrap()).collect();
    assert_eq!(exprs, ["CRASH_state@0", "CRASH_state@1", "CRASH_state@2"], "{c}");
    let (status, e) = eval("CRASH_state");
    assert_eq!(status, 400, "{e}");
    let msg = e["message"].as_str().unwrap();
    assert!(msg.contains("`CRASH_state@0`, `CRASH_state@1` or `CRASH_state@2`") && msg.contains("not assigned to a single core"), "{msg}");
    assert_eq!(eval("CRASH_state@1").1["value"], 2);
    let mut ws = Ws::connect(http.0, &[]).unwrap();
    ws.send(json!({ "op": "subscribe", "id": 1, "points": [{ "expression": "CRASH_state", "rate_hz": 100 }] }));
    let cfg = ws.recv().expect("config frame");
    assert!(cfg["points"][0]["error"].as_str().is_some_and(|m| m.contains("CRASH_state@1")), "{cfg}");
    drop(ws);

    let _ = std::fs::remove_dir_all(&dir);
}

/// Struct members and array elements, against a real application ELF (`AURIX_TEST_ELF`).
fn variables_of_real_elf(http: &Http) {
    let Some(elf) = std::env::var_os("AURIX_TEST_ELF").map(PathBuf::from).filter(|p| p.is_file()) else {
        eprintln!("AURIX_TEST_ELF not set, skipping the struct and array children");
        return;
    };
    http.ok("POST", "/api/v1/symbols", Some(json!({ "files": [tcl_path(elf)] })));
    let t = Instant::now();
    let all = http.ok("GET", "/api/v1/globals?limit=10000", None);
    eprintln!("list_globals: {} globals, first page in {:?}", all["total"], t.elapsed());
    let t = Instant::now();
    let q = http.ok("GET", &format!("/api/v1/globals?query={}", enc("*a*")), None);
    eprintln!("list_globals *a*: {} matches in {:?}", q["total"], t.elapsed());
    assert!(t.elapsed() < Duration::from_secs(2), "a search must stay interactive");
    let items = all["items"].as_array().unwrap();
    assert!(!items.is_empty());
    // Sorted by name, ignoring case.
    let names: Vec<String> = items.iter().map(|i| i["name"].as_str().unwrap().to_ascii_lowercase()).collect();
    assert!(names.windows(2).all(|w| w[0] <= w[1]), "not sorted");
    // Every expression handed out evaluates to the variable it describes.
    let readable = |v: &Value| v["size"].as_u64().is_some_and(|n| n <= 65536);
    for g in items.iter().filter(|g| readable(g)).take(200) {
        let v = http.ok("GET", &format!("/api/v1/expressions?expression={}", enc(g["expression"].as_str().unwrap())), None);
        assert_eq!(v["address"], g["address"], "{g} -> {v}");
        assert_eq!(v["type_name"], g["type_name"], "{g} -> {v}");
    }

    // A struct: its members, each naming itself and lying inside the parent.
    let s = items.iter().find(|i| i["class"] == "struct" && i["has_children"] == true).expect("a struct global");
    let c = http.ok("GET", &format!("/api/v1/expressions/children?expression={}", enc(s["expression"].as_str().unwrap())), None);
    let members = c["items"].as_array().unwrap();
    assert!(!members.is_empty() && c["total"].as_u64() >= Some(members.len() as u64), "{c}");
    let (base, size) = (s["address"].as_u64().unwrap(), s["size"].as_u64().unwrap());
    for m in members {
        let a = m["address"].as_u64().unwrap();
        assert!(a >= base && a < base + size.max(1), "{m} outside {s}");
        assert!(m["expression"].as_str().unwrap().contains(&format!(".{}", m["name"].as_str().unwrap())), "{m}");
        if !readable(m) {
            continue;
        }
        let v = http.ok("GET", &format!("/api/v1/expressions?expression={}", enc(m["expression"].as_str().unwrap())), None);
        assert_eq!(v["address"], m["address"], "{m} -> {v}");
    }

    // An array: elements paged by offset and limit, one element size apart.
    if let Some(arr) = items.iter().find(|i| i["class"] == "array" && i["has_children"] == true && i["size"].as_u64() > Some(8)) {
        let e = enc(arr["expression"].as_str().unwrap());
        let all = http.ok("GET", &format!("/api/v1/expressions/children?expression={e}&limit=10000"), None);
        let n = all["total"].as_u64().unwrap();
        if n >= 3 {
            let c = http.ok("GET", &format!("/api/v1/expressions/children?expression={e}&offset=1&limit=2"), None);
            let el = c["items"].as_array().unwrap();
            assert_eq!((el.len(), c["total"].as_u64()), (2, Some(n)), "{c}");
            assert_eq!(el[0]["name"], "[1]");
            let step = el[0]["size"].as_u64().unwrap();
            assert_eq!(el[1]["address"].as_u64().unwrap() - el[0]["address"].as_u64().unwrap(), step, "{c}");
            assert_eq!(el[0]["address"].as_u64().unwrap(), arr["address"].as_u64().unwrap() + step, "{c}");
        }
    }

    // Writing an integer in RAM (not a constant in flash) reads back what was written.
    let in_ram = |v: &Value| v["address"].as_u64().is_some_and(|a| matches!(a >> 28, 0x5 | 0x6 | 0x7 | 0x9 | 0xB));
    let scalar = items.iter().find(|g| g["sampleable"] == true && matches!(g["class"].as_str(), Some("uint" | "int")) && in_ram(g));
    if let Some(g) = scalar {
        let w = http.ok("PUT", "/api/v1/expressions", Some(json!({ "expression": g["expression"], "value": 1 })));
        assert_eq!(w["value"], 1, "{w}");
    }
}

#[test]
fn rest_mcp_and_tcf_share_one_session() {
    let h = server();
    let session = tcf::session(h.clone());
    let tcf_port = tcf::listen(&session, "127.0.0.1", 0).unwrap();
    let http = Http(api::start(h.clone(), session.clone(), "127.0.0.1", 0).unwrap());
    let mut vs = Tcf::connect(tcf_port);
    // A TCF client will not use a service the server does not advertise, so every service the
    // extension needs has to be in the Hello list - not merely implemented in the dispatcher.
    let hello = vs.event("Locator", "Hello", "Locator");
    let advertised: Vec<String> = serde_json::from_str(&hello[3]).unwrap();
    for want in ["Locator", "RunControl", "Memory", "Registers", "Breakpoints", "Expressions", "AurixOS"] {
        assert!(advertised.iter().any(|s| s == want), "{want} is not advertised: {advertised:?}");
    }
    let sse = http.events();
    let c0 = tcf::cores(&h).into_iter().find(|c| c.idx == 0).unwrap().id;
    const CPU0: &str = "tc37x_ext.cpu0";

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

    // Symbols can be loaded from the API, without a VS Code debug session.
    let (status, e) = http.request("POST", "/api/v1/symbols", Some(json!({ "files": ["/no/such/file.elf"] })), &[]);
    assert_eq!(status, 404, "{e}");
    assert!(e["message"].as_str().unwrap().contains("no such file"), "{e}");
    assert_eq!(http.ok("GET", "/api/v1/symbols", None)["targets"].as_array().unwrap().len(), 0);
    variables_without_symbols(&http, &h);

    // --- DAQ over the WebSocket ---------------------------------------------------------------

    // The upgrade goes through the same origin check as everything else. This matters more here
    // than for REST: browsers exempt WebSocket from CORS, so this is the only defence.
    assert_eq!(Ws::connect(http.0, &[("Origin", "http://evil.example")]).err(), Some(400));

    let mut ws = Ws::connect(http.0, &[]).unwrap();
    http.ok("PUT", "/api/v1/memory", Some(json!({ "address": "0x70000100", "data": "44332211" })));
    http.ok("PUT", "/api/v1/memory", Some(json!({ "address": "0x70000200", "data": "aabbccdd" })));
    ws.send(json!({
        "op": "subscribe", "id": 1,
        "points": [
            { "address": "0x70000100", "size": 4, "rate_hz": 1000, "name": "fast" },
            { "address": "0x70000200", "size": 4, "interval_us": 5000, "name": "slow" },
            { "expression": "no_such_global", "rate_hz": 10 },
        ]
    }));
    let cfg = ws.recv().expect("config frame");
    assert_eq!(cfg["type"], "config", "{cfg}");
    let pts = cfg["points"].as_array().unwrap();
    assert_eq!(pts.len(), 3);
    assert_eq!(pts[0]["name"], "fast");
    assert_eq!(pts[0]["address"], 0x7000_0100u32);
    assert_eq!(pts[0]["interval_us"], 1000);
    assert_eq!(pts[1]["interval_us"], 5000);
    assert_eq!(pts[0]["mode"], "every_tick");
    // A point that does not resolve is reported and skipped; the others still run.
    assert!(pts[2]["error"].as_str().unwrap().contains("no symbols loaded"), "{cfg}");
    assert_eq!(pts[2]["interval_us"], 0);
    assert!(pts[2]["id"].is_null());
    let (fast, slow) = (pts[0]["id"].as_u64().unwrap(), pts[1]["id"].as_u64().unwrap());

    let batches = ws.collect("samples", 20);
    assert_eq!(batches.len(), 20, "the stream runs");
    let mut t_prev = 0u64;
    let (mut n_fast, mut n_slow) = (0, 0);
    for b in &batches {
        let t = b["t_us"].as_u64().unwrap();
        assert!(t >= t_prev, "t_us goes forward: {t} after {t_prev}");
        t_prev = t;
        for (i, p) in b["points"].as_array().unwrap().iter().enumerate() {
            let v = &b["values"][i];
            if p.as_u64() == Some(fast) {
                n_fast += 1;
                // Raw points stream hex, little-endian as read.
                assert_eq!(v, "44332211");
            } else if p.as_u64() == Some(slow) {
                n_slow += 1;
                assert_eq!(v, "aabbccdd");
            }
        }
    }
    assert!(n_fast >= 3 * n_slow.max(1), "1 kHz vs 200 Hz: {n_fast} fast, {n_slow} slow");

    // Retiming works on the open socket, without re-subscribing.
    ws.send(json!({ "op": "set_rate", "id": 2, "point": fast, "rate_hz": 100 }));
    let ch = ws.collect("changed", 1);
    assert_eq!(ch[0]["interval_us"], 10000, "{:?}", ch[0]);
    ws.send(json!({ "op": "set_mode", "id": 3, "point": slow, "mode": "on_change" }));
    assert_eq!(ws.collect("changed", 1)[0]["mode"], "on_change");

    // A second session on the same socket is refused rather than silently replacing the first.
    ws.send(json!({ "op": "subscribe", "id": 4, "points": [] }));
    let e = ws.collect("error", 1);
    assert_eq!(e[0]["code"], "busy", "{:?}", e[0]);

    // `pacing` is gone. A script that still sets it is told so, rather than quietly getting the
    // other behaviour: the removed `exact` mode busy-waited the core thread.
    ws.send(json!({ "op": "add", "id": 5, "points": [{ "address": "0x70000200", "size": 4, "pacing": "exact" }] }));
    let e = ws.collect("error", 1);
    assert_eq!(e[0]["code"], "bad_request", "{:?}", e[0]);
    assert!(e[0]["message"].as_str().is_some_and(|m| m.contains("pacing")), "{:?}", e[0]);

    // The console can see what is sampling, without an API operation (and so without an MCP tool).
    let out = http.ok("POST", "/api/v1/tcl", Some(json!({ "command": "daq" })));
    let out = out["output"].as_str().unwrap();
    assert!(out.contains("session 1: 2 point(s)"), "{out}");
    assert!(out.contains("0x70000100"), "{out}");
    assert!(out.contains("on_change"), "{out}");
    let (status, e) = http.request("POST", "/api/v1/tcl", Some(json!({ "command": "daq open" })), &[]);
    assert_eq!(status, 500, "the console must not be able to open a session: {e}");

    // The session must die with the socket: this is what keeps a crashed client from sampling
    // the target forever, and it is the whole reason configuration lives on the WebSocket.
    assert_eq!(h.call(|cx| cx.with(|c| c.daq.is_empty())), Some(false));
    drop(ws);
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(5) && h.call(|cx| cx.with(|c| c.daq.is_empty())) != Some(true) {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(h.call(|cx| cx.with(|c| c.daq.is_empty())), Some(true), "the DAQ session outlived its socket");

    // --- AUTOSAR OS awareness (ORTI) ---------------------------------------------------------

    // Nothing loaded: an empty description rather than an error, so a panel can ask on startup.
    let os = http.ok("GET", "/api/v1/os", None);
    assert_eq!(os["cores"], 0);
    assert!(os["file"].is_null(), "{os}");
    assert!(os["types"].as_array().unwrap().is_empty(), "{os}");

    // Reading objects before a file is loaded says which call is missing.
    let (status, e) = http.request("GET", "/api/v1/os/objects/TASK", None, &[]);
    assert_eq!(status, 404, "{e}");
    assert!(e["message"].as_str().is_some_and(|m| m.contains("load_orti")), "{e}");

    // A file that is not there is a 404, not a parse error.
    let (status, e) = http.request("POST", "/api/v1/os/orti", Some(json!({ "file": "no/such.ORT" })), &[]);
    assert_eq!(status, 404, "{e}");

    // Attribute expressions need symbols, so that failure has to name `load_symbols`.
    // Optional real files; without them only the error cases above run.
    let ort = PathBuf::from(std::env::var_os("AURIX_TEST_ORTI").unwrap_or_default());
    let elf = PathBuf::from(std::env::var_os("AURIX_TEST_ELF").unwrap_or_default());
    if ort.is_file() {
        let (status, e) = http.request("POST", "/api/v1/os/orti", Some(json!({ "file": tcl_path(ort.clone()) })), &[]);
        assert_eq!(status, 400, "{e}");
        assert!(e["message"].as_str().is_some_and(|m| m.contains("load_symbols")), "{e}");
    }

    if ort.is_file() && elf.is_file() {
        // What the file declares, to check the server's answers against.
        let parsed = orti::parse(&std::fs::read_to_string(&ort).unwrap()).unwrap();
        let cores = parsed.core_count() as usize;
        let first_task = parsed.objects_of("TASK").next().unwrap();
        let (task_name, task_attrs) = (first_task.name.clone(), first_task.attrs.len());
        let task_count = parsed.objects_of("TASK").count();
        let stack_count = parsed.objects_of("STACK").count();
        let stack_name = parsed.objects_of("STACK").next().unwrap().name.clone();
        http.ok("POST", "/api/v1/symbols", Some(json!({ "files": [tcl_path(elf)] })));
        let os = http.ok("POST", "/api/v1/os/orti", Some(json!({ "file": tcl_path(ort) })));
        assert_eq!(os["implementation"], "Vector_ORTI");
        assert_eq!(os["version"], "2.3");
        assert_eq!(os["cores"], cores);
        assert_eq!(os["target"], CPU0);

        // The object types are whatever the generator declared, Vector's own ones included.
        let types: Vec<&str> = os["types"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(types.contains(&"TASK") && types.contains(&"STACK"), "{types:?}");
        let task = os["types"].as_array().unwrap().iter().find(|t| t["name"] == "TASK").unwrap();
        assert_eq!(task["count"], task_count);
        let attrs = task["attributes"].as_array().unwrap();
        let state = attrs.iter().find(|a| a["name"] == "STATE").unwrap();
        assert_eq!((state["kind"].as_str(), state["live"].as_bool()), (Some("enum"), Some(true)));
        // `STACK` is `&OsCfg_Stack_...`: a constant, so it is never re-read.
        let stack = attrs.iter().find(|a| a["name"] == "STACK").unwrap();
        assert_eq!(stack["live"], false);
        // Prose stays prose.
        let sched = attrs.iter().find(|a| a["name"] == "vs_Schedule").unwrap();
        assert_eq!(sched["kind"], "string");
        // The SMP extension shows up as per-core attributes on the OS object.
        let os_t = os["types"].as_array().unwrap().iter().find(|t| t["name"] == "OS").unwrap();
        let running = os_t["attributes"].as_array().unwrap().iter().find(|a| a["name"] == "RUNNINGTASK").unwrap();
        assert_eq!(running["per_core"], true);

        // The description survives a reload of the same session.
        let again = http.ok("GET", "/api/v1/os", None);
        assert_eq!(again["cores"], cores);

        // Reading objects. The simulator has no real OS in memory, so the values are whatever
        // the sim holds - what matters is that every attribute resolves and is reported.
        let list = http.ok("GET", &format!("/api/v1/os/objects/TASK?name={task_name}"), None);
        let objs = list["objects"].as_array().unwrap();
        assert_eq!(objs.len(), 1, "{list}");
        let attrs = objs[0]["attributes"].as_array().unwrap();
        assert_eq!(attrs.len(), task_attrs, "{:?}", attrs);
        let by = |n: &str| attrs.iter().find(|a| a["name"] == n).unwrap();
        // The stack is a constant address, and its enumerator names the stack object, so the
        // cell can link to it.
        assert!(by("STACK")["link"].is_string(), "{:?}", by("STACK"));
        assert_eq!(by("STACK")["value"], by("STACK")["link"]);
        assert!(by("vs_Schedule")["value"].is_string(), "{:?}", by("vs_Schedule"));
        assert!(by("PRIORITY")["error"].is_null(), "{:?}", by("PRIORITY"));
        assert!(list["reads"].as_u64().unwrap() > 0);

        // `live_only` drops the attributes that cannot change, which is most of a refresh.
        let live = http.ok("GET", &format!("/api/v1/os/objects/TASK?name={task_name}&live_only=true"), None);
        let n = live["objects"][0]["attributes"].as_array().unwrap().len();
        assert!(n > 0 && n < task_attrs, "{n} of {task_attrs} attributes are live");

        // An OS object type that the file declares but never instantiates is empty, not an error.
        if let Some(d) = parsed.decls.iter().find(|d| parsed.objects_of(&d.name).next().is_none()) {
            let empty = http.ok("GET", &format!("/api/v1/os/objects/{}", d.name), None);
            assert!(empty["objects"].as_array().unwrap().is_empty(), "{empty}");
            assert_eq!(empty["reads"], 0);
        }

        // A type the file never declared lists the ones it did.
        let (status, e) = http.request("GET", "/api/v1/os/objects/NOPE", None, &[]);
        assert_eq!(status, 404, "{e}");
        assert!(e["message"].as_str().is_some_and(|m| m.contains("TASK")), "{e}");

        // The whole OS object, all cores at once: this is what the sidebar view reads.
        let os_objs = http.ok("GET", "/api/v1/os/objects/OS", None);
        let attrs = os_objs["objects"][0]["attributes"].as_array().unwrap();
        let running: Vec<&Value> = attrs.iter().filter(|a| a["name"] == "RUNNINGTASK").collect();
        assert_eq!(running.len(), cores, "one RUNNINGTASK per core: {running:?}");
        for (core, r) in running.iter().enumerate() {
            assert_eq!(r["core"], core);
        }

        // ...and the same awareness over TCF, which is the channel the VS Code extension has.
        // One session: the file the REST call loaded is already there.
        let r = vs.command("AurixOS", "getInfo", &[]);
        assert_eq!(r[0], "", "{r:?}");
        let info: Value = serde_json::from_str(&r[1]).unwrap();
        assert_eq!(info["Loaded"], true);
        assert_eq!(info["Implementation"], "Vector_ORTI");
        assert_eq!(info["Cores"], cores);
        let types: Vec<&str> = info["Types"].as_array().unwrap().iter().map(|t| t["Name"].as_str().unwrap()).collect();
        assert!(types.contains(&"TASK"), "{types:?}");

        let r = vs.command("AurixOS", "getObjects", &[json!("TASK"), json!(task_name), json!(false)]);
        assert_eq!(r[0], "", "{r:?}");
        let got: Value = serde_json::from_str(&r[1]).unwrap();
        let attrs = got["Objects"][0]["Attributes"].as_array().unwrap();
        let by = |n: &str| attrs.iter().find(|a| a["Name"] == n).unwrap().clone();
        assert!(by("STACK")["Link"].is_string(), "{:?}", by("STACK"));
        assert_eq!(by("STACK")["Value"], by("STACK")["Link"]);
        assert!(by("vs_Schedule")["Value"].is_string(), "{:?}", by("vs_Schedule"));

        // `getRunning` is what the debug adapter calls on every stop to name its threads, so it
        // has to answer per core and stay small.
        let r = vs.command("AurixOS", "getRunning", &[]);
        assert_eq!(r[0], "", "{r:?}");
        let run: Value = serde_json::from_str(&r[1]).unwrap();
        let per_core = run["Cores"].as_array().unwrap();
        assert_eq!(per_core.len(), cores);
        for (core, c) in per_core.iter().enumerate() {
            assert_eq!(c["Core"], core);
        }
        for c in per_core {
            assert!(c["Task"].is_string(), "{c}");
            assert!(c["Priority"].is_string(), "{c}");
        }
        assert!(run["Reads"].as_u64().unwrap() <= 6 * cores as u64, "a stop must not cost much: {run}");
        // The AUTOSAR core id goes out as a number: it is for matching a debug target, not for
        // a person to read.
        assert!(per_core.iter().all(|c| c["CoreId"].is_number()), "{run}");


        // Stack coverage. The simulator's RAM is not filled with the OS pattern, so the numbers
        // are meaningless - what matters is that every stack is described and measured, and that
        // an unreadable one says so instead of reporting zero.
        let cov = http.ok("GET", "/api/v1/os/stacks", None);
        let stacks = cov["stacks"].as_array().unwrap();
        assert_eq!(stacks.len(), stack_count, "one entry per STACK object");
        for s in stacks {
            assert!(s["name"].is_string() && s["size"].as_u64().is_some(), "{s}");
            assert_eq!(s["direction"], "DOWN", "{s}");
            // Vector fills with 0xAAAAAAAA; the value comes from the ORTI file, not from us.
            assert_eq!(s["pattern"], 0xAAAA_AAAAu32, "{s}");
            // Either it was measured or it explains itself - never silently absent.
            assert!(s["used"].is_u64() || s["error"].is_string(), "{s}");
            if let Some(u) = s["used"].as_u64() {
                assert!(u <= s["size"].as_u64().unwrap(), "used beyond the end: {s}");
                assert_eq!(s["free"].as_u64().unwrap(), s["size"].as_u64().unwrap() - u, "{s}");
            }
        }
        assert!(cov["reads"].as_u64().unwrap() > 0);

        // Narrowing to one stack is what a panel does when the user picks a row: far cheaper.
        let one = http.ok("GET", &format!("/api/v1/os/stacks?names={stack_name}"), None);
        assert_eq!(one["stacks"].as_array().unwrap().len(), 1, "{one}");
        assert_eq!(one["stacks"][0]["name"], stack_name.as_str());
        assert!(one["reads"].as_u64().unwrap() < cov["reads"].as_u64().unwrap(), "narrowing must cost less");

        // A name nobody declared measures nothing rather than erroring: a panel may be showing a
        // stale selection after the ORTI file changed.
        let none = http.ok("GET", "/api/v1/os/stacks?names=nope", None);
        assert!(none["stacks"].as_array().unwrap().is_empty(), "{none}");
        assert_eq!(none["reads"], 0);

        // ...and over TCF, which is the channel the extension uses.
        let r = vs.command("AurixOS", "getStacks", &[Value::Null]);
        assert_eq!(r[0], "", "{r:?}");
        let got: Value = serde_json::from_str(&r[1]).unwrap();
        assert_eq!(got["Stacks"].as_array().unwrap().len(), stack_count);


        // Sampled task profiling. The simulator's RAM never changes, so there is nothing to
        // observe except the first baseline sample per core - which is exactly the shape the
        // panel consumes, and enough to prove the sampler, the decoder and the accounting.
        let r = vs.command("AurixOS", "getProfile", &[json!(0)]);
        assert_eq!(r[0], "", "{r:?}");
        let idle: Value = serde_json::from_str(&r[1]).unwrap();
        assert_eq!(idle["Running"], false, "nothing runs before it is started: {idle}");

        let r = vs.command("AurixOS", "startProfile", &[json!(500)]);
        assert_eq!(r[0], "", "{r:?}");
        let started: Value = serde_json::from_str(&r[1]).unwrap();
        assert_eq!(started["Cores"], cores, "one sampling point per core: {started}");
        assert_eq!(started["PeriodUs"], 500);

        // Give the sampler a moment to deliver its baseline.
        thread::sleep(Duration::from_millis(300));
        let r = vs.command("AurixOS", "getProfile", &[json!(0)]);
        let p: Value = serde_json::from_str(&r[1]).unwrap();
        assert_eq!(p["Running"], true, "{p}");
        assert!(p["NowUs"].as_u64().unwrap() > 0, "{p}");
        let trans = p["Transitions"].as_array().unwrap();
        assert_eq!(trans.len(), cores, "one baseline transition per core: {p}");
        for t in trans {
            // The bytes came back through the ORTI enumerator table, so this is a task name.
            assert_eq!(t["Task"], "INVALID_TASK", "{t}");
            assert!(t["Core"].is_u64() && t["T"].is_u64(), "{t}");
        }
        // Occupancy counts the time each core has spent in its current task, not only closed ones.
        let totals = p["Totals"].as_array().unwrap();
        assert_eq!(totals.len(), cores, "{p}");
        assert!(totals.iter().all(|o| o["Us"].as_u64().unwrap() > 0), "{p}");
        assert_eq!(p["DroppedTransitions"], 0);

        // Polling with the last timestamp returns only what is new - nothing, here, because a
        // value that does not change is never delivered again.
        let now = p["NowUs"].as_u64().unwrap();
        let r = vs.command("AurixOS", "getProfile", &[json!(now)]);
        let p2: Value = serde_json::from_str(&r[1]).unwrap();
        assert!(p2["Transitions"].as_array().unwrap().is_empty(), "{p2}");
        assert!(p2["Totals"].as_array().unwrap().len() == cores, "totals keep accumulating: {p2}");

        // Starting again replaces the session rather than sampling the same points twice.
        let r = vs.command("AurixOS", "startProfile", &[json!(1000)]);
        assert_eq!(r[0], "", "{r:?}");
        let restarted: Value = serde_json::from_str(&r[1]).unwrap();
        assert_eq!(restarted["PeriodUs"], 1000);
        assert_eq!(h.call(|cx| cx.with(|c| c.daq.sessions().count())), Some(1), "the old sampling session must be gone");

        let r = vs.command("AurixOS", "stopProfile", &[]);
        assert_eq!(r[0], "", "{r:?}");
        assert_eq!(serde_json::from_str::<Value>(&r[1]).unwrap()["Stopped"], true);
        // Stopping releases the sampler: nothing keeps reading the target.
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(5) && h.call(|cx| cx.with(|c| c.daq.is_empty())) != Some(true) {
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(h.call(|cx| cx.with(|c| c.daq.is_empty())), Some(true), "the profiler outlived its stop");
        // Stopping twice is not an error: a panel closing says so blindly.
        let r = vs.command("AurixOS", "stopProfile", &[]);
        assert_eq!(serde_json::from_str::<Value>(&r[1]).unwrap()["Stopped"], false);

        // Errors come back as TCF errors, not as a reply that looks like success.
        let r = vs.command("AurixOS", "getObjects", &[json!("NOPE"), Value::Null, json!(false)]);
        assert_ne!(r[0], "", "{r:?}");
        assert!(r[0].contains("NOPE"), "{r:?}");
    }

    // Loading over TCF is what the extension will actually do. Without symbols it must say so
    // rather than failing obscurely.
    {
        let r = vs.command("AurixOS", "load", &[json!("no/such.ORT"), Value::Null]);
        assert!(r[0].contains("no such file"), "{r:?}");
    }

    // --- Variables by name ---------------------------------------------------------------------

    variables(&http, &mut vs);
    core_local_variables(&http, &mut vs);
    variables_of_real_elf(&http);

    // Web pages of other sites must not drive the debugger.
    let (status, _) = http.request("POST", &format!("/api/v1/targets/{CPU0}/halt"), None, &[("Origin", "http://evil.example")]);
    assert_eq!(status, 400);
    let (status, _) = http.request("GET", "/api/v1/targets", None, &[("Origin", "http://localhost:8080")]);
    assert_eq!(status, 200);
}
