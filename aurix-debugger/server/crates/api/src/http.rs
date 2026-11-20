//! HTTP routes of a worker: one REST route per session operation of the API definition, a
//! generic `/api/v1/ops/{op}` route (the hub's MCP front door calls it), the event stream and the
//! DAQ WebSocket. MCP, the OpenAPI document and Swagger UI are the hub's.

use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, Request};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, MethodFilter, MethodRouter};
use axum::{Json, Router};
use futures_util::Stream;
use serde_json::{Map, Value};
use tokio::sync::broadcast::error::RecvError;

use api_spec::types::ApiEvent;
use api_spec::{ApiError, Method, OpMeta, CLIENT_HEADER, OPS};

use crate::client;
use crate::core_impl::CoreApi;
use crate::events::Hub;

/// Runs an operation off the async runtime (the core call blocks), as `client`.
pub async fn run(api: Arc<CoreApi>, op: &'static str, args: Value, client: Option<String>) -> Result<Value, ApiError> {
    tokio::task::spawn_blocking(move || client::with_client(client, || api_spec::dispatch(&*api, op, args)))
        .await
        .unwrap_or_else(|e| Err(ApiError::failed(format!("{op} failed: {e}"))))
}

/// The calling client (`X-Aurix-Client`).
fn caller(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    match headers.get(CLIENT_HEADER) {
        None => Ok(None),
        Some(v) => match v.to_str() {
            Ok(name) if client::valid(name) => Ok(Some(name.to_string())),
            _ => Err(ApiError::bad_request(format!("`{CLIENT_HEADER}` must be printable ASCII without spaces, at most 128 bytes"))),
        },
    }
}

pub fn error_response(e: ApiError) -> Response {
    let status = StatusCode::from_u16(e.code.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(e)).into_response()
}

/// How a path or query parameter string becomes a JSON value, from the request schema.
#[derive(Clone, Debug, PartialEq)]
enum Kind {
    Int,
    Num,
    Bool,
    Str,
    List(Box<Kind>),
}

fn kind(s: &Value) -> Kind {
    if let Some(alts) = s.get("anyOf").or_else(|| s.get("oneOf")).and_then(Value::as_array) {
        let real: Vec<&Value> = alts.iter().filter(|a| a.get("type").and_then(Value::as_str) != Some("null")).collect();
        // Addresses (number or string) stay strings: the server parses them.
        return if real.len() == 1 { kind(real[0]) } else { Kind::Str };
    }
    let types: Vec<&str> = match s.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => vec![],
    };
    match types.iter().copied().find(|t| *t != "null") {
        Some("integer") => Kind::Int,
        Some("number") => Kind::Num,
        Some("boolean") => Kind::Bool,
        Some("array") => Kind::List(Box::new(s.get("items").map_or(Kind::Str, kind))),
        _ => Kind::Str,
    }
}

fn coerce(k: &Kind, raw: String) -> Value {
    match k {
        Kind::Int => raw.parse::<i64>().map(Value::from).unwrap_or(Value::String(raw)),
        Kind::Num => raw.parse::<f64>().map(Value::from).unwrap_or(Value::String(raw)),
        Kind::Bool => match raw.as_str() {
            "true" | "1" => Value::Bool(true),
            "false" | "0" => Value::Bool(false),
            _ => Value::String(raw),
        },
        Kind::Str | Kind::List(_) => Value::String(raw),
    }
}

fn param_kinds(op: &OpMeta) -> HashMap<String, Kind> {
    let schema = api_spec::gen::inline_schema(op.request);
    schema.get("properties").and_then(Value::as_object).map(|p| p.iter().map(|(k, s)| (k.clone(), kind(s))).collect()).unwrap_or_default()
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

/// Path parameter values of `path` for the route `template`.
fn path_values(template: &str, path: &str) -> Vec<(String, String)> {
    template
        .split('/')
        .zip(path.split('/'))
        .filter_map(|(t, v)| t.strip_prefix('{').and_then(|t| t.strip_suffix('}')).map(|name| (name.to_string(), percent_decode(v))))
        .collect()
}

async fn rest(api: Arc<CoreApi>, op: &'static OpMeta, kinds: Arc<HashMap<String, Kind>>, uri: Uri, headers: HeaderMap, query: Vec<(String, String)>, body: Bytes) -> Response {
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
        match kinds.get(&k) {
            Some(Kind::List(item)) => {
                let list = args.entry(k).or_insert_with(|| Value::Array(vec![]));
                if let Value::Array(a) = list {
                    a.push(coerce(item, v));
                }
            }
            Some(kd) => {
                args.insert(k, coerce(kd, v));
            }
            None => {
                args.insert(k, Value::String(v));
            }
        }
    }
    for (k, v) in path_values(op.path, uri.path()) {
        let value = coerce(kinds.get(&k).unwrap_or(&Kind::Str), v);
        args.insert(k, value);
    }
    match run(api, op.name, Value::Object(args), client).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => error_response(e),
    }
}

/// `GET /api/v1/svd/definitions[?device=TC37x]`: the peripheral viewer's definitions (its
/// `PeripheralOptions`) of the session's SVD description, or of a bundled device's, which is
/// then loaded for the session (as `load_svd` does).
async fn svd_definitions(api: Arc<CoreApi>, q: HashMap<String, String>) -> Response {
    let device = q.get("device").cloned();
    let r = tokio::task::spawn_blocking(move || crate::peripherals::definitions(&api, device.as_deref())).await;
    match r.unwrap_or_else(|e| Err(ApiError::failed(format!("definitions failed: {e}")))) {
        Ok(json) => ([(header::CONTENT_TYPE, "application/json")], json).into_response(),
        Err(e) => error_response(e),
    }
}

/// `POST /api/v1/ops/{op}` with the arguments as JSON object: any operation by name.
async fn by_name(api: Arc<CoreApi>, name: String, headers: HeaderMap, body: Bytes) -> Response {
    let client = match caller(&headers) {
        Ok(c) => c,
        Err(e) => return error_response(e),
    };
    let Some(op) = OPS.iter().find(|o| o.name == name) else {
        return error_response(ApiError::not_found(format!("unknown operation '{name}'")));
    };
    let args = if body.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice::<Value>(&body) {
            Ok(v @ (Value::Object(_) | Value::Null)) => v,
            Ok(_) => return error_response(ApiError::bad_request("the request body must be a JSON object")),
            Err(e) => return error_response(ApiError::bad_request(format!("request body: {e}"))),
        }
    };
    match run(api, op.name, args, client).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => error_response(e),
    }
}

async fn events(hub: Arc<Hub>) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = hub.subscribe();
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        let ev = match rx.recv().await {
            Ok(ev) => ev,
            Err(RecvError::Lagged(missed)) => ApiEvent::Lagged { missed },
            Err(RecvError::Closed) => return None,
        };
        let v = serde_json::to_value(&ev).unwrap_or_default();
        let name = v.get("type").and_then(Value::as_str).unwrap_or("event").to_string();
        Some((Ok(SseEvent::default().event(name).data(v.to_string())), rx))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Browsers send `Origin`: only pages served from this machine (e.g. the hub's Swagger UI) may
/// call the API, so a web page cannot drive the debugger (DNS rebinding, cross-site requests).
async fn check_origin(req: Request, next: Next) -> Response {
    if let Some(origin) = req.headers().get(header::ORIGIN) {
        let origin = origin.to_str().unwrap_or_default();
        let authority = origin.split_once("://").map_or("", |(_, a)| a).trim_end_matches('/');
        let host = authority.rsplit_once(':').filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit())).map_or(authority, |(h, _)| h);
        let same = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()) == Some(authority);
        if !(same || matches!(host, "localhost" | "127.0.0.1" | "[::1]")) {
            return error_response(ApiError::new(api_spec::ErrorCode::BadRequest, format!("requests from origin {origin} are not allowed")));
        }
    }
    next.run(req).await
}

fn method_filter(m: Method) -> MethodFilter {
    match m {
        Method::GET => MethodFilter::GET,
        Method::POST => MethodFilter::POST,
        Method::PUT => MethodFilter::PUT,
        Method::DELETE => MethodFilter::DELETE,
    }
}

pub fn router(api: Arc<CoreApi>) -> Router {
    let mut routes: BTreeMap<&str, MethodRouter> = BTreeMap::new();
    for op in OPS {
        let kinds = Arc::new(param_kinds(op));
        let api = api.clone();
        let handler = move |uri: Uri, headers: HeaderMap, Query(query): Query<Vec<(String, String)>>, body: Bytes| rest(api, op, kinds, uri, headers, query, body);
        let r = routes.remove(op.path).unwrap_or_default();
        routes.insert(op.path, r.on(method_filter(op.method), handler));
    }
    let mut router = Router::new();
    for (path, r) in routes {
        router = router.route(path, r);
    }

    let hub = api.hub.clone();
    let daq_api = api.clone();
    let svd_api = api.clone();
    let ops_api = api.clone();
    router
        .route("/api/v1/events", get(move || events(hub)))
        // A WebSocket upgrade is an ordinary GET, so it is added before `.layer(check_origin)`
        // below and gets the same origin check. That matters more here than for REST: browsers
        // exempt WebSocket from CORS entirely, so the Origin check is the only defence.
        .route("/api/v1/daq", get(move |ws| crate::daq_ws::handler(daq_api, ws)))
        // Not an operation: the definitions are tens of MB (TC4Dx: 118), streamed as they are
        // written rather than built into a JSON value, and of no use as an MCP tool.
        .route("/api/v1/svd/definitions", get(move |Query(q): Query<HashMap<String, String>>| svd_definitions(svd_api, q)))
        .route("/api/v1/ops/{op}", post(move |Path(op): Path<String>, headers: HeaderMap, body: Bytes| by_name(ops_api, op, headers, body)))
        .route("/", get(|| async { (StatusCode::OK, "aurix-dbg worker: its API is served by the hub (aurix-hub)\n") }))
        .layer(middleware::from_fn(check_origin))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_params() {
        assert_eq!(
            path_values("/api/v1/targets/{target}/registers/{name}", "/api/v1/targets/tc37x.cpu0/registers/a%5B10%5D"),
            vec![("target".to_string(), "tc37x.cpu0".to_string()), ("name".to_string(), "a[10]".to_string())]
        );
        let op = OPS.iter().find(|o| o.name == "read_memory").unwrap();
        let k = param_kinds(op);
        assert_eq!(k["length"], Kind::Int);
        assert_eq!(k["address"], Kind::Str);
        assert_eq!(coerce(&k["length"], "16".into()), Value::from(16));
        let op = OPS.iter().find(|o| o.name == "add_breakpoint").unwrap();
        assert_eq!(param_kinds(op)["targets"], Kind::List(Box::new(Kind::Str)));
    }
}
