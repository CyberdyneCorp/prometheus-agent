//! TypeScript bindings for the frontends, rendered from `schema/rpc.json`
//! (spec: workspace-architecture, Contracts generated from types and checked
//! in). One `export` per schema definition, plus maps from each method,
//! server request and event name to its types, so a frontend call is typed
//! by the name it uses.

use std::fmt::Write as _;

use serde_json::{Map, Value};

/// The header of the generated file.
const HEADER: &str = "// Generated from schema/rpc.json by `cargo xtask gen`; do not edit.\n";

/// The bindings of `schema` (a [`crate::Catalog::schema`] document).
#[must_use]
pub fn render(schema: &Value) -> String {
    let mut out = String::from(HEADER);
    let protocol = schema["protocol"].as_str().unwrap_or("0.0");
    let (major, minor) = protocol.split_once('.').unwrap_or(("0", "0"));
    let _ = writeln!(
        out,
        "\nexport const PROTOCOL = {{ major: {major}, minor: {minor} }} as const;"
    );
    let _ = writeln!(
        out,
        "\nexport const ERROR_CODES = {} as const;",
        code_object(&schema["error_codes"])
    );
    let _ = writeln!(
        out,
        "\nexport const CLOSE_CODES = {} as const;",
        code_object(&schema["close_codes"])
    );
    if let Some(defs) = schema["$defs"].as_object() {
        for (name, def) in defs {
            render_def(&mut out, name, def);
        }
    }
    render_map(&mut out, "Methods", &schema["methods"], true);
    render_map(&mut out, "ServerRequests", &schema["server_requests"], true);
    render_map(&mut out, "Events", &schema["events"], false);
    render_undeclared(&mut out, schema);
    out
}

fn code_object(codes: &Value) -> String {
    let fields: Vec<String> = codes
        .as_object()
        .map(|codes| {
            codes
                .iter()
                .map(|(code, meaning)| {
                    format!("  {}: {code}", quote(meaning.as_str().unwrap_or_default()))
                })
                .collect()
        })
        .unwrap_or_default();
    format!("{{\n{},\n}}", fields.join(",\n"))
}

fn render_def(out: &mut String, name: &str, def: &Value) {
    out.push('\n');
    if let Some(doc) = def.get("description").and_then(Value::as_str) {
        let _ = writeln!(
            out,
            "/** {} */",
            doc.replace("*/", "*\\/").replace('\n', " ")
        );
    }
    if def.get("type").and_then(Value::as_str) == Some("object") && def.get("properties").is_some()
    {
        let _ = writeln!(out, "export interface {name} {}", object_type(def, 0));
    } else {
        let _ = writeln!(out, "export type {name} = {};", type_of(def, 0));
    }
}

fn render_map(out: &mut String, name: &str, section: &Value, call: bool) {
    let _ = writeln!(out, "\nexport interface {name} {{");
    for (wire, item) in section.as_object().into_iter().flatten() {
        if item.get("declared") == Some(&Value::Bool(false)) {
            continue;
        }
        if call {
            let _ = writeln!(
                out,
                "  {}: {{ params: {}; result: {} }};",
                quote(wire),
                type_of(&item["params"], 1),
                type_of(&item["result"], 1)
            );
        } else {
            let _ = writeln!(out, "  {}: {};", quote(wire), type_of(&item["payload"], 1));
        }
    }
    out.push_str("}\n");
}

/// Names indexed without a contract yet: callers know they exist.
fn render_undeclared(out: &mut String, schema: &Value) {
    let names: Vec<String> = ["methods", "server_requests", "events"]
        .iter()
        .flat_map(|section| schema[section].as_object().into_iter().flatten())
        .filter(|(_, item)| item.get("declared") == Some(&Value::Bool(false)))
        .map(|(name, _)| format!("  {},", quote(name)))
        .collect();
    let _ = writeln!(
        out,
        "\nexport const UNDECLARED = [\n{}\n] as const;",
        names.join("\n")
    );
}

fn quote(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_default()
}

fn type_of(schema: &Value, depth: usize) -> String {
    if schema == &Value::Bool(true) || schema.as_object().is_some_and(Map::is_empty) {
        return "unknown".into();
    }
    if let Some(target) = schema.get("$ref").and_then(Value::as_str) {
        return target.rsplit('/').next().unwrap_or("unknown").to_owned();
    }
    if let Some(branches) = schema
        .get("anyOf")
        .or_else(|| schema.get("oneOf"))
        .and_then(Value::as_array)
    {
        return union(branches.iter().map(|b| type_of(b, depth)));
    }
    if let Some(constant) = schema.get("const") {
        return constant.to_string();
    }
    if let Some(Value::Array(values)) = schema.get("enum") {
        return union(values.iter().map(Value::to_string));
    }
    match schema.get("type") {
        Some(Value::Array(types)) => union(types.iter().map(|t| {
            let mut single = schema.clone();
            single["type"] = t.clone();
            type_of(&single, depth)
        })),
        Some(Value::String(t)) => scalar(t, schema, depth),
        _ => "unknown".into(),
    }
}

fn scalar(name: &str, schema: &Value, depth: usize) -> String {
    match name {
        "string" => "string".into(),
        "integer" | "number" => "number".into(),
        "boolean" => "boolean".into(),
        "null" => "null".into(),
        "array" => match schema.get("items") {
            Some(items) => format!("Array<{}>", type_of(items, depth)),
            None => "unknown[]".into(),
        },
        "object" if schema.get("properties").is_some() => object_type(schema, depth),
        "object" => match schema.get("additionalProperties") {
            Some(extra @ Value::Object(_)) => format!("Record<string, {}>", type_of(extra, depth)),
            _ => "Record<string, unknown>".into(),
        },
        _ => "unknown".into(),
    }
}

fn object_type(schema: &Value, depth: usize) -> String {
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let indent = "  ".repeat(depth + 1);
    let mut out = String::from("{\n");
    for (field, field_schema) in schema["properties"].as_object().into_iter().flatten() {
        if let Some(doc) = field_schema.get("description").and_then(Value::as_str) {
            let _ = writeln!(
                out,
                "{indent}/** {} */",
                doc.replace("*/", "*\\/").replace('\n', " ")
            );
        }
        let optional = if required.contains(&field.as_str()) {
            ""
        } else {
            "?"
        };
        let _ = writeln!(
            out,
            "{indent}{field}{optional}: {};",
            type_of(field_schema, depth + 1)
        );
    }
    let _ = write!(out, "{}}}", "  ".repeat(depth));
    out
}

fn union(parts: impl Iterator<Item = String>) -> String {
    let mut parts: Vec<String> = parts.collect();
    parts.dedup();
    parts.join(" | ")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::render;
    use crate::Catalog;

    #[test]
    fn bindings_type_each_name_by_its_contract() {
        let ts = render(&Catalog::builtin().schema());
        assert!(ts.contains("export interface HelloParams {"), "{ts}");
        assert!(ts.contains("\"prompt.submit\": { params: SubmitParams; result: Submitted };"));
        assert!(ts.contains("\"request.cancel\": RequestCancel;"));
        assert!(
            ts.contains("\"worktree.new\","),
            "undeclared names are listed"
        );
        assert!(ts.contains("export const PROTOCOL = { major: 1, minor: 0 } as const;"));
    }

    #[test]
    fn optional_and_nullable_fields_render_as_such() {
        let schema = json!({
            "protocol": "1.0",
            "$defs": {"A": {"type": "object", "properties": {
                "x": {"type": "string"},
                "y": {"type": ["string", "null"]},
                "z": {"type": "array", "items": {"type": "integer"}},
                "w": {"enum": ["a", "b"]}
            }, "required": ["x", "y"]}},
            "methods": {}, "server_requests": {}, "events": {}
        });
        let ts = render(&schema);
        assert!(
            ts.contains("  x: string;\n  y: string | null;\n  z?: Array<number>;"),
            "{ts}"
        );
        assert!(ts.contains("w?: \"a\" | \"b\";"), "{ts}");
    }
}
