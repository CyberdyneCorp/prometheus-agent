//! The generated client-protocol reference page, rendered from
//! `schema/rpc.json` (spec: workspace-architecture, Generated reference
//! documentation): every method, server request and event with its owning
//! capability, the minor version that added it and its contract, then
//! every contract type with its fields. Like the TypeScript bindings it is
//! a pure function of the schema, so it cannot drift from the daemon.

use std::fmt::Write as _;

use serde_json::{Map, Value};

/// The page for `schema` (a [`crate::Catalog::schema`] document), below
/// `header` (the generated-file marker).
#[must_use]
pub fn render(schema: &Value, header: &str) -> String {
    let protocol = schema["protocol"].as_str().unwrap_or("0.0");
    let mut out = format!(
        "{header}# Client protocol reference\n\n\
         Protocol `{protocol}`: JSON-RPC 2.0 over the daemon's loopback WebSocket, \
         one message per text frame. The [client protocol guide](../guide/client-protocol.md) \
         explains attaching, the handshake, events and server requests; this page \
         lists every name of `schema/rpc.json`. A name whose contract is \
         \"not served yet\" is reserved for its owning capability and answered \
         `-32006 rpc.not_served` until that capability lands.\n"
    );
    render_codes(&mut out, "Error codes", "Code", &schema["error_codes"]);
    render_codes(&mut out, "Close codes", "Code", &schema["close_codes"]);
    render_names(&mut out, "Methods", &schema["methods"], Some("Result"));
    render_names(
        &mut out,
        "Server requests",
        &schema["server_requests"],
        Some("Result"),
    );
    render_names(&mut out, "Events", &schema["events"], None);
    if let Some(defs) = schema["$defs"].as_object() {
        out.push_str("\n## Types\n");
        for (name, def) in defs {
            render_def(&mut out, name, def);
        }
    }
    out
}

fn render_codes(out: &mut String, title: &str, column: &str, codes: &Value) {
    let _ = write!(out, "\n## {title}\n\n| {column} | Meaning |\n|---:|---|\n");
    for (code, meaning) in codes.as_object().into_iter().flatten() {
        let _ = writeln!(out, "| `{code}` | `{}` |", meaning.as_str().unwrap_or(""));
    }
}

/// One table per section. Methods and requests have params and a result,
/// events a payload.
fn render_names(out: &mut String, title: &str, names: &Value, result: Option<&str>) {
    let (first, second) = match result {
        Some(result) => ("Params", result),
        None => ("Payload", ""),
    };
    let _ = write!(out, "\n## {title}\n\n| Name | Owner | Since | {first} |");
    if result.is_some() {
        let _ = write!(out, " {second} |");
    }
    out.push_str("\n|---|---|---|---|");
    if result.is_some() {
        out.push_str("---|");
    }
    out.push('\n');
    for (name, entry) in names.as_object().into_iter().flatten() {
        let owner = entry["owner"].as_str().unwrap_or("");
        let since = entry["since"].as_str().unwrap_or("");
        let _ = write!(out, "| `{name}` | `{owner}` | {since} |");
        if entry["declared"] == Value::Bool(false) {
            out.push_str(" not served yet |");
            if result.is_some() {
                out.push_str(" |");
            }
        } else if result.is_some() {
            let _ = write!(
                out,
                " {} | {} |",
                type_of(&entry["params"]),
                type_of(&entry["result"])
            );
        } else {
            let _ = write!(out, " {} |", type_of(&entry["payload"]));
        }
        out.push('\n');
    }
}

fn render_def(out: &mut String, name: &str, def: &Value) {
    let _ = write!(out, "\n### `{name}`\n\n");
    if let Some(description) = def["description"].as_str() {
        let _ = writeln!(out, "{}\n", one_line(description));
    }
    if let Some(properties) = def["properties"].as_object() {
        render_fields(out, properties, &def["required"]);
    } else if let Some(variants) = def["oneOf"].as_array() {
        out.push_str("| Value | Meaning |\n|---|---|\n");
        for variant in variants {
            let _ = writeln!(
                out,
                "| `{}` | {} |",
                variant["const"].as_str().unwrap_or(""),
                one_line(variant["description"].as_str().unwrap_or(""))
            );
        }
    } else if let Some(values) = def["enum"].as_array() {
        let listed: Vec<String> = values.iter().map(|v| format!("`{v}`")).collect();
        let _ = writeln!(out, "One of {}.", listed.join(", "));
    }
}

fn render_fields(out: &mut String, properties: &Map<String, Value>, required: &Value) {
    if properties.is_empty() {
        out.push_str("No fields.\n");
        return;
    }
    out.push_str("| Field | Type | Required | Description |\n|---|---|---|---|\n");
    let required: Vec<&str> = required
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    for (field, schema) in properties {
        let _ = writeln!(
            out,
            "| `{field}` | {} | {} | {} |",
            type_of(schema),
            if required.contains(&field.as_str()) {
                "yes"
            } else {
                "no"
            },
            one_line(schema["description"].as_str().unwrap_or(""))
        );
    }
}

/// A short type expression; a reference links to its type's heading.
fn type_of(schema: &Value) -> String {
    if schema == &Value::Bool(true) {
        return "any".to_owned();
    }
    if let Some(target) = schema["$ref"].as_str() {
        let name = target.rsplit('/').next().unwrap_or(target);
        return format!("[`{name}`](#{})", name.to_lowercase());
    }
    if let Some(variants) = schema["anyOf"].as_array() {
        let parts: Vec<String> = variants.iter().map(type_of).collect();
        return parts.join(" or ");
    }
    match &schema["type"] {
        Value::String(kind) => scalar(kind, schema),
        Value::Array(kinds) => {
            let parts: Vec<String> = kinds
                .iter()
                .filter_map(Value::as_str)
                .map(|kind| scalar(kind, schema))
                .collect();
            parts.join(" or ")
        }
        _ => "any".to_owned(),
    }
}

fn scalar(kind: &str, schema: &Value) -> String {
    if kind == "array" {
        return format!("array of {}", type_of(&schema["items"]));
    }
    kind.to_owned()
}

/// Doc comments may wrap; a table cell must not.
fn one_line(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

#[cfg(test)]
mod tests {
    use crate::Catalog;

    #[test]
    fn every_name_and_type_of_the_schema_is_documented() {
        let schema = Catalog::builtin().schema();
        let page = super::render(&schema, "");
        for section in ["methods", "server_requests", "events"] {
            for (name, entry) in schema[section].as_object().unwrap() {
                let owner = entry["owner"].as_str().unwrap();
                assert!(
                    page.contains(&format!("| `{name}` | `{owner}` |")),
                    "{name} missing"
                );
            }
        }
        for name in schema["$defs"].as_object().unwrap().keys() {
            assert!(page.contains(&format!("### `{name}`")), "{name} missing");
        }
        assert!(page.contains("| `session.create` | `workspace-architecture` | 1.0 | [`SessionCreateParams`](#sessioncreateparams) | [`SessionRef`](#sessionref) |"));
        assert!(page.contains("| `worktree.new` | `git-worktrees` | 1.0 | not served yet | |"));
        assert!(page.contains("| `-32601` | `unknown_method` |"));
        assert!(page.contains("| `4426` | `protocol_unsupported` |"));
    }
}
