//! MCP messages (Streamable HTTP transport, JSON responses): the tools are the operations of the
//! API definition, with their documentation and schemas. Transport-neutral; the hub serves it.

use serde_json::{json, Value};

use crate::{all_ops, ApiError, OpMeta, Scope};

/// Protocol versions, newest first.
pub const VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];

/// First version with structured tool results (`outputSchema`, `structuredContent`).
pub const STRUCTURED: &str = "2025-06-18";

/// Header carrying the MCP session ID.
pub const SESSION_HEADER: &str = "mcp-session-id";

/// Header carrying the negotiated protocol version.
pub const VERSION_HEADER: &str = "mcp-protocol-version";

/// Description of the `session` argument every session operation gets as a tool.
pub const SESSION_ARG: &str = "Session ID or name (see `list_sessions`). May be left out while exactly one session is open.";

/// `read_memory` -> `Read memory`
fn title(name: &str) -> String {
    let s = name.replace('_', " ");
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// The tool definition of an operation. Session operations get the `session` argument.
pub fn tool(op: &OpMeta) -> Value {
    let mut input = crate::gen::inline_schema(op.request);
    if op.scope == Scope::Session {
        if let Some(props) = input.as_object_mut().map(|o| o.entry("properties").or_insert_with(|| json!({}))) {
            props["session"] = json!({ "type": "string", "description": SESSION_ARG });
        }
    }
    json!({
        "name": op.name,
        "title": title(op.name),
        "description": op.description(),
        "inputSchema": input,
        "outputSchema": crate::gen::inline_schema(op.response),
        "annotations": { "readOnlyHint": op.read_only(), "openWorldHint": false },
    })
}

/// All tools; without `structured` (clients before 2025-06-18) without `outputSchema`.
pub fn tools(structured: bool) -> Vec<Value> {
    all_ops()
        .map(|op| {
            let mut t = tool(op);
            if !structured {
                t.as_object_mut().unwrap().remove("outputSchema");
            }
            t
        })
        .collect()
}

/// The version to answer an `initialize` asking for `asked`.
pub fn negotiate(asked: &str) -> &'static str {
    VERSIONS.iter().copied().find(|v| *v == asked).unwrap_or(VERSIONS[0])
}

/// Whether a client speaking `version` gets structured results.
pub fn structured(version: &str) -> bool {
    version >= STRUCTURED
}

/// The result of `initialize`.
pub fn initialize(params: &Value, instructions: &str) -> Value {
    let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or_default();
    json!({
        "protocolVersion": negotiate(asked),
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "aurix-hub", "title": "aurix-dbg AURIX debugger", "version": env!("CARGO_PKG_VERSION") },
        "instructions": instructions,
    })
}

/// A tool result from an operation result.
pub fn tool_result(r: Result<Value, ApiError>, structured: bool) -> Value {
    match r {
        Ok(v) => {
            let mut out = json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }], "isError": false });
            if structured && v.is_object() {
                out["structuredContent"] = v;
            }
            out
        }
        Err(e) => json!({ "content": [{ "type": "text", "text": e.to_string() }], "isError": true }),
    }
}

/// A tool result that is guidance, not the operation's output (e.g. "choose a session"): text
/// only, since it does not match the tool's `outputSchema`, and not an error, so the model reads
/// it and retries.
pub fn guidance(v: Value) -> Value {
    json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }], "isError": false })
}

/// A JSON-RPC response.
pub fn response(id: Value, result: Result<Value, (i64, String)>) -> Value {
    match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err((code, message)) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_tools_take_a_session() {
        let all = tools(true);
        let halt = all.iter().find(|t| t["name"] == "halt").unwrap();
        assert_eq!(halt["inputSchema"]["properties"]["session"]["type"], "string");
        assert!(!halt["inputSchema"]["required"].as_array().unwrap().iter().any(|r| r == "session"));
        let open = all.iter().find(|t| t["name"] == "open_session").unwrap();
        assert!(open["inputSchema"]["properties"].get("session").is_none());
        assert!(tools(false).iter().all(|t| t.get("outputSchema").is_none()));
    }
}
