//! `prometheus config explain <key>` and `--changed` (spec:
//! configuration-and-profiles, Config explain): the key's type, default,
//! effective value, the layer that supplied it with its file position,
//! description and apply class.

use std::fmt::Write as _;

use pmt_config::{ProfileScope, Source, catalog};
use pmt_core::error::Error;
use serde::Serialize;
use serde_json::Value;

use super::{document, known_key, print_raw};

/// The `config explain --json` document.
#[derive(Debug, Serialize)]
struct Explained<'a> {
    key: &'a str,
    #[serde(rename = "type")]
    type_label: String,
    default: Option<&'a Value>,
    value: Option<&'a Value>,
    source: String,
    file: Option<String>,
    line: Option<usize>,
    apply: &'static str,
    secret_ref: bool,
    description: &'a str,
}

/// Explains one key.
pub(super) fn key(scope: &ProfileScope, key: &str, json: bool) -> Result<u8, Error> {
    let info = known_key(key)?;
    let layers = scope.layers();
    let source = layers.source(key);
    let (file, line) = match &source {
        Source::File { path, line } => (Some(path.display().to_string()), Some(*line)),
        _ => (None, None),
    };
    let explained = Explained {
        key,
        type_label: catalog::type_label(info.schema),
        default: info.default,
        value: layers.value(key),
        source: source.to_string(),
        file,
        line,
        apply: info.apply.as_str(),
        secret_ref: info.secret_ref,
        description: &info.description,
    };
    if json {
        return document("config explain", &explained);
    }
    print_raw(&render(&explained))
}

fn render(e: &Explained<'_>) -> String {
    let show = |value: Option<&Value>| value.map_or_else(|| "(unset)".to_owned(), render_value);
    let mut out = format!("{}\n", e.key);
    let mut row = |name: &str, value: &str| {
        let _ = writeln!(out, "  {name:<12} {value}");
    };
    row("type:", &e.type_label);
    row("default:", &show(e.default));
    row("effective:", &show(e.value));
    let source = match (&e.file, e.line) {
        (Some(file), Some(line)) => format!("{} ({file}:{line})", e.source),
        _ => e.source.clone(),
    };
    row("source:", &source);
    row("apply:", e.apply);
    if e.secret_ref {
        row("secret:", "names a secret in the secrets store");
    }
    row("description:", e.description);
    out
}

/// Lists every key whose effective value differs from its default.
pub(super) fn changed(scope: &ProfileScope, json: bool) -> Result<u8, Error> {
    #[derive(Serialize)]
    struct Changed<'a> {
        key: &'a str,
        value: Option<&'a Value>,
        default: Option<&'a Value>,
        source: String,
    }
    #[derive(Serialize)]
    struct Listing<'a> {
        changed: Vec<Changed<'a>>,
    }
    let layers = scope.layers();
    let keys = layers.changed();
    let changed: Vec<Changed<'_>> = keys
        .iter()
        .map(|key| Changed {
            key,
            value: layers.value(key),
            default: catalog::lookup(key).and_then(|info| info.default),
            source: layers.source(key).to_string(),
        })
        .collect();
    if json {
        return document("config explain", &Listing { changed });
    }
    let mut out = String::new();
    for c in &changed {
        let value = c.value.map_or_else(|| "(unset)".to_owned(), render_value);
        let _ = writeln!(out, "{} = {value}  ({})", c.key, c.source);
    }
    print_raw(&out)
}

/// A value as the user would write it in `config.toml`; strings are shown
/// without quotes.
pub(super) fn render_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => toml_literal(other),
    }
}

fn toml_literal(value: &Value) -> String {
    match value {
        Value::String(text) => toml::Value::String(text.clone()).to_string(),
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(toml_literal).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Object(map) => {
            let entries: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{k} = {}", toml_literal(v)))
                .collect();
            format!("{{ {} }}", entries.join(", "))
        }
        Value::Null => "(unset)".to_owned(),
        scalar => scalar.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::render_value;

    #[test]
    fn values_render_as_toml() {
        assert_eq!(render_value(&json!("mono")), "mono");
        assert_eq!(render_value(&json!(40)), "40");
        assert_eq!(render_value(&json!(["core", "web"])), "[\"core\", \"web\"]");
        assert_eq!(render_value(&json!({"a": 1})), "{ a = 1 }");
    }
}
