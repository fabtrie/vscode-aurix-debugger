//! The aurix-dbg REST/MCP API definition - the single source for the REST routes, the MCP tools,
//! the OpenAPI document (Swagger), the Rust traits the worker and the hub implement and the
//! Python client.
//!
//! Operations are declared once with the [`api!`] macro: the session operations a worker serves
//! in `ops`, the hub's own in `hub_ops`. Their `///` comments and the `///` comments of the
//! request and response types in [`types`] are the documentation of all of them. The generators
//! in [`gen`] run in the build script of the `api` crate; [`mcp`] is the MCP message handling the
//! hub uses.

#[macro_use]
mod macros;
mod error;
pub mod gen;
mod hub_ops;
pub mod mcp;
mod ops;
pub mod types;

use schemars::generate::SchemaGenerator;
use schemars::Schema;

pub use error::{ApiError, ErrorCode};
pub use hub_ops::{hub_dispatch, HubApi, HUB_OPS};
pub use ops::{dispatch, DebuggerApi, OPS};

/// Overall API description (OpenAPI `info.description`, MCP server instructions).
pub const API_DOC: &str = "\
Control and inspect AURIX targets through the aurix-dbg hub: open debug sessions on boards, then \
run control, registers, memory, variables by name, breakpoints, watchpoints, flash programming \
and the OpenOCD-compatible Tcl console.

A session is one board connection (one worker process). `list_sessions` shows the open ones; \
`open_session` starts one from a profile (`list_profiles`) or configuration scripts and can run \
the launch steps of a VS Code launch configuration: symbols, flash, reset, breakpoints, run to \
a function. The session operations take a `session` (ID or name); it can be left out while \
exactly one session is open.

Every client shares a session with the others: actions taken here are reflected in a connected \
VS Code debug session (TCF) and the other way round, e.g. a breakpoint set here shows up there \
and a halt from VS Code is seen in `wait_state` and the event stream. A client that needs the \
target to itself takes the control lease (`acquire_control`).

Targets are named as in the OpenOCD configuration (e.g. `tc37x.cpu0`); `targets` lists them. \
Addresses accept numbers or strings in decimal or `0x` hex; memory contents are hex strings.";

/// Revision of the API reported by `info`, raised whenever an operation or a field is added or
/// changed. 1: `info`, `list_globals`, `symbol_children`, `evaluate`, `write_variable`. 2: `@N`
/// in expressions for core-local RAM, `per_core` groups, `load_symbols` `path@N`,
/// `local_cores` in `info`. 3: `flash_write` files with `offset` and `type`. 4: watchpoints in
/// the shared session (string IDs, `owner`, `instances`), on an `expression`, with a `condition`.
/// 5: the hub (sessions, profiles), `launch`, the control lease, clients (`client`,
/// `end_client`).
pub const API_VERSION: u32 = 5;

/// Largest memory transfer of one `read_memory` / `write_memory` call in bytes. One call occupies
/// the debug core while it runs; larger transfers are split by the client.
pub const MAX_TRANSFER: u32 = 65536;

/// HTTP method of an operation.
#[allow(clippy::upper_case_acronyms)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Method {
    GET,
    POST,
    PUT,
    DELETE,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::GET => "GET",
            Method::POST => "POST",
            Method::PUT => "PUT",
            Method::DELETE => "DELETE",
        }
    }

    /// Whether the arguments are sent as query parameters (else as JSON body).
    pub fn uses_query(self) -> bool {
        matches!(self, Method::GET | Method::DELETE)
    }
}

/// Who serves an operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// One board: served by its worker, at `/api/v1/sessions/{session}/...` on the hub.
    Session,
    /// The hub itself.
    Hub,
}

/// One operation of the API.
pub struct OpMeta {
    /// Operation ID, MCP tool name and Rust/Python method name.
    pub name: &'static str,
    /// Group (OpenAPI tag).
    pub tag: &'static str,
    pub scope: Scope,
    pub method: Method,
    /// Route with `{param}` path parameters; they are fields of the request type. For a session
    /// operation this is the worker's route; see [`OpMeta::hub_path`].
    pub path: &'static str,
    /// The `///` documentation, one line per doc attribute.
    pub doc: &'static str,
    pub request: fn(&mut SchemaGenerator) -> Schema,
    pub response: fn(&mut SchemaGenerator) -> Schema,
}

impl OpMeta {
    /// The documentation without the leading space `///` leaves on every line.
    pub fn description(&self) -> String {
        let lines: Vec<&str> = self.doc.lines().map(|l| l.strip_prefix(' ').unwrap_or(l)).collect();
        lines.join("\n").trim().to_string()
    }

    /// The first sentence (or line) of the documentation.
    pub fn summary(&self) -> String {
        let d = self.description();
        let first = d.split("\n\n").next().unwrap_or_default().replace('\n', " ");
        match first.find(". ") {
            Some(i) => first[..=i].to_string(),
            None => first,
        }
    }

    pub fn path_params(&self) -> Vec<&'static str> {
        self.path.split('/').filter_map(|s| s.strip_prefix('{').and_then(|s| s.strip_suffix('}'))).collect()
    }

    /// Operations that only read (MCP `readOnlyHint`).
    pub fn read_only(&self) -> bool {
        self.method == Method::GET
    }

    /// The route on the hub: a session operation's worker route below
    /// `/api/v1/sessions/{session}`, a hub operation's own.
    pub fn hub_path(&self) -> String {
        match self.scope {
            Scope::Hub => self.path.to_string(),
            Scope::Session => format!("{SESSION_PREFIX}{{session}}{}", self.sub_path()),
        }
    }

    /// The route below `/api/v1`, e.g. `/targets/{target}`.
    pub fn sub_path(&self) -> &'static str {
        self.path.strip_prefix(API_PREFIX).unwrap_or(self.path)
    }
}

/// Prefix of every route.
pub const API_PREFIX: &str = "/api/v1";
/// Prefix of a session's routes on the hub, followed by the session ID or name.
pub const SESSION_PREFIX: &str = "/api/v1/sessions/";
/// Prefix of the routes of the only open session on the hub.
pub const SINGLE_SESSION_PREFIX: &str = "/api/v1/session";
/// Header naming the API client (owner of its breakpoints, holder of the lease).
pub const CLIENT_HEADER: &str = "x-aurix-client";
/// Port of the hub.
pub const HUB_PORT: u16 = 3001;

/// The hub's operations, then the session operations.
pub fn all_ops() -> impl Iterator<Item = &'static OpMeta> {
    HUB_OPS.iter().chain(OPS.iter())
}

/// An operation by name.
pub fn find_op(name: &str) -> Option<&'static OpMeta> {
    all_ops().find(|o| o.name == name)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use serde_json::Value;

    use super::*;

    fn refs<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
        match v {
            Value::Object(m) => {
                if let Some(Value::String(r)) = m.get("$ref") {
                    out.push(r);
                }
                m.values().for_each(|x| refs(x, out));
            }
            Value::Array(a) => a.iter().for_each(|x| refs(x, out)),
            _ => {}
        }
    }

    #[test]
    fn operations_are_consistent() {
        let mut names = HashSet::new();
        let mut routes = HashSet::new();
        for op in all_ops() {
            assert!(names.insert(op.name), "duplicate operation {}", op.name);
            assert!(routes.insert((op.method, op.hub_path())), "duplicate route {} {}", op.method.as_str(), op.hub_path());
            assert!(op.path.starts_with(API_PREFIX), "{}: route outside {API_PREFIX}", op.name);
            assert!(!op.summary().is_empty(), "{} is not documented", op.name);
            // Path parameters are required string fields of the request.
            let req = gen::inline_schema(op.request);
            let required: Vec<&str> = req["required"].as_array().map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            for p in op.path_params() {
                assert!(required.contains(&p), "{}: path parameter {p} is not a required field", op.name);
                assert_eq!(req["properties"][p]["type"], "string", "{}: path parameter {p}", op.name);
            }
            // MCP needs object schemas for tool input and output.
            assert_eq!(req["type"], "object", "{} request", op.name);
            assert_eq!(gen::inline_schema(op.response)["type"], "object", "{} response", op.name);
        }
    }

    #[test]
    fn openapi_refs_resolve() {
        let doc = gen::openapi();
        let mut all = Vec::new();
        refs(&doc, &mut all);
        assert!(!all.is_empty());
        for r in all {
            let name = r.strip_prefix("#/components/schemas/").unwrap_or_else(|| panic!("unexpected $ref {r}"));
            assert!(doc["components"]["schemas"].get(name).is_some(), "unresolved $ref {r}");
        }
        // Every operation, plus the hub's and a session's event stream.
        assert_eq!(doc["paths"].as_object().unwrap().values().map(|p| p.as_object().unwrap().len()).sum::<usize>(), OPS.len() + HUB_OPS.len() + 2);
    }

    #[test]
    fn python_has_every_operation() {
        let py = gen::python();
        for op in all_ops() {
            assert!(py.contains(&format!("    def {}(self", op.name)), "{} missing", op.name);
        }
    }
}
