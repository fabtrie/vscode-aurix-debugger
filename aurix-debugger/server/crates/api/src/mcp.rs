//! MCP server (Streamable HTTP transport, stateless, JSON responses): the tools are the
//! operations of the API definition, with their documentation and schemas.

use std::sync::Arc;

use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use api_spec::{API_DOC, OPS};

use crate::core_impl::CoreApi;
use crate::http::run;

/// Protocol versions, newest first.
const VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// First version with structured tool results (`outputSchema`, `structuredContent`).
const STRUCTURED: &str = "2025-06-18";

pub struct Mcp {
    tools: Vec<Value>,
}

/// `read_memory` -> `Read memory`
fn title(name: &str) -> String {
    let s = name.replace('_', " ");
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

impl Mcp {
    pub fn new() -> Mcp {
        let tools = OPS
            .iter()
            .map(|op| {
                json!({
                    "name": op.name,
                    "title": title(op.name),
                    "description": op.description(),
                    "inputSchema": api_spec::gen::inline_schema(op.request),
                    "outputSchema": api_spec::gen::inline_schema(op.response),
                    "annotations": { "readOnlyHint": op.read_only(), "openWorldHint": false },
                })
            })
            .collect();
        Mcp { tools }
    }

    fn tools(&self, structured: bool) -> Vec<Value> {
        if structured {
            return self.tools.clone();
        }
        self.tools
            .iter()
            .map(|t| {
                let mut t = t.clone();
                t.as_object_mut().unwrap().remove("outputSchema");
                t
            })
            .collect()
    }
}

fn initialize(params: &Value) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
    let version = VERSIONS.iter().copied().find(|v| *v == asked).unwrap_or(VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "aurix-dbg", "title": "aurix-dbg AURIX debugger", "version": env!("CARGO_PKG_VERSION") },
        "instructions": API_DOC,
    })
}

async fn call_tool(api: Arc<CoreApi>, params: &Value, structured: bool) -> Result<Value, (i64, String)> {
    let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
    let op = OPS.iter().find(|o| o.name == name).ok_or_else(|| (-32602, format!("Unknown tool: {name}")))?;
    let args = params.get("arguments").cloned().unwrap_or(Value::Null);
    Ok(match run(api, op.name, args).await {
        Ok(v) => {
            let mut r = json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }], "isError": false });
            if structured {
                r["structuredContent"] = v;
            }
            r
        }
        Err(e) => json!({ "content": [{ "type": "text", "text": e.to_string() }], "isError": true }),
    })
}

/// One JSON-RPC message; `None` for notifications and responses (nothing to answer).
async fn message(api: Arc<CoreApi>, mcp: &Mcp, structured: bool, msg: Value) -> Option<Value> {
    let method = msg.get("method").and_then(Value::as_str)?;
    let id = msg.get("id")?.clone();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => Ok(initialize(&params)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": mcp.tools(structured) })),
        "tools/call" => call_tool(api, &params, structured).await,
        other => Err((-32601, format!("Method not found: {other}"))),
    };
    Some(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err((code, message)) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
    })
}

pub async fn handle(api: Arc<CoreApi>, mcp: Arc<Mcp>, headers: HeaderMap, body: Bytes) -> Response {
    // Clients send the negotiated version with every request after `initialize`.
    let version = headers.get("mcp-protocol-version").and_then(|v| v.to_str().ok()).unwrap_or("2025-03-26");
    let structured = version >= STRUCTURED;
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return Json(json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": format!("Parse error: {e}") } })).into_response(),
    };
    let replies = match msg {
        Value::Array(items) => {
            let mut out = Vec::new();
            for m in items {
                out.extend(message(api.clone(), &mcp, structured, m).await);
            }
            if out.is_empty() {
                return StatusCode::ACCEPTED.into_response();
            }
            Value::Array(out)
        }
        m => match message(api, &mcp, structured, m).await {
            Some(r) => r,
            None => return StatusCode::ACCEPTED.into_response(),
        },
    };
    Json(replies).into_response()
}
