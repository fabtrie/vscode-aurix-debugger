//! OpenAPI 3.1 document of the hub (served at `/openapi.json`, shown by Swagger UI at `/docs`):
//! the hub's operations and the session operations below `/api/v1/sessions/{session}`.

use schemars::generate::SchemaSettings;
use serde_json::{json, Map, Value};

use crate::types::{ApiEvent, HubEvent};
use crate::{all_ops, ApiError, Scope, API_DOC, HUB_PORT};

const SCHEMAS: &str = "#/components/schemas/";

fn ref_name(schema: &Value) -> Option<&str> {
    schema.get("$ref")?.as_str()?.strip_prefix(SCHEMAS)
}

/// The OpenAPI document.
pub fn openapi() -> Value {
    let mut g = SchemaSettings::draft2020_12()
        .with(|s| {
            s.definitions_path = "/components/schemas".into();
            s.meta_schema = None;
        })
        .into_generator();
    let error = g.subschema_for::<ApiError>().to_value();
    let event = g.subschema_for::<ApiEvent>().to_value();
    let hub_event = g.subschema_for::<HubEvent>().to_value();
    let schemas: Vec<(Value, Value)> = all_ops().map(|op| ((op.request)(&mut g).to_value(), (op.response)(&mut g).to_value())).collect();
    let defs = g.take_definitions(true);
    let session_param = json!({
        "name": "session", "in": "path", "required": true, "schema": { "type": "string" },
        "description": "Session ID or name (see `list_sessions`). `/api/v1/session/...` (no ID) is the only open session.",
    });

    let mut paths = Map::new();
    for (op, (req, resp)) in all_ops().zip(schemas) {
        // The request type's fields: path parameters, then query parameters or the JSON body.
        let def = ref_name(&req).and_then(|n| defs.get(n)).cloned().unwrap_or(req);
        let props = def.get("properties").and_then(Value::as_object).cloned().unwrap_or_default();
        let required: Vec<&str> = def.get("required").and_then(Value::as_array).map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
        let path_params = op.path_params();
        let mut params = Vec::new();
        if op.scope == Scope::Session {
            params.push(session_param.clone());
        }
        let mut body_props = Map::new();
        for (name, schema) in &props {
            let mut schema = schema.clone();
            let description = schema.as_object_mut().and_then(|o| o.remove("description"));
            let location = if path_params.contains(&name.as_str()) {
                "path"
            } else if op.method.uses_query() {
                "query"
            } else {
                if let (Some(o), Some(d)) = (schema.as_object_mut(), description) {
                    o.insert("description".into(), d);
                }
                body_props.insert(name.clone(), schema);
                continue;
            };
            let mut p = json!({ "name": name, "in": location, "required": location == "path" || required.contains(&name.as_str()), "schema": schema });
            if let Some(d) = description {
                p["description"] = d;
            }
            params.push(p);
        }

        let mut operation = json!({
            "operationId": op.name,
            "tags": [op.tag],
            "summary": op.summary(),
            "description": op.description(),
            "responses": {
                "200": { "description": "Success", "content": { "application/json": { "schema": resp } } },
                "default": { "description": "Error", "content": { "application/json": { "schema": error } } },
            },
        });
        if !params.is_empty() {
            operation["parameters"] = Value::Array(params);
        }
        if !body_props.is_empty() {
            let body_required: Vec<&str> = required.iter().copied().filter(|r| body_props.contains_key(*r)).collect();
            let mut body = json!({ "type": "object", "properties": body_props, "additionalProperties": false });
            if !body_required.is_empty() {
                body["required"] = json!(body_required);
            }
            if let Some(d) = def.get("description") {
                body["description"] = d.clone();
            }
            operation["requestBody"] = json!({ "required": !body_required.is_empty(), "content": { "application/json": { "schema": body } } });
        }
        let entry = paths.entry(op.hub_path()).or_insert_with(|| json!({}));
        entry[op.method.as_str().to_ascii_lowercase()] = operation;
    }

    let doc_of = |schema: &Value| ref_name(schema).and_then(|n| defs.get(n)).and_then(|d| d.get("description")).cloned().unwrap_or_default();
    paths.insert(
        "/api/v1/sessions/{session}/events".into(),
        json!({ "get": {
            "operationId": "events",
            "tags": ["events"],
            "summary": "Event stream of a session (server-sent events).",
            "description": doc_of(&event),
            "parameters": [session_param],
            "responses": { "200": { "description": "One SSE event per target event", "content": { "text/event-stream": { "schema": event } } } },
        } }),
    );
    paths.insert(
        "/api/v1/events".into(),
        json!({ "get": {
            "operationId": "hub_events",
            "tags": ["events"],
            "summary": "Event stream of the hub (server-sent events): sessions opened, launched, closed.",
            "description": doc_of(&hub_event),
            "responses": { "200": { "description": "One SSE event per hub event", "content": { "text/event-stream": { "schema": hub_event } } } },
        } }),
    );

    json!({
        "openapi": "3.1.0",
        "info": { "title": "aurix-dbg API", "version": env!("CARGO_PKG_VERSION"), "description": API_DOC },
        "servers": [{ "url": format!("http://127.0.0.1:{HUB_PORT}") }],
        "paths": paths,
        "components": { "schemas": defs },
    })
}

/// The OpenAPI document as it is written to `openapi.json`.
pub fn openapi_json() -> String {
    serde_json::to_string_pretty(&openapi()).unwrap() + "\n"
}
