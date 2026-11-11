//! The hub's HTTP routes.
//!
//! | Route | |
//! |---|---|
//! | `/mcp` | MCP (see [`crate::mcp`]) |
//! | `/api/v1/hub`, `/api/v1/sessions`, `/api/v1/profiles`, ... | the hub's operations |
//! | `/api/v1/sessions/{session}/...` | a session's operations, forwarded to its worker |
//! | `/api/v1/session/...` | the same for the only open session |
//! | `/api/v1/sessions/{session}/events`, `.../daq`, `.../tcf` | event stream, DAQ and TCF relays |
//! | `/api/v1/events` | the hub's events (sessions opened, launched, closed) |
//! | `/api/v1/files` | uploads (`file:` IDs) |
//! | `/docs`, `/openapi.json` | Swagger UI and the OpenAPI document |
//! | `/health`, `/shutdown`, `/internal/*` | liveness, stopping, worker registration |
//!
//! Clients on other machines need the token (`Authorization: Bearer <token>`) on every route
//! but `/health`; `/internal/*` is for this machine only.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::ws::{Message as AxMsg, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, HeaderMap, Method as HttpMethod, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post, MethodFilter, MethodRouter};
use axum::{Json, Router};
use futures_util::{SinkExt, Stream, StreamExt};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::broadcast::error::RecvError;

use api_spec::types::{HubEvent, WorkerRegistration};
use api_spec::{ApiError, ErrorCode, Method, OpMeta, CLIENT_HEADER, HUB_OPS};

use crate::state::{with_client, Choose, Hub, Target};
use crate::{files, hlog};

/// Whether the request comes from this machine (set by the auth layer).
#[derive(Clone, Copy)]
pub struct Local(pub bool);

type Shared = Arc<Hub>;

pub fn error_response(e: ApiError) -> Response {
    let status = StatusCode::from_u16(e.code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(e)).into_response()
}

fn choose_response(c: Choose) -> Response {
    let (e, guidance) = crate::mcp::choose_error(c);
    let status = StatusCode::from_u16(e.code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut v = serde_json::to_value(&e).unwrap_or_default();
    if let (Some(g), Some(o)) = (guidance, v.as_object_mut()) {
        o.insert("status".into(), g["status"].clone());
        for k in ["sessions", "profiles"] {
            if let Some(x) = g.get(k) {
                o.insert(k.into(), x.clone());
            }
        }
    }
    (status, Json(v)).into_response()
}

/// The caller's client name; the hub's own name is refused.
fn caller(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    match headers.get(CLIENT_HEADER).map(|v| v.to_str()) {
        None => Ok(None),
        Some(Ok(n)) if !n.is_empty() && n.len() <= 128 && n.bytes().all(|b| b.is_ascii_graphic()) && n != "aurix-hub" => Ok(Some(n.to_string())),
        Some(_) => Err(ApiError::bad_request(format!("invalid `{CLIENT_HEADER}`"))),
    }
}

// --- Authentication -----------------------------------------------------------------------------

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn auth(State(hub): State<Shared>, mut req: Request, next: Next) -> Response {
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0);
    let local = peer.is_none_or(|p| p.ip().is_loopback());
    let path = req.uri().path().to_string();
    if path.starts_with("/internal/") && !local {
        return error_response(ApiError::new(ErrorCode::NotFound, "not found"));
    }
    if !local && path != "/health" {
        let given = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
            .map(str::trim);
        let ok = match (hub.cfg.token(), given) {
            (Some(t), Some(g)) => constant_time_eq(t.as_bytes(), g.as_bytes()),
            _ => false,
        };
        if !ok {
            return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer")], Json(ApiError::bad_request("a token is needed (Authorization: Bearer <token>)"))).into_response();
        }
    }
    // Browsers send `Origin`: only pages of this hub (Swagger UI) may call it.
    if let Some(origin) = req.headers().get(header::ORIGIN).and_then(|o| o.to_str().ok()) {
        let authority = origin.split_once("://").map_or("", |(_, a)| a).trim_end_matches('/');
        let host = authority.rsplit_once(':').filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit())).map_or(authority, |(h, _)| h);
        let same = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()) == Some(authority);
        if !(same || matches!(host, "localhost" | "127.0.0.1" | "[::1]")) {
            return error_response(ApiError::bad_request(format!("requests from origin {origin} are not allowed")));
        }
    }
    req.extensions_mut().insert(Local(local));
    next.run(req).await
}

// --- The hub's operations -------------------------------------------------------------------------

fn param_kind(op: &OpMeta, name: &str) -> &'static str {
    let s = api_spec::gen::inline_schema(op.request);
    let p = &s["properties"][name];
    let t = match &p["type"] {
        Value::String(t) => t.clone(),
        Value::Array(ts) => ts.iter().filter_map(Value::as_str).find(|t| *t != "null").unwrap_or("string").to_string(),
        _ => "string".into(),
    };
    match t.as_str() {
        "integer" => "integer",
        "number" => "number",
        "boolean" => "boolean",
        _ => "string",
    }
}

fn coerce(kind: &str, raw: String) -> Value {
    match kind {
        "integer" => raw.parse::<i64>().map(Value::from).unwrap_or(Value::String(raw)),
        "number" => raw.parse::<f64>().map(Value::from).unwrap_or(Value::String(raw)),
        "boolean" => match raw.as_str() {
            "true" | "1" => Value::Bool(true),
            "false" | "0" => Value::Bool(false),
            _ => Value::String(raw),
        },
        _ => Value::String(raw),
    }
}

async fn hub_op(hub: Shared, op: &'static OpMeta, uri: Uri, headers: HeaderMap, query: Vec<(String, String)>, body: Bytes) -> Response {
    let client = match caller(&headers) {
        Ok(c) => c,
        Err(e) => return error_response(e),
    };
    let mut args = Map::new();
    if !body.is_empty() {
        match serde_json::from_slice::<Value>(&body) {
            Ok(Value::Object(m)) => args = m,
            Ok(Value::Null) => {}
            Ok(_) => return error_response(ApiError::bad_request("the request body must be a JSON object")),
            Err(e) => return error_response(ApiError::bad_request(format!("request body: {e}"))),
        }
    }
    for (k, v) in query {
        let kind = param_kind(op, &k);
        args.insert(k, coerce(kind, v));
    }
    for (t, v) in op.path.split('/').zip(uri.path().split('/')) {
        if let Some(name) = t.strip_prefix('{').and_then(|t| t.strip_suffix('}')) {
            args.insert(name.to_string(), Value::String(percent_decode(v)));
        }
    }
    let mut args = Value::Object(args);
    if let Err(e) = files::resolve_ids(&mut args) {
        return error_response(ApiError::bad_request(e));
    }
    let r = tokio::task::spawn_blocking(move || with_client(client, || api_spec::hub_dispatch(&*hub, op.name, args))).await;
    match r {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => error_response(e),
        Err(e) => error_response(ApiError::failed(format!("{} failed: {e}", op.name))),
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// --- Session proxy ------------------------------------------------------------------------------------

fn client() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| reqwest::Client::builder().no_proxy().connect_timeout(Duration::from_secs(5)).build().expect("HTTP client"))
}

async fn choose(hub: &Shared, key: Option<String>) -> Result<(String, String), Response> {
    let h = hub.clone();
    match tokio::task::spawn_blocking(move || h.choose(key.as_deref())).await {
        Ok(Ok(Target::Worker { id, base })) => Ok((id, base)),
        Ok(Err(c)) => Err(choose_response(c)),
        Err(e) => Err(error_response(ApiError::failed(e.to_string()))),
    }
}

/// Forwards a session request to its worker, streaming the answer (SSE stays a stream).
async fn proxy(hub: Shared, key: Option<String>, rest: String, local: bool, method: HttpMethod, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    let client_name = match caller(&headers) {
        Ok(c) => c,
        Err(e) => return error_response(e),
    };
    let rest = rest.trim_start_matches('/').to_string();
    // `wait_session` knows about the start of the session, before its worker answers.
    if rest == "launch" && method == HttpMethod::GET {
        let timeout = uri.query().and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("timeout_s="))).and_then(|v| v.parse().ok());
        let h = hub.clone();
        return match tokio::task::spawn_blocking(move || h.wait_session(key.as_deref(), timeout)).await {
            Ok(Ok(v)) => Json(v).into_response(),
            Ok(Err(c)) => choose_response(c),
            Err(e) => error_response(ApiError::failed(e.to_string())),
        };
    }
    if !local && !hub.cfg.allow_tcl && (rest == "tcl" || rest == "ops/tcl") {
        return error_response(ApiError::bad_request("Tcl commands from other machines are switched off on this hub (allow_tcl = false)"));
    }
    let (_, base) = match choose(&hub, key).await {
        Ok(t) => t,
        Err(r) => return r,
    };
    let mut body = body;
    let json_body = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_none_or(|c| c.contains("json"));
    if !body.is_empty() && json_body {
        if let Ok(mut v) = serde_json::from_slice::<Value>(&body) {
            if let Err(e) = files::resolve_ids(&mut v) {
                return error_response(ApiError::bad_request(e));
            }
            body = Bytes::from(v.to_string());
        }
    }
    let url = format!("{base}/api/v1/{rest}{}", uri.query().map(|q| format!("?{q}")).unwrap_or_default());
    let m = reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap_or(reqwest::Method::GET);
    let mut req = client().request(m, &url);
    if let Some(c) = client_name {
        req = req.header(CLIENT_HEADER, c);
    }
    if !body.is_empty() {
        req = req.header(reqwest::header::CONTENT_TYPE, "application/json").body(body);
    }
    match req.send().await {
        Ok(resp) => {
            let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let ctype = resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_string);
            let mut out = Response::builder().status(status);
            if let Some(c) = ctype {
                out = out.header(header::CONTENT_TYPE, c);
            }
            out.body(Body::from_stream(resp.bytes_stream())).unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
        }
        Err(e) => error_response(ApiError::failed(format!("the session's worker does not answer: {e}"))),
    }
}

async fn ws_daq(hub: Shared, key: Option<String>, ws: WebSocketUpgrade) -> Response {
    let (_, base) = match choose(&hub, key).await {
        Ok(t) => t,
        Err(r) => return r,
    };
    let url = format!("{}/api/v1/daq", base.replacen("http://", "ws://", 1));
    ws.on_upgrade(move |socket| async move {
        let upstream = match tokio_tungstenite::connect_async(&url).await {
            Ok((s, _)) => s,
            Err(e) => {
                hlog!("DAQ relay: cannot reach {url}: {e}");
                return;
            }
        };
        relay_ws(socket, upstream).await;
    })
}

async fn relay_ws(socket: WebSocket, upstream: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>) {
    use tokio_tungstenite::tungstenite::Message as TMsg;
    let (mut down_tx, mut down_rx) = socket.split();
    let (mut up_tx, mut up_rx) = upstream.split();
    let to_worker = async {
        while let Some(Ok(m)) = down_rx.next().await {
            let t = match m {
                AxMsg::Text(t) => TMsg::text(t.as_str().to_string()),
                AxMsg::Binary(b) => TMsg::binary(b.to_vec()),
                AxMsg::Ping(p) => TMsg::Ping(p.to_vec().into()),
                AxMsg::Pong(p) => TMsg::Pong(p.to_vec().into()),
                AxMsg::Close(_) => break,
            };
            if up_tx.send(t).await.is_err() {
                break;
            }
        }
        let _ = up_tx.close().await;
    };
    let to_client = async {
        while let Some(Ok(m)) = up_rx.next().await {
            let a = match m {
                TMsg::Text(t) => AxMsg::Text(t.as_str().to_string().into()),
                TMsg::Binary(b) => AxMsg::Binary(b.to_vec().into()),
                TMsg::Ping(p) => AxMsg::Ping(p.to_vec().into()),
                TMsg::Pong(p) => AxMsg::Pong(p.to_vec().into()),
                TMsg::Close(_) => break,
                TMsg::Frame(_) => continue,
            };
            if down_tx.send(a).await.is_err() {
                break;
            }
        }
        let _ = down_tx.close().await;
    };
    tokio::select! { _ = to_worker => {}, _ = to_client => {} }
}

/// TCF over a WebSocket: binary frames carry the bytes of the worker's TCF connection, for a
/// VS Code on another machine (TCF itself has no authentication; the hub's token guards this).
async fn ws_tcf(hub: Shared, key: Option<String>, ws: WebSocketUpgrade) -> Response {
    let (id, _) = match choose(&hub, key).await {
        Ok(t) => t,
        Err(r) => return r,
    };
    let port = hub.lock().sessions.iter().find(|s| s.id == id).and_then(|s| s.tcf_port);
    let Some(port) = port else {
        return error_response(ApiError::not_found("the session has no TCF port"));
    };
    ws.on_upgrade(move |socket| async move {
        let tcp = match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
            Ok(t) => t,
            Err(e) => {
                hlog!("TCF relay: cannot reach port {port}: {e}");
                return;
            }
        };
        let _ = tcp.set_nodelay(true);
        let (mut rd, mut wr) = tcp.into_split();
        let (mut tx, mut rx) = socket.split();
        let up = async {
            while let Some(Ok(m)) = rx.next().await {
                let data = match m {
                    AxMsg::Binary(b) => b.to_vec(),
                    AxMsg::Text(t) => t.as_str().as_bytes().to_vec(),
                    AxMsg::Close(_) => break,
                    _ => continue,
                };
                if wr.write_all(&data).await.is_err() {
                    break;
                }
            }
            let _ = wr.shutdown().await;
        };
        let down = async {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match rd.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(AxMsg::Binary(buf[..n].to_vec().into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = tx.close().await;
        };
        tokio::select! { _ = up => {}, _ = down => {} }
    })
}

// --- Routes ---------------------------------------------------------------------------------------------

async fn hub_events(hub: Shared) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = hub.events.subscribe();
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        let ev = match rx.recv().await {
            Ok(ev) => ev,
            Err(RecvError::Lagged(missed)) => HubEvent::Lagged { missed },
            Err(RecvError::Closed) => return None,
        };
        let v = serde_json::to_value(&ev).unwrap_or_default();
        let name = v.get("type").and_then(Value::as_str).unwrap_or("event").to_string();
        Some((Ok(SseEvent::default().event(name).data(v.to_string())), rx))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

fn health(hub: &Hub) -> Value {
    json!({
        "ok": true,
        "role": "aurix-hub",
        "version": env!("CARGO_PKG_VERSION"),
        "pid": std::process::id(),
        "instance": hub.instance,
        "port": hub.port,
        "exe": std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
        "sessions": hub.live_sessions(),
    })
}

/// `POST /shutdown`: `{}` stops the hub (the workers keep running and register with the next
/// one), `{"now": true}` closes the sessions first, `{"when_idle": true, "exe": "..."}` restarts
/// from `exe` (default: the installed hub) once no session is open.
async fn shutdown(hub: Shared, body: Bytes) -> Response {
    let v: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
    if v["when_idle"] == true {
        if let Some(exe) = v["exe"].as_str() {
            *hub.restart_exe.lock().unwrap() = Some(exe.into());
        }
        hub.update_pending.store(true, std::sync::atomic::Ordering::Relaxed);
        hlog!("update pending: restarting once no session is open");
        return Json(json!({ "ok": true, "pending": true, "sessions": hub.live_sessions() })).into_response();
    }
    let now = v["now"] == true;
    let h = hub.clone();
    tokio::task::spawn_blocking(move || {
        if now {
            h.close_all();
        }
        hlog!("stopping (requested)");
        h.stop();
    });
    Json(json!({ "ok": true })).into_response()
}

async fn upload(Query(q): Query<Vec<(String, String)>>, body: Bytes) -> Response {
    let name = q.iter().find(|(k, _)| k == "name").map(|(_, v)| v.clone()).unwrap_or_else(|| "upload.bin".into());
    match tokio::task::spawn_blocking(move || files::store(&name, &body).map(|(id, p)| (id, p, body.len()))).await {
        Ok(Ok((id, path, size))) => Json(json!({ "file": id, "path": path.to_string_lossy(), "size": size })).into_response(),
        Ok(Err(e)) => error_response(ApiError::failed(e)),
        Err(e) => error_response(ApiError::failed(e.to_string())),
    }
}

fn index(hub: &Hub) -> Value {
    let sessions: Vec<Value> = hub
        .lock()
        .sessions
        .iter()
        .map(|s| json!({ "session": s.id, "name": s.name, "state": s.state, "base_url": format!("{}{}", api_spec::SESSION_PREFIX, s.id) }))
        .collect();
    json!({
        "service": "aurix-hub",
        "version": env!("CARGO_PKG_VERSION"),
        "mcp": "/mcp",
        "docs": "/docs",
        "openapi": "/openapi.json",
        "sessions_url": "/api/v1/sessions",
        "single_session_url": api_spec::SINGLE_SESSION_PREFIX,
        "sessions": sessions,
        "routing": match sessions.len() {
            0 => "No session is open: POST /api/v1/sessions (open_session) opens one.",
            1 => "/api/v1/session/... goes to the only open session.",
            _ => "Several sessions are open: use /api/v1/sessions/{session}/...",
        },
    })
}

fn method_filter(m: Method) -> MethodFilter {
    match m {
        Method::GET => MethodFilter::GET,
        Method::POST => MethodFilter::POST,
        Method::PUT => MethodFilter::PUT,
        Method::DELETE => MethodFilter::DELETE,
    }
}

pub fn router(hub: Shared) -> Router {
    let openapi: &'static str = Box::leak(api_spec::gen::openapi_json().into_boxed_str());
    let mut ops: BTreeMap<&str, MethodRouter<Shared>> = BTreeMap::new();
    for op in HUB_OPS {
        let handler = move |State(h): State<Shared>, uri: Uri, headers: HeaderMap, Query(q): Query<Vec<(String, String)>>, body: Bytes| hub_op(h, op, uri, headers, q, body);
        let r = ops.remove(op.path).unwrap_or_default();
        ops.insert(op.path, r.on(method_filter(op.method), handler));
    }
    let mut router: Router<Shared> = Router::new();
    for (path, r) in ops {
        router = router.route(path, r);
    }
    router
        .route("/health", get(|State(h): State<Shared>| async move { Json(health(&h)) }).post(|State(h): State<Shared>| async move { Json(health(&h)) }))
        .route("/", get(|State(h): State<Shared>| async move { Json(index(&h)) }))
        .route(
            "/internal/register",
            post(|State(h): State<Shared>, Json(reg): Json<WorkerRegistration>| async move { Json(tokio::task::spawn_blocking(move || h.register(reg)).await.unwrap()) }),
        )
        .route(
            "/internal/deregister",
            post(|State(h): State<Shared>, Json(v): Json<Value>| async move {
                let (s, pid) = (v["session"].as_str().unwrap_or_default().to_string(), v["pid"].as_u64().unwrap_or(0) as u32);
                tokio::task::spawn_blocking(move || h.deregister(&s, pid)).await.ok();
                StatusCode::NO_CONTENT
            }),
        )
        .route("/shutdown", post(|State(h): State<Shared>, body: Bytes| shutdown(h, body)))
        .route(
            "/mcp",
            post(|State(h): State<Shared>, axum::Extension(Local(l)): axum::Extension<Local>, headers: HeaderMap, body: Bytes| crate::mcp::post(h, l, headers, body))
                .delete(|State(h): State<Shared>, headers: HeaderMap| crate::mcp::delete(h, headers))
                .get(|| async { StatusCode::METHOD_NOT_ALLOWED }),
        )
        .route("/api/v1/events", get(|State(h): State<Shared>| hub_events(h)))
        .route("/api/v1/files", post(upload).layer(DefaultBodyLimit::max(1 << 31)))
        .route("/api/v1/sessions/{session}/daq", get(|State(h): State<Shared>, Path(s): Path<String>, ws: WebSocketUpgrade| ws_daq(h, Some(s), ws)))
        .route("/api/v1/sessions/{session}/tcf", get(|State(h): State<Shared>, Path(s): Path<String>, ws: WebSocketUpgrade| ws_tcf(h, Some(s), ws)))
        .route("/api/v1/session/daq", get(|State(h): State<Shared>, ws: WebSocketUpgrade| ws_daq(h, None, ws)))
        .route("/api/v1/session/tcf", get(|State(h): State<Shared>, ws: WebSocketUpgrade| ws_tcf(h, None, ws)))
        .route(
            "/api/v1/sessions/{session}/{*rest}",
            any(|State(h): State<Shared>, axum::Extension(Local(l)): axum::Extension<Local>, Path((s, rest)): Path<(String, String)>, m: HttpMethod, uri: Uri, headers: HeaderMap, body: Bytes| {
                proxy(h, Some(s), rest, l, m, uri, headers, body)
            }),
        )
        .route(
            "/api/v1/session/{*rest}",
            any(|State(h): State<Shared>, axum::Extension(Local(l)): axum::Extension<Local>, Path(rest): Path<String>, m: HttpMethod, uri: Uri, headers: HeaderMap, body: Bytes| {
                proxy(h, None, rest, l, m, uri, headers, body)
            }),
        )
        .route("/openapi.json", get(move || async move { ([(header::CONTENT_TYPE, "application/json")], openapi) }))
        .with_state(hub.clone())
        .merge(swagger_ui_dist::generate_routes(swagger_ui_dist::ApiDefinition {
            uri_prefix: "/docs",
            api_definition: swagger_ui_dist::OpenApiSource::Uri("/openapi.json"),
            title: Some("aurix-dbg API"),
        }))
        .layer(middleware::from_fn_with_state(hub, auth))
}
