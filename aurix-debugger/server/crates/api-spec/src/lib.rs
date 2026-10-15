//! The aurix-dbg REST/MCP API definition - the single source for the REST routes, the MCP tools,
//! the OpenAPI document (Swagger), the Rust trait the server implements and the Python client.
//!
//! Operations are declared once in [`ops`] with the [`api!`] macro; their `///` comments and the
//! `///` comments of the request and response types in [`types`] are the documentation of all
//! of them. The generators in [`gen`] run in the build script of the `api` crate.

#[macro_use]
mod macros;
mod error;
pub mod gen;
mod ops;
pub mod types;

use schemars::generate::SchemaGenerator;
use schemars::Schema;

pub use error::{ApiError, ErrorCode};
pub use ops::{dispatch, DebuggerApi, OPS};

/// Overall API description (OpenAPI `info.description`, MCP server instructions).
pub const API_DOC: &str = "\
Control and inspect AURIX targets through the aurix-dbg debug server: run control, registers, \
memory, breakpoints, watchpoints, flash programming and the OpenOCD-compatible Tcl console.

The API shares its session with the other frontends: actions taken here are reflected in a \
connected VS Code debug session (TCF) and the other way round, e.g. a breakpoint set here shows \
up there and a halt from VS Code is seen in `wait_state` and the event stream.

Targets are named as in the OpenOCD configuration (e.g. `tc37x.cpu0`); `targets` lists them. \
Addresses accept numbers or strings in decimal or `0x` hex; memory contents are hex strings.";

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

/// One operation of the API.
pub struct OpMeta {
    /// Operation ID, MCP tool name and Rust/Python method name.
    pub name: &'static str,
    /// Group (OpenAPI tag).
    pub tag: &'static str,
    pub method: Method,
    /// Route with `{param}` path parameters; they are fields of the request type.
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
        for op in OPS {
            assert!(names.insert(op.name), "duplicate operation {}", op.name);
            assert!(routes.insert((op.method, op.path)), "duplicate route {} {}", op.method.as_str(), op.path);
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
        assert_eq!(doc["paths"].as_object().unwrap().values().map(|p| p.as_object().unwrap().len()).sum::<usize>(), OPS.len() + 1);
    }

    #[test]
    fn python_has_every_operation() {
        let py = gen::python();
        for op in OPS {
            assert!(py.contains(&format!("    def {}(self", op.name)), "{} missing", op.name);
        }
    }
}
