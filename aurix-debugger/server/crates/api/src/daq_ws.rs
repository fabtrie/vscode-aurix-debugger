//! The DAQ WebSocket at `GET /api/v1/daq`: configuration and sample stream on one socket.
//!
//! Configuration travels on the socket rather than through REST operations for one reason: the
//! session then lives and dies with the connection. When a client crashes, the socket closes, the
//! guard below removes the session on the core thread, and nothing keeps reading the target. There
//! is no reaper and no idle timeout to get wrong.
//!
//! Because it is not an `api-spec` operation, DAQ appears in no OpenAPI path, no generated Python
//! method and - deliberately - no MCP tool: a streaming session is of no use to an LLM and would
//! only hold a socket open. The protocol is specified in the README.
//!
//! Three stages, so neither the core thread nor the async runtime ever does heavy work:
//! the core thread fills a bounded channel with raw bytes, a thread per session decodes them to
//! JSON, and the socket task only writes.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use serde_json::{json, Map, Value};

use aurix_core::daq::{self, DaqMsg, DaqState, PointId, SampleMode, SessionCfg, SessionId};

use crate::core_impl::CoreApi;
use crate::daq_resolve::{self, Decoder, Resolved, Spec};

/// Per-point state the decoding thread needs; built once at resolve time.
struct Live {
    id: PointId,
    name: String,
    decoder: Decoder,
}

/// Removes the session from the core registry however the socket ends, including on a panic.
struct Guard {
    api: Arc<CoreApi>,
    id: SessionId,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let (api, id) = (self.api.clone(), self.id);
        // On a detached thread: `call` waits for the core thread, which may be busy flashing for
        // seconds, and this can run on the single-threaded async runtime that serves every other
        // API client. If it never gets through, the core still reaps the session at its next tick,
        // when the sample channel reports its consumer gone.
        let _ = thread::Builder::new().name(format!("daq-close-{id}")).spawn(move || {
            api.handle.call(move |cx| cx.with(|c| c.daq.close(id)));
        });
    }
}

pub async fn handler(api: Arc<CoreApi>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| serve(api, socket))
}

fn err_frame(id: Option<u64>, code: &str, message: impl Into<String>) -> String {
    let mut m = Map::new();
    m.insert("type".into(), json!("error"));
    if let Some(id) = id {
        m.insert("id".into(), json!(id));
    }
    m.insert("code".into(), json!(code));
    m.insert("message".into(), json!(message.into()));
    Value::Object(m).to_string()
}

fn point_json(r: &Resolved, id: Option<PointId>) -> Value {
    let mut m = Map::new();
    if let Some(id) = id {
        m.insert("id".into(), json!(id));
    }
    m.insert("name".into(), json!(r.name));
    if let Some(e) = &r.expression {
        m.insert("expression".into(), json!(e));
    }
    if let Some(t) = &r.target {
        m.insert("target".into(), json!(t));
    }
    if let Some(a) = r.address {
        m.insert("address".into(), json!(a));
    }
    if let Some(s) = r.size {
        m.insert("size".into(), json!(s));
    }
    if let Some((shift, bits)) = r.bits {
        m.insert("bits".into(), json!([shift, bits]));
    }
    if let Some(t) = &r.type_name {
        m.insert("type_name".into(), json!(t));
    }
    m.insert("requested_interval_us".into(), json!(r.requested_interval_us));
    let granted = r.cfg.as_ref().map(|_| daq::quantize(r.requested_interval_us)).unwrap_or(0);
    m.insert("interval_us".into(), json!(granted));
    m.insert("rate_hz".into(), json!(if granted > 0 { 1_000_000.0 / granted as f64 } else { 0.0 }));
    m.insert("encoding".into(), json!(r.decoder.name()));
    m.insert("mode".into(), json!(mode_name(r.cfg.as_ref().map(|c| c.mode).unwrap_or(SampleMode::EveryTick))));
    if let Some(e) = &r.error {
        m.insert("error".into(), json!(e));
    }
    Value::Object(m)
}

fn mode_name(m: SampleMode) -> &'static str {
    match m {
        SampleMode::EveryTick => "every_tick",
        SampleMode::OnChange => "on_change",
    }
}

/// `pacing` is gone: sampling always delivers at least the requested rate, because the `exact`
/// alternative busy-waited the core thread to deliver *fewer* samples. Rejected rather than
/// ignored, so a script that still sets it is told instead of quietly getting something else.
fn reject_pacing(v: &serde_json::Map<String, Value>) -> Result<(), String> {
    match v.contains_key("pacing") {
        false => Ok(()),
        true => Err("`pacing` was removed; every point now delivers at least the rate asked for".into()),
    }
}

fn parse_mode(v: Option<&Value>, dflt: SampleMode) -> Result<SampleMode, String> {
    match v.and_then(Value::as_str) {
        None => Ok(dflt),
        Some("every_tick") => Ok(SampleMode::EveryTick),
        Some("on_change") => Ok(SampleMode::OnChange),
        Some(other) => Err(format!("unknown mode '{other}' (every_tick or on_change)")),
    }
}

fn parse_addr(v: &Value) -> Option<u32> {
    match v {
        Value::Number(n) => n.as_u64().and_then(|x| u32::try_from(x).ok()),
        Value::String(s) => {
            let s = s.trim();
            match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                Some(h) => u32::from_str_radix(h, 16).ok(),
                None => s.parse().ok(),
            }
        }
        _ => None,
    }
}

/// One point of a `subscribe` or `add` frame.
fn parse_spec(v: &Value, dflt_mode: SampleMode, dflt_interval: u32) -> Result<Spec, String> {
    let o = v.as_object().ok_or("each point must be an object")?;
    reject_pacing(o)?;
    let interval_us = match (o.get("interval_us").and_then(Value::as_u64), o.get("rate_hz").and_then(Value::as_f64)) {
        (Some(_), Some(_)) => return Err("give either `interval_us` or `rate_hz`, not both".into()),
        (Some(us), None) => us as u32,
        (None, Some(hz)) if hz > 0.0 => (1_000_000.0 / hz).round() as u32,
        (None, Some(_)) => return Err("`rate_hz` must be positive".into()),
        (None, None) => dflt_interval,
    };
    Ok(Spec {
        expression: o.get("expression").and_then(Value::as_str).map(str::to_string),
        address: o.get("address").and_then(parse_addr),
        size: o.get("size").and_then(Value::as_u64).map(|s| s as u32),
        interval_us,
        target: o.get("target").and_then(Value::as_str).map(str::to_string),
        name: o.get("name").and_then(Value::as_str).map(str::to_string),
        mode: parse_mode(o.get("mode"), dflt_mode)?,
        resend_after_us: o.get("resend_after_us").and_then(Value::as_u64).unwrap_or(0) as u32,
        decode: o.get("decode").and_then(Value::as_bool).unwrap_or(true),
    })
}

/// Resolves specs and registers the ones that worked with the core.
///
/// Every point of one call is registered in a single core job, so they all get the same starting
/// instant and share their ticks. Resolving a name takes a moment, so adding them one at a time
/// would scatter their phases and produce one message per point instead of one per tick.
fn add_points(api: &Arc<CoreApi>, id: SessionId, specs: &[Spec], live: &mut Vec<Live>) -> Result<Vec<Value>, String> {
    let cores = tcf::cores(&api.handle);
    let mut out = Vec::with_capacity(specs.len());
    // Resolved points waiting to be registered, with the slot in `out` that reports them.
    let mut pending: Vec<(usize, Resolved)> = Vec::new();
    for spec in specs {
        // A point may name its own core, for HSM-private memory or MCS program RAM.
        let core = match &spec.target {
            Some(n) => match cores.iter().find(|c| &c.name == n || c.idx.to_string() == *n) {
                Some(c) => Some(c.idx),
                None => {
                    let r = daq_resolve::resolve(&api.handle, None, 0, None, spec);
                    out.push(point_json(&Resolved { error: Some(format!("unknown target '{n}'")), ..r }, None));
                    continue;
                }
            },
            None => None,
        };
        // Symbols of the named core, else of the first core that has any.
        let ctx = match core {
            Some(idx) => cores.iter().find(|c| c.idx == idx).map(|c| c.id.clone()).unwrap_or_default(),
            None => String::new(),
        };
        let set = api.session.symbols_for(&ctx);
        let r = daq_resolve::resolve(&api.handle, set.as_ref(), core.unwrap_or(0), spec.target.as_deref(), spec);
        if r.cfg.is_none() {
            out.push(point_json(&r, None));
            continue;
        }
        out.push(Value::Null); // filled in once the point has an id
        pending.push((out.len() - 1, r));
    }
    if pending.is_empty() {
        return Ok(out);
    }
    let cfgs: Vec<_> = pending.iter().filter_map(|(_, r)| r.cfg.clone()).collect();
    let granted = api
        .handle
        .call(move |cx| cx.with(|c| c.daq.add_points(id, &cfgs, std::time::Instant::now())))
        .ok_or("the debug server stopped")?
        .map_err(|e| e.to_string())?;
    for ((slot, r), (pid, _)) in pending.into_iter().zip(granted) {
        live.push(Live { id: pid, name: r.name.clone(), decoder: r.decoder.clone() });
        out[slot] = point_json(&r, Some(pid));
    }
    Ok(out)
}

/// Turns one core message into a frame, or `None` when there is nothing to say.
fn frame(msg: DaqMsg, live: &[Live]) -> Option<String> {
    match msg {
        DaqMsg::Samples { t_us, values, .. } => {
            let mut points = Vec::with_capacity(values.len());
            let mut out = Vec::with_capacity(values.len());
            for (id, data) in values {
                let Some(l) = live.iter().find(|l| l.id == id) else { continue };
                points.push(id);
                out.push(l.decoder.decode(&data));
            }
            if points.is_empty() {
                return None;
            }
            Some(json!({ "type": "samples", "t_us": t_us, "points": points, "values": out }).to_string())
        }
        DaqMsg::Overrun { dropped, t_us } => Some(json!({ "type": "overrun", "t_us": t_us, "dropped": dropped }).to_string()),
        DaqMsg::Gap { missed, t_us } => Some(json!({ "type": "gap", "t_us": t_us, "missed": missed }).to_string()),
        DaqMsg::PointError { point, err, muted } => {
            Some(json!({ "type": "point_error", "point": point, "message": err, "muted": muted }).to_string())
        }
        DaqMsg::State { state, t_us } => {
            let s = match state {
                DaqState::Running => "running",
                DaqState::Reset => "reset",
                DaqState::Unavailable => "unavailable",
                DaqState::Stalled => "stalled",
            };
            Some(json!({ "type": "state", "t_us": t_us, "state": s }).to_string())
        }
    }
}

/// Drains the core channel on its own thread, decodes, and forwards ready-made frames.
///
/// Decoding belongs here: not on the core thread, which must only move bytes, and not on the
/// single-threaded async runtime, which serves every other API client.
fn spawn_decoder(id: SessionId, rx: Receiver<DaqMsg>, tx: tokio::sync::mpsc::Sender<String>, live: Arc<std::sync::Mutex<Vec<Live>>>) {
    let _ = thread::Builder::new().name(format!("daq-{id}")).spawn(move || loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(msg) => {
                let f = {
                    let l = live.lock().unwrap();
                    frame(msg, &l)
                };
                if let Some(f) = f {
                    // Blocking send: back pressure reaches the core, which then drops and counts.
                    if tx.blocking_send(f).is_err() {
                        return;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if tx.is_closed() {
                    return;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    });
}

async fn serve(api: Arc<CoreApi>, mut socket: WebSocket) {
    // Nothing is sampled until the client subscribes.
    let mut state: Option<(Guard, Arc<std::sync::Mutex<Vec<Live>>>, tokio::sync::mpsc::Receiver<String>)> = None;

    loop {
        // Either a client frame, or a sample to write.
        let incoming = match &mut state {
            Some((_, _, out)) => {
                tokio::select! {
                    msg = socket.recv() => msg,
                    sample = out.recv() => {
                        match sample {
                            Some(text) => {
                                if socket.send(Message::Text(text.into())).await.is_err() {
                                    return;
                                }
                                continue;
                            }
                            None => return,
                        }
                    }
                }
            }
            None => socket.recv().await,
        };
        let Some(Ok(msg)) = incoming else { return };
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Binary(_) => {
                let _ = socket.send(Message::Text(err_frame(None, "bad_request", "send JSON text frames").into())).await;
                continue;
            }
            Message::Close(_) => return,
            Message::Ping(_) | Message::Pong(_) => continue,
        };
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                let _ = socket.send(Message::Text(err_frame(None, "bad_request", format!("invalid JSON: {e}")).into())).await;
                continue;
            }
        };
        let req_id = v.get("id").and_then(Value::as_u64);
        let op = v.get("op").and_then(Value::as_str).unwrap_or_default().to_string();

        // Off the async runtime: handling an op waits for the core thread, and resolving a name
        // walks DWARF. Doing that here would stall every other API client, which is why the REST
        // handlers go through `spawn_blocking` too. The state travels with the task and comes back.
        let taken = state.take();
        let a = api.clone();
        let done = tokio::task::spawn_blocking(move || {
            let mut st = taken;
            let reply = handle_op(&a, &mut st, &op, &v, req_id);
            (reply, st)
        })
        .await;
        let reply = match done {
            Ok((reply, st)) => {
                state = st;
                reply
            }
            // The task panicked; its state (and with it the session) is already gone.
            Err(e) => err_frame(req_id, "failed", format!("DAQ request failed: {e}")),
        };
        if socket.send(Message::Text(reply.into())).await.is_err() {
            return;
        }
    }
}

type State = Option<(Guard, Arc<std::sync::Mutex<Vec<Live>>>, tokio::sync::mpsc::Receiver<String>)>;

fn handle_op(api: &Arc<CoreApi>, state: &mut State, op: &str, v: &Value, req_id: Option<u64>) -> String {
    match op {
        "subscribe" => {
            if state.is_some() {
                return err_frame(req_id, "busy", "this socket already has a session; use add, remove or set_rate");
            }
            let dflt_mode = match parse_mode(v.get("mode"), SampleMode::EveryTick) {
                Ok(m) => m,
                Err(e) => return err_frame(req_id, "bad_request", e),
            };
            if let Err(e) = v.as_object().map_or(Ok(()), reject_pacing) {
                return err_frame(req_id, "bad_request", e);
            }
            let dflt_interval = v.get("interval_us").and_then(Value::as_u64).unwrap_or(10_000) as u32;
            let specs: Vec<Spec> = match v.get("points").and_then(Value::as_array) {
                Some(a) => {
                    let mut out = Vec::with_capacity(a.len());
                    for p in a {
                        match parse_spec(p, dflt_mode, dflt_interval) {
                            Ok(s) => out.push(s),
                            Err(e) => return err_frame(req_id, "bad_request", e),
                        }
                    }
                    out
                }
                None => return err_frame(req_id, "bad_request", "`points` must be an array"),
            };
            let queue = v.get("queue").and_then(Value::as_u64).unwrap_or(daq::DEFAULT_QUEUE as u64) as usize;

            let opened = api.handle.call(move |cx| cx.with(|c| c.daq.open(SessionCfg { queue }, std::time::Instant::now())));
            let (id, rx) = match opened {
                Some(Ok(r)) => r,
                Some(Err(e)) => return err_frame(req_id, "resource", e.to_string()),
                None => return err_frame(req_id, "failed", "the debug server stopped"),
            };
            let guard = Guard { api: api.clone(), id };
            let mut live = Vec::new();
            let points = match add_points(api, id, &specs, &mut live) {
                Ok(p) => p,
                Err(e) => return err_frame(req_id, "bad_request", e),
            };
            let epoch_unix_us = api.handle.call(move |cx| cx.with(|c| c.daq.get(id).map(|s| s.epoch_unix_us))).flatten().unwrap_or(0);
            let live = Arc::new(std::sync::Mutex::new(live));
            let (tx, out) = tokio::sync::mpsc::channel(64);
            spawn_decoder(id, rx, tx, live.clone());
            *state = Some((guard, live, out));
            json!({
                "type": "config",
                "id": req_id,
                "start_wall_ms": epoch_unix_us / 1000,
                "points": points,
            })
            .to_string()
        }
        "add" => {
            let Some((guard, live, _)) = state.as_mut() else {
                return err_frame(req_id, "not_found", "subscribe first");
            };
            let specs: Vec<Spec> = match v.get("points").and_then(Value::as_array) {
                Some(a) => {
                    let mut out = Vec::with_capacity(a.len());
                    for p in a {
                        match parse_spec(p, SampleMode::EveryTick, 10_000) {
                            Ok(s) => out.push(s),
                            Err(e) => return err_frame(req_id, "bad_request", e),
                        }
                    }
                    out
                }
                None => return err_frame(req_id, "bad_request", "`points` must be an array"),
            };
            let mut l = live.lock().unwrap();
            match add_points(api, guard.id, &specs, &mut l) {
                Ok(points) => json!({ "type": "changed", "id": req_id, "points": points }).to_string(),
                Err(e) => err_frame(req_id, "bad_request", e),
            }
        }
        "remove" => {
            let Some((guard, live, _)) = state.as_mut() else {
                return err_frame(req_id, "not_found", "subscribe first");
            };
            let ids: Vec<PointId> = v.get("points").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).map(|x| x as PointId).collect()).unwrap_or_default();
            let id = guard.id;
            let gone = ids.clone();
            let _ = api.handle.call(move |cx| cx.with(|c| c.daq.remove_points(id, &gone)));
            live.lock().unwrap().retain(|l| !ids.contains(&l.id));
            json!({ "type": "changed", "id": req_id, "removed": ids }).to_string()
        }
        "set_rate" => {
            let Some((guard, _, _)) = state.as_mut() else {
                return err_frame(req_id, "not_found", "subscribe first");
            };
            let Some(point) = v.get("point").and_then(Value::as_u64).map(|x| x as PointId) else {
                return err_frame(req_id, "bad_request", "`point` is required");
            };
            let interval_us = match (v.get("interval_us").and_then(Value::as_u64), v.get("rate_hz").and_then(Value::as_f64)) {
                (Some(us), _) => us as u32,
                (None, Some(hz)) if hz > 0.0 => (1_000_000.0 / hz).round() as u32,
                _ => return err_frame(req_id, "bad_request", "give `interval_us` or a positive `rate_hz`"),
            };
            let id = guard.id;
            match api.handle.call(move |cx| cx.with(|c| c.daq.set_interval(id, point, interval_us, std::time::Instant::now()))) {
                Some(Ok(granted)) => json!({
                    "type": "changed", "id": req_id, "point": point,
                    "interval_us": granted, "rate_hz": 1_000_000.0 / granted as f64,
                })
                .to_string(),
                Some(Err(e)) => err_frame(req_id, "not_found", e.to_string()),
                None => err_frame(req_id, "failed", "the debug server stopped"),
            }
        }
        "set_mode" => {
            let Some((guard, _, _)) = state.as_mut() else {
                return err_frame(req_id, "not_found", "subscribe first");
            };
            let Some(point) = v.get("point").and_then(Value::as_u64).map(|x| x as PointId) else {
                return err_frame(req_id, "bad_request", "`point` is required");
            };
            let mode = match parse_mode(v.get("mode"), SampleMode::EveryTick) {
                Ok(m) => m,
                Err(e) => return err_frame(req_id, "bad_request", e),
            };
            let id = guard.id;
            match api.handle.call(move |cx| cx.with(|c| c.daq.set_mode(id, point, mode))) {
                Some(Ok(())) => json!({ "type": "changed", "id": req_id, "point": point, "mode": mode_name(mode) }).to_string(),
                Some(Err(e)) => err_frame(req_id, "not_found", e.to_string()),
                None => err_frame(req_id, "failed", "the debug server stopped"),
            }
        }
        "pause" | "resume" => {
            let Some((guard, _, _)) = state.as_mut() else {
                return err_frame(req_id, "not_found", "subscribe first");
            };
            let paused = op == "pause";
            let id = guard.id;
            match api.handle.call(move |cx| cx.with(|c| c.daq.set_paused(id, paused, std::time::Instant::now()))) {
                Some(Ok(())) => json!({ "type": "changed", "id": req_id, "paused": paused }).to_string(),
                Some(Err(e)) => err_frame(req_id, "not_found", e.to_string()),
                None => err_frame(req_id, "failed", "the debug server stopped"),
            }
        }
        "status" => {
            let Some((guard, live, _)) = state.as_mut() else {
                return err_frame(req_id, "not_found", "subscribe first");
            };
            let id = guard.id;
            let stats = api.handle.call(move |cx| cx.with(|c| c.daq.stats(id))).flatten();
            let names: Vec<Value> = live.lock().unwrap().iter().map(|l| json!({ "id": l.id, "name": l.name, "encoding": l.decoder.name() })).collect();
            json!({
                "type": "status", "id": req_id, "points": names,
                "dropped": stats.map(|s| s.dropped).unwrap_or(0),
            })
            .to_string()
        }
        "" => err_frame(req_id, "bad_request", "`op` is required"),
        other => err_frame(req_id, "bad_request", format!("unknown op '{other}'")),
    }
}
