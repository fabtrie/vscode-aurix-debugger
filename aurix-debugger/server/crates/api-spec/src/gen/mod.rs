//! Generators for the artifacts derived from the API definition: the OpenAPI document and the
//! Python client. The MCP tool schemas come from [`inline_schema`].

mod openapi;
mod python;

use schemars::generate::{SchemaGenerator, SchemaSettings};
use schemars::Schema;
use serde_json::Value;

pub use openapi::{openapi, openapi_json};
pub use python::python;

/// A self-contained JSON Schema (no `$ref`s) of a request or response type, as MCP tool
/// `inputSchema` / `outputSchema`.
pub fn inline_schema(f: fn(&mut SchemaGenerator) -> Schema) -> Value {
    let mut g = SchemaSettings::draft2020_12()
        .with(|s| {
            s.inline_subschemas = true;
            s.meta_schema = None;
        })
        .into_generator();
    let mut v = f(&mut g).to_value();
    if let Some(o) = v.as_object_mut() {
        o.remove("title");
    }
    v
}
