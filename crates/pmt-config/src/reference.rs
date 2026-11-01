//! The generated configuration reference (`docs/reference/configuration.md`
//! and `prometheus config docs`): one row per key with its type, default,
//! allowed values, apply class and description, all read from the schema
//! (spec: workspace-architecture, Generated reference documentation).

use std::fmt::Write as _;

use serde_json::Value;

use crate::catalog::{KeyInfo, allowed_values, keys_in, type_label};

/// Renders the reference of the schema `root`. `header` precedes the title
/// (for a "generated" marker) and `intro` says which file it describes.
#[must_use]
pub fn render(root: &'static Value, header: &str, title: &str, intro: &str) -> String {
    let mut out = format!("{header}# {title}\n\n{intro}\n\n");
    out.push_str(
        "`<name>` stands for any table name (a provider id, a model alias, a platform). \
         Apply: `live` takes effect at once, `next_session` for sessions started \
         afterwards, `restart` after the process restarts.\n\n",
    );
    out.push_str("| Key | Type | Default | Allowed values | Apply | Description |\n");
    out.push_str("|---|---|---|---|---|---|\n");
    for key in keys_in(root) {
        out.push_str(&row(&key));
    }
    out
}

fn row(key: &KeyInfo) -> String {
    let default = key.default.map_or_else(|| "unset".to_owned(), code);
    let mut line = String::new();
    let _ = writeln!(
        line,
        "| `{}` | {} | {default} | {} | `{}` | {} |",
        key.pattern,
        escape(&type_label(key.schema)),
        allowed(key.schema),
        key.apply,
        escape(&key.description),
    );
    line
}

fn allowed(schema: &Value) -> String {
    if let Some(values) = allowed_values(schema) {
        return values.iter().map(code).collect::<Vec<_>>().join(", ");
    }
    if schema.get("x-secret-ref") == Some(&Value::Bool(true)) {
        return "a secret name".to_owned();
    }
    let bound = |name: &str| schema.get(name).map(Value::to_string);
    match (
        bound("minimum").or_else(|| bound("exclusiveMinimum").map(|m| format!("> {m}"))),
        bound("maximum").or_else(|| bound("exclusiveMaximum").map(|m| format!("< {m}"))),
    ) {
        (Some(min), Some(max)) if min.starts_with('>') => format!("{min}, {max}"),
        (Some(min), Some(max)) => format!("{min} to {max}"),
        (Some(min), None) if min.starts_with('>') => min,
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
    use crate::catalog;

    use super::render;

    fn reference() -> String {
        render(catalog::schema(), "", "T", "I")
    }

    #[test]
    fn rows_carry_type_default_range_apply_and_description() {
        let out = reference();
        assert!(
            out.contains("| `agent.max_iterations` | integer | `500` | 0 to 100000 | `next_session` | Maximum model calls per turn; `0` means unlimited. |"),
            "{out}"
        );
        assert!(out.contains(
            "| `approvals.mode` | one of \"manual\", \"smart\", \"off\" | `\"manual\"` | `\"manual\"`, `\"smart\"`, `\"off\"` | `live` |"
        ));
        assert!(out.contains("| `compression.threshold` | number | `0.5` | > 0.0, < 1.0 |"));
        assert!(out.contains("| `providers.<name>.max_retries` | integer | `2` | 0 to 6 |"));
        assert!(out.contains("| `agent.run_budget_secs` | integer | unset |"));
    }

    #[test]
    fn every_key_is_documented() {
        for key in catalog::keys() {
            assert!(
                !key.description.is_empty(),
                "{} has no description",
                key.pattern
            );
        }
    }
}
