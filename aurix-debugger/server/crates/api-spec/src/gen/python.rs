//! Python client (`python/aurix_dbg/_generated.py`): one dataclass per response type and one
//! method per operation, documented with the API documentation.

use std::fmt::Write;

use schemars::generate::{SchemaGenerator, SchemaSettings};
use schemars::Schema;
use serde_json::{Map, Value};

use super::openapi::openapi;
use crate::{API_DOC, MAX_TRANSFER, OPS};

const SCHEMAS: &str = "#/components/schemas/";
const KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else",
    "except", "finally", "for", "from", "global", "if", "import", "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise",
    "return", "try", "while", "with", "yield",
];

fn ident(name: &str) -> String {
    if KEYWORDS.contains(&name) {
        format!("{name}_")
    } else {
        name.to_string()
    }
}

struct Gen<'a> {
    defs: &'a Map<String, Value>,
}

/// A string enum: its values.
fn enum_values(s: &Value) -> Option<Vec<String>> {
    if let Some(e) = s.get("enum").and_then(Value::as_array) {
        return e.iter().map(|v| v.as_str().map(str::to_string)).collect();
    }
    // Variants with documentation are `const` alternatives, the others one `enum` alternative.
    let mut vals = Vec::new();
    for alt in s.get("oneOf").and_then(Value::as_array)? {
        match (alt.get("const"), alt.get("enum").and_then(Value::as_array)) {
            (Some(c), _) => vals.push(c.as_str()?.to_string()),
            (None, Some(e)) => vals.extend(e.iter().map(|v| v.as_str().map(str::to_string)).collect::<Option<Vec<_>>>()?),
            _ => return None,
        }
    }
    Some(vals)
}

fn is_class(def: &Value) -> bool {
    def.get("type").and_then(Value::as_str) == Some("object") && def.get("properties").is_some()
}

impl Gen<'_> {
    fn def(&self, s: &Value) -> Option<(String, &Value)> {
        let name = s.get("$ref")?.as_str()?.strip_prefix(SCHEMAS)?;
        Some((name.to_string(), self.defs.get(name)?))
    }

    /// Python type of a schema; optional (nullable) types get `| None`.
    fn ty(&self, s: &Value) -> String {
        if let Some((name, def)) = self.def(s) {
            return if is_class(def) || enum_values(def).is_some() { name.to_string() } else { self.ty(def) };
        }
        if let Some(alts) = s.get("anyOf").or_else(|| s.get("oneOf")).and_then(Value::as_array) {
            if let Some(vals) = enum_values(s) {
                return literal(&vals);
            }
            let mut parts: Vec<String> = alts.iter().map(|a| self.ty(a)).collect();
            parts.dedup();
            return parts.join(" | ");
        }
        if let Some(vals) = enum_values(s) {
            return literal(&vals);
        }
        let one = |t: &str| -> String {
            match t {
                "integer" => "int".into(),
                "number" => "float".into(),
                "boolean" => "bool".into(),
                "string" => "str".into(),
                "null" => "None".into(),
                "array" => format!("list[{}]", s.get("items").map_or("Any".into(), |i| self.ty(i))),
                _ => "dict[str, Any]".into(),
            }
        };
        match s.get("type") {
            Some(Value::String(t)) => one(t),
            Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).map(one).collect::<Vec<_>>().join(" | "),
            _ => "Any".into(),
        }
    }

    /// Expression converting the JSON value `v` of a response field to its Python type.
    fn convert(&self, s: &Value, v: &str) -> String {
        if let Some((name, def)) = self.def(s) {
            return if is_class(def) { format!("_obj({name}, {v})") } else { v.to_string() };
        }
        if let Some(alts) = s.get("anyOf").and_then(Value::as_array) {
            let real: Vec<&Value> = alts.iter().filter(|a| a.get("type").and_then(Value::as_str) != Some("null")).collect();
            if real.len() == 1 {
                return self.convert(real[0], v);
            }
        }
        if s.get("type").and_then(Value::as_str) == Some("array") {
            if let Some((name, def)) = s.get("items").and_then(|i| self.def(i)) {
                if is_class(def) {
                    return format!("_list({name}, {v})");
                }
            }
        }
        v.to_string()
    }
}

/// The `$ref` schema of a request or response type.
fn ref_schema(f: fn(&mut SchemaGenerator) -> Schema) -> Value {
    let mut g = SchemaSettings::draft2020_12().with(|s| s.definitions_path = "/components/schemas".into()).into_generator();
    f(&mut g).to_value()
}

fn literal(vals: &[String]) -> String {
    format!("Literal[{}]", vals.iter().map(|v| format!("{v:?}")).collect::<Vec<_>>().join(", "))
}

/// Docstring body lines, indented.
fn doc(text: &str, indent: &str) -> String {
    text.lines().map(|l| if l.is_empty() { String::new() } else { format!("{indent}{}", l.replace('\\', "\\\\")) }).collect::<Vec<_>>().join("\n")
}

fn props_of(def: &Value) -> (Map<String, Value>, Vec<String>) {
    let props = def.get("properties").and_then(Value::as_object).cloned().unwrap_or_default();
    let req = def.get("required").and_then(Value::as_array).map(|r| r.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
    (props, req)
}

/// The generated module.
pub fn python() -> String {
    let api = openapi();
    let defs = api["components"]["schemas"].as_object().cloned().unwrap_or_default();
    let g = Gen { defs: &defs };
    let mut out = String::new();
    let w = &mut out;

    writeln!(w, "# Generated from the aurix-dbg API definition (server/crates/api-spec) by `cargo build`. Do not edit.").unwrap();
    writeln!(w, "\"\"\"Generated part of the aurix-dbg client: response types and API methods.\n").unwrap();
    writeln!(w, "{}\n\"\"\"", doc(API_DOC, "")).unwrap();
    writeln!(w, "\nfrom __future__ import annotations\n").unwrap();
    writeln!(w, "from dataclasses import dataclass").unwrap();
    writeln!(w, "from typing import Any, Literal").unwrap();
    writeln!(w, "from urllib.parse import quote\n").unwrap();
    writeln!(w, "API_VERSION = {:?}", env!("CARGO_PKG_VERSION")).unwrap();
    writeln!(w, "MAX_TRANSFER = {MAX_TRANSFER}").unwrap();
    writeln!(w, "\"\"\"Largest memory transfer of one read_memory / write_memory call in bytes.\"\"\"\n").unwrap();
    writeln!(w, "\ndef _obj(cls, v):\n    return None if v is None else cls.from_dict(v)\n").unwrap();
    writeln!(w, "\ndef _list(cls, v):\n    return None if v is None else [cls.from_dict(x) for x in v]\n").unwrap();
    writeln!(w, "\ndef _args(d):\n    return {{k: v for k, v in d.items() if v is not None}}\n").unwrap();

    // Enums, as Literal aliases.
    for (name, def) in &defs {
        if let Some(vals) = enum_values(def) {
            writeln!(w, "\n{name} = {}", literal(&vals)).unwrap();
            if let Some(d) = def.get("description").and_then(Value::as_str) {
                writeln!(w, "\"\"\"{}\"\"\"", doc(d, "")).unwrap();
            }
        }
    }

    // Response (and other object) types as dataclasses; the request types become method
    // arguments instead.
    let requests: Vec<String> = OPS.iter().filter_map(|op| g.def(&ref_schema(op.request)).map(|(n, _)| n.to_string())).collect();
    for (name, def) in &defs {
        if !is_class(def) || requests.contains(name) {
            continue;
        }
        let (props, required) = props_of(def);
        writeln!(w, "\n\n@dataclass\nclass {name}:").unwrap();
        if let Some(d) = def.get("description").and_then(Value::as_str) {
            writeln!(w, "    \"\"\"{}\"\"\"\n", doc(d, "    ").trim_start()).unwrap();
        }
        // Required fields first: dataclass fields with defaults must follow those without.
        let mut fields: Vec<(&String, &Value)> = props.iter().collect();
        fields.sort_by_key(|(k, _)| !required.contains(k));
        for (fname, fs) in &fields {
            let opt = !required.contains(fname);
            let ty = g.ty(fs);
            let ty = if opt && !ty.contains("None") { format!("{ty} | None") } else { ty };
            writeln!(w, "    {}: {ty}{}", ident(fname), if opt { " = None" } else { "" }).unwrap();
            if let Some(d) = fs.get("description").and_then(Value::as_str) {
                writeln!(w, "    \"\"\"{}\"\"\"", doc(d, "    ").trim_start()).unwrap();
            }
        }
        writeln!(w, "\n    @classmethod\n    def from_dict(cls, d: dict[str, Any]) -> {name}:").unwrap();
        writeln!(w, "        return cls(").unwrap();
        for (fname, fs) in &fields {
            writeln!(w, "            {}={},", ident(fname), g.convert(fs, &format!("d.get({fname:?})"))).unwrap();
        }
        writeln!(w, "        )").unwrap();
    }

    // The client methods.
    writeln!(w, "\n\nclass GeneratedClient:").unwrap();
    writeln!(w, "    \"\"\"The API operations; a subclass provides the transport (`_call`).\"\"\"\n").unwrap();
    writeln!(w, "    def _call(self, method: str, path: str, query: dict[str, Any] | None, body: dict[str, Any] | None) -> Any:").unwrap();
    writeln!(w, "        raise NotImplementedError").unwrap();
    for op in OPS {
        let req = ref_schema(op.request);
        let resp = ref_schema(op.response);
        let def = g.def(&req).map(|(_, d)| d.clone()).unwrap_or(req);
        let (props, required) = props_of(&def);
        let mut args: Vec<(&String, &Value)> = props.iter().collect();
        // Path parameters first (in route order), then required, then optional arguments.
        let pp = op.path_params();
        args.sort_by_key(|(k, _)| (pp.iter().position(|p| p == k).unwrap_or(pp.len()), !required.contains(k)));
        // Path parameters are positional, the other arguments keyword-only.
        let mut sig = vec!["self".to_string()];
        for (i, (a, s)) in args.iter().enumerate() {
            if i == pp.len() {
                sig.push("*".into());
            }
            let ty = g.ty(s);
            if required.contains(a) {
                sig.push(format!("{}: {ty}", ident(a)));
            } else {
                let ty = if ty.contains("None") { ty } else { format!("{ty} | None") };
                sig.push(format!("{}: {ty} = None", ident(a)));
            }
        }
        let (rname, _) = g.def(&resp).expect("responses are named types");
        writeln!(w, "\n    def {}({}) -> {rname}:", op.name, sig.join(", ")).unwrap();
        let mut docstr = op.description();
        if !args.is_empty() {
            docstr.push_str("\n\nArgs:");
            for (a, s) in &args {
                let d = s.get("description").and_then(Value::as_str).unwrap_or_default().replace('\n', " ");
                write!(docstr, "\n    {}: {d}", ident(a)).unwrap();
            }
        }
        writeln!(w, "        \"\"\"{}\n        \"\"\"", doc(&docstr, "        ").trim_start()).unwrap();
        let mut path = if pp.is_empty() { format!("{:?}", op.path) } else { format!("f{:?}", op.path) };
        for p in &pp {
            path = path.replace(&format!("{{{p}}}"), &format!("{{quote(str({}), safe='')}}", ident(p)));
        }
        let rest: Vec<&(&String, &Value)> = args.iter().filter(|(a, _)| !pp.contains(&a.as_str())).collect();
        let dict = format!("_args({{{}}})", rest.iter().map(|(a, _)| format!("{a:?}: {}", ident(a))).collect::<Vec<_>>().join(", "));
        let (query, body) = if rest.is_empty() {
            ("None".to_string(), "None".to_string())
        } else if op.method.uses_query() {
            (dict, "None".to_string())
        } else {
            ("None".to_string(), dict)
        };
        writeln!(w, "        return {rname}.from_dict(self._call({:?}, {path}, {query}, {body}))", op.method.as_str()).unwrap();
    }
    out
}
