//! Renders the configuration reference from the `config.toml` JSON Schema,
//! so every key, type, default and allowed value is stated only in the
//! `pmt-config` types (spec: workspace-architecture, Generated reference
//! documentation).

use std::fmt::Write as _;

use serde_json::Value;

/// One table row per leaf key; sections (objects with their own keys) are
/// flattened into dotted paths so the reference reads like `config.toml`.
pub(crate) fn render(schema: &Value, header: &str) -> String {
    let mut out = format!("{header}# Configuration reference\n\n");
    out.push_str(
        "Every key a profile's `config.toml` accepts. Unknown keys are rejected, \
         so a key missing here is a typo, not a silent default.\n\n",
    );
    out.push_str("| Key | Type | Default | Allowed values | Description |\n");
    out.push_str("|---|---|---|---|---|\n");
    let mut rows = Vec::new();
    collect_rows(schema, schema, "", &mut rows);
    for row in rows {
        out.push_str(&row);
    }
    out
}

fn collect_rows(root: &Value, schema: &Value, prefix: &str, rows: &mut Vec<String>) {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return;
    };
    for (name, property) in properties {
        let key = format!("{prefix}{name}");
        let resolved = resolve(root, property);
        if resolved.get("properties").is_some() {
            collect_rows(root, resolved, &format!("{key}."), rows);
        } else {
            rows.push(row(&key, property, resolved));
        }
    }
}

/// Follows a local `$ref` (`#/$defs/Name`) to its definition; schemars emits
/// one for every named nested type.
fn resolve<'a>(root: &'a Value, schema: &'a Value) -> &'a Value {
    let target = schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix('#'))
        .and_then(|pointer| root.pointer(pointer));
    target.unwrap_or(schema)
}

/// The property's own fields win over the referenced definition's: schemars
/// puts the field doc comment and default next to the `$ref`.
fn row(key: &str, property: &Value, resolved: &Value) -> String {
    let field = |name: &str| property.get(name).or_else(|| resolved.get(name));
    let default = field("default").map_or_else(|| "required".to_owned(), code);
    let description = field("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .replace('\n', " ");
    let mut line = String::new();
    let _ = writeln!(
        line,
        "| `{key}` | {} | {default} | {} | {} |",
        type_name(resolved),
        allowed_values(resolved),
        escape(&description),
    );
    line
}

fn type_name(schema: &Value) -> String {
    match schema.get("type") {
        Some(Value::String(t)) => t.clone(),
        Some(Value::Array(types)) => types
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" or "),
        _ if schema.get("enum").is_some() => "enum".to_owned(),
        _ => "any".to_owned(),
    }
}

fn allowed_values(schema: &Value) -> String {
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        return values.iter().map(code).collect::<Vec<_>>().join(", ");
    }
    match (schema.get("minimum"), schema.get("maximum")) {
        (Some(min), Some(max)) => format!("{min} to {max}"),
        (Some(min), None) => format!("≥ {min}"),
        (None, Some(max)) => format!("≤ {max}"),
        (None, None) => "any".to_owned(),
    }
}

fn code(value: &Value) -> String {
    format!("`{}`", escape(&value.to_string()))
}

fn escape(text: &str) -> String {
    text.replace('|', "\\|")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::render;

    #[test]
    fn nested_sections_become_dotted_keys_with_defaults_and_values() {
        let schema = json!({
            "properties": {
                "agent": {"$ref": "#/$defs/Agent", "description": "Turn loop."},
                "mode": {"type": "string", "enum": ["a", "b"], "default": "a",
                         "description": "Pick a | b."}
            },
            "$defs": {"Agent": {"type": "object", "properties": {
                "max_iterations": {"type": "integer", "minimum": 1, "default": 90,
                                   "description": "Turn limit."}
            }}}
        });
        let out = render(&schema, "");
        assert!(
            out.contains("| `agent.max_iterations` | integer | `90` | ≥ 1 | Turn limit. |"),
            "{out}"
        );
        assert!(
            out.contains("| `mode` | string | `\"a\"` | `\"a\"`, `\"b\"` | Pick a \\| b. |"),
            "{out}"
        );
    }

    #[test]
    fn a_key_without_default_is_required() {
        let schema = json!({"properties": {"name": {"type": "string"}}});
        assert!(render(&schema, "").contains("| `name` | string | required | any |  |"));
    }
}
