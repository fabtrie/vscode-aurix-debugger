//! The MCP front door (`/mcp`, Streamable HTTP with JSON responses).
//!
//! `initialize` opens an MCP session (`Mcp-Session-Id`); its tool calls run as client
//! `mcp:<id>`, which owns the breakpoints it sets and the lease it takes. `DELETE /mcp`, or no
//! request for `mcp_idle_min`, ends it: the hub removes its breakpoints on every worker. Session
//! operations take a `session` argument, optional while exactly one session is open; otherwise
//! the result is guidance listing the choices (not an error, so the model retries).

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use api_spec::mcp as m;
use api_spec::{ApiError, ErrorCode, Scope, API_DOC};

use crate::state::{with_client, Choose, Hub, Target};
use crate::{files, hlog};

/// Timeout of a forwarded tool call: launches and flashing take long; `wait_s` bounds them.
const CALL_TIMEOUT: Duration = Duration::from_secs(3700);

struct Ctx {
    hub: Arc<Hub>,
    /// `mcp:<id>`, or `mcp` for a client without a session.
    client: String,
    structured: bool,
    /// The request comes from this machine (Tcl may be refused for others).
    local: bool,
}

/// Runs one tool call on a blocking thread.
fn call_tool(ctx: &Ctx, params: &Value) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
    let op = api_spec::find_op(name).ok_or_else(|| (-32602, format!("Unknown tool: {name}")))?;
    let mut args = params.get("arguments").cloned().unwrap_or(json!({}));
    if args.is_null() {
        args = json!({});
    }
    if !args.is_object() {
        return Err((-32602, "arguments must be an object".into()));
    }
    let structured = ctx.structured;
    let hub = &ctx.hub;
    let result = with_client(Some(ctx.client.clone()), || -> Result<Value, Choose> {
        if op.name == "tcl" && !ctx.local && !hub.cfg.allow_tcl {
            return Err(Choose::Error(ApiError::bad_request("Tcl commands from other machines are switched off on this hub (allow_tcl = false)")));
        }
        files::resolve_ids(&mut args).map_err(|e| Choose::Error(ApiError::bad_request(e)))?;
        match op.scope {
            Scope::Hub => api_spec::hub_dispatch(&**hub, op.name, args).map_err(Choose::Error),
            Scope::Session => {
                let key = args.as_object_mut().and_then(|o| o.remove("session")).and_then(|v| v.as_str().map(str::to_string));
                if op.name == "wait_session" {
                    let timeout = args.get("timeout_s").and_then(Value::as_u64).map(|t| t as u32);
                    return hub.wait_session(key.as_deref(), timeout);
                }
                match hub.choose(key.as_deref())? {
                    Target::Worker { base, .. } => hub.forward(&base, op.name, &args, CALL_TIMEOUT).map_err(Choose::Error),
                }
            }
        }
    });
    Ok(match result {
        Ok(v) => m::tool_result(Ok(v), structured),
        Err(Choose::Error(e)) => m::tool_result(Err(e), structured),
        Err(Choose::Guidance(g)) => m::guidance(g),
    })
}

async fn message(ctx: Arc<Ctx>, msg: Value) -> Option<Value> {
    let method = msg.get("method").and_then(Value::as_str)?.to_string();
    let id = msg.get("id")?.clone();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let result = match method.as_str() {
        "initialize" => Ok(m::initialize(&params, API_DOC)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": m::tools(ctx.structured) })),
        "tools/call" => {
            let c = ctx.clone();
            tokio::task::spawn_blocking(move || call_tool(&c, &params)).await.unwrap_or_else(|e| Err((-32603, format!("tool call failed: {e}"))))
        }
        other => Err((-32601, format!("Method not found: {other}"))),
    };
    Some(m::response(id, result))
}

/// `POST /mcp`.
pub async fn post(hub: Arc<Hub>, local: bool, headers: HeaderMap, body: Bytes) -> Response {
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return Json(json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": format!("Parse error: {e}") } })).into_response(),
    };
    let is_init = |v: &Value| v.get("method").and_then(Value::as_str) == Some("initialize");
    let init = match &msg {
        Value::Array(a) => a.iter().any(is_init),
        v => is_init(v),
    };
    let header_id = headers.get(m::SESSION_HEADER).and_then(|v| v.to_str().ok()).map(str::to_string);
    // `initialize` opens a session; a later request names it.
    let (session, new) = if init {
        let id = crate::config::random_hex(16);
        hub.lock().mcp.insert(id.clone(), crate::state::McpClient { last_seen: Instant::now() });
        hlog!("MCP session {id} opened");
        (Some(id), true)
    } else {
        match header_id {
            Some(id) => {
                let mut st = hub.lock();
                match st.mcp.get_mut(&id) {
                    Some(c) => c.last_seen = Instant::now(),
                    // An ended (or unknown) session: the client must initialize again.
                    None => return (StatusCode::NOT_FOUND, Json(json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32001, "message": "unknown MCP session; initialize again" } }))).into_response(),
                }
                (Some(id), false)
            }
            None => (None, false),
        }
    };
    let version = headers.get(m::VERSION_HEADER).and_then(|v| v.to_str().ok()).unwrap_or("2025-03-26");
    let ctx = Arc::new(Ctx {
        hub: hub.clone(),
        client: session.as_ref().map_or_else(|| "mcp".to_string(), |id| format!("mcp:{id}")),
        structured: m::structured(version),
        local,
    });
    let replies = match msg {
        Value::Array(items) => {
            let mut out = Vec::new();
            for it in items {
                out.extend(message(ctx.clone(), it).await);
            }
            if out.is_empty() {
                return StatusCode::ACCEPTED.into_response();
            }
            Value::Array(out)
        }
        it => match message(ctx, it).await {
            Some(r) => r,
            None => return StatusCode::ACCEPTED.into_response(),
        },
    };
    let mut resp = Json(replies).into_response();
    if let (true, Some(id)) = (new, &session) {
        if let Ok(v) = HeaderValue::from_str(id) {
            resp.headers_mut().insert(m::SESSION_HEADER, v);
        }
    }
    resp
}

/// `DELETE /mcp`: the client ends its session.
pub async fn delete(hub: Arc<Hub>, headers: HeaderMap) -> Response {
    let Some(id) = headers.get(m::SESSION_HEADER).and_then(|v| v.to_str().ok()).map(str::to_string) else {
        return (StatusCode::BAD_REQUEST, "no Mcp-Session-Id").into_response();
    };
    let ended = tokio::task::spawn_blocking(move || {
        let r = hub.end_mcp(&id);
        if r {
            hlog!("MCP session {id} ended by the client");
        }
        r
    })
    .await
    .unwrap_or(false);
    if ended {
        StatusCode::NO_CONTENT.into_response()
    } else {
        StatusCode::NOT_FOUND.into_response()
    }
}

/// The error code of a guidance-free failure, for REST.
pub fn choose_error(c: Choose) -> (ApiError, Option<Value>) {
    match c {
        Choose::Error(e) => (e, None),
        Choose::Guidance(g) => {
            let code = if g["status"] == "no_session" { ErrorCode::NotFound } else { ErrorCode::Busy };
            (ApiError::new(code, g["message"].as_str().unwrap_or_default()), Some(g))
        }
    }
}
