//! The key catalog: the JSON Schema of [`Config`] read as a tree of dotted
//! keys. Validation, `config get|set|explain`, the reference docs and
//! completions all look keys up here, so nothing restates a key, default or
//! apply class by hand.

use std::fmt;
use std::sync::LazyLock;

use serde_json::Value;

use crate::schema::Config;

static SCHEMA: LazyLock<Value> = LazyLock::new(|| {
    serde_json::to_value(schemars::schema_for!(Config)).expect("the schema serializes")
});

/// The schema of a value that may be anything (`additionalProperties: true`).
static ANY: LazyLock<Value> = LazyLock::new(|| Value::Object(serde_json::Map::new()));

/// The `config.toml` JSON Schema (draft 2020-12), as `prometheus config
/// schema` prints it.
#[must_use]
pub fn schema() -> &'static Value {
    &SCHEMA
}

/// A schema as pretty JSON with a trailing newline: the bytes of
/// `schema/config.schema.json` and `prometheus config schema`.
///
/// # Panics
///
/// Never: a JSON value always serializes.
#[must_use]
pub fn pretty(schema: &Value) -> String {
    let mut json = serde_json::to_string_pretty(schema).expect("a JSON value serializes");
    json.push('\n');
    json
}

/// When a changed value takes effect in a running process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyClass {
    /// Immediately.
    Live,
    /// For sessions created afterwards.
    NextSession,
    /// After the process restarts.
    Restart,
}

impl ApplyClass {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "live" => Some(Self::Live),
            "next_session" => Some(Self::NextSession),
            "restart" => Some(Self::Restart),
            _ => None,
        }
    }

    /// The name used in the schema and docs.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::NextSession => "next_session",
            Self::Restart => "restart",
        }
    }
}

impl fmt::Display for ApplyClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One key of the schema.
#[derive(Debug, Clone)]
pub struct KeyInfo {
    /// The dotted key as asked for (`providers.openrouter.max_retries`).
    pub key: String,
    /// The key with map entries as placeholders (`providers.<name>.max_retries`).
    pub pattern: String,
    /// The value schema with `$ref` and the optional (`null`) variant
    /// removed.
    pub schema: &'static Value,
    /// The schema default; `None` when the key is unset by default.
    pub default: Option<&'static Value>,
    /// The user-facing description.
    pub description: String,
    /// When a change takes effect.
    pub apply: ApplyClass,
    /// The value names a secret in the secrets store.
    pub secret_ref: bool,
    /// The key is a section (a table of keys), not a value.
    pub section: bool,
}

/// Looks up a dotted key (`agent.max_iterations`, `providers.acme.base_url`).
/// Map entries accept any name.
#[must_use]
pub fn lookup(key: &str) -> Option<KeyInfo> {
    lookup_in(schema(), key)
}

/// [`lookup`] in another schema (for example `daemon.toml`'s).
#[must_use]
pub fn lookup_in(root: &'static Value, key: &str) -> Option<KeyInfo> {
    let mut node = Node::root(root);
    let mut pattern = Vec::new();
    for segment in key.split('.') {
        let (child, placeholder) = node.child(segment)?;
        pattern.push(if placeholder { "<name>" } else { segment });
        node = child;
    }
    if pattern.is_empty() {
        return None;
    }
    Some(node.info(key.to_owned(), pattern.join(".")))
}

/// The closest known keys to an unknown `key`, for "did you mean".
#[must_use]
pub fn suggestions(key: &str) -> Vec<String> {
    let (parent, leaf) = key.rsplit_once('.').unwrap_or(("", key));
    let siblings = if parent.is_empty() {
        child_names(schema())
    } else {
        lookup(parent).map_or_else(Vec::new, |info| child_names(info.schema))
    };
    pmt_core::registry::closest(leaf, siblings.iter().map(String::as_str))
        .into_iter()
        .map(|name| join(parent, name))
        .collect()
}

/// Every value key in schema order, map entries as `<name>` placeholders;
/// maps of tables are expanded, other maps are one key.
#[must_use]
pub fn keys() -> Vec<KeyInfo> {
    keys_in(schema())
}

/// [`keys`] of another schema (for example `daemon.toml`'s).
#[must_use]
pub fn keys_in(root: &'static Value) -> Vec<KeyInfo> {
    let mut out = Vec::new();
    collect(Node::root(root), "", &mut out);
    out
}

fn collect(node: Node, prefix: &str, out: &mut Vec<KeyInfo>) {
    for name in child_names(node.resolved) {
        let (child, _) = node.child(&name).expect("listed child exists");
        let key = join(prefix, &name);
        push_or_descend(child, key, out);
    }
    if let Some(entry) = node.resolved.get("additionalProperties")
        && is_table(resolve_in(node.root, entry))
    {
        let (child, _) = node.child("<name>").expect("map entry exists");
        collect(child, &join(prefix, "<name>"), out);
    }
}

fn push_or_descend(child: Node, key: String, out: &mut Vec<KeyInfo>) {
    let map_of_tables = child
        .resolved
        .get("additionalProperties")
        .is_some_and(|entry| is_table(resolve_in(child.root, entry)));
    if is_table(child.resolved) || map_of_tables {
        collect(child, &key, out);
    } else {
        out.push(child.info(key.clone(), key));
    }
}

fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}.{name}")
    }
}

/// A struct-like schema: it lists its keys.
fn is_table(schema: &Value) -> bool {
    schema.get("properties").is_some()
}

fn child_names(schema: &Value) -> Vec<String> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|p| p.keys().cloned().collect())
        .unwrap_or_default()
}

/// A position in the schema tree.
#[derive(Clone, Copy)]
struct Node {
    /// The schema document `$ref`s point into.
    root: &'static Value,
    /// The property schema (holds the field's description and default).
    property: &'static Value,
    /// The value schema after `$ref` and optional unwrapping.
    resolved: &'static Value,
    apply: Option<ApplyClass>,
}

impl Node {
    fn root(root: &'static Value) -> Self {
        Self {
            root,
            property: root,
            resolved: root,
            apply: apply_of(root),
        }
    }

    /// The child for `segment`; the flag says it is a map entry.
    fn child(self, segment: &str) -> Option<(Self, bool)> {
        let named = self
            .resolved
            .get("properties")
            .and_then(|p| p.get(segment))
            .map(|p| (p, false));
        let (property, placeholder) =
            named.or_else(|| match self.resolved.get("additionalProperties")? {
                Value::Bool(true) => Some((&*ANY, true)),
                entry if entry.is_object() => Some((entry, true)),
                _ => None,
            })?;
        let resolved = resolve_in(self.root, property);
        let apply = apply_of(property)
            .or_else(|| apply_of(resolved))
            .or(self.apply);
        Some((
            Self {
                root: self.root,
                property,
                resolved,
                apply,
            },
            placeholder,
        ))
    }

    fn info(self, key: String, pattern: String) -> KeyInfo {
        let field = |name: &str| self.property.get(name).or_else(|| self.resolved.get(name));
        KeyInfo {
            key,
            pattern,
            schema: self.resolved,
            default: field("default").filter(|d| !d.is_null()),
            description: field("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            apply: self.apply.unwrap_or(ApplyClass::NextSession),
            secret_ref: self.resolved.get("x-secret-ref") == Some(&Value::Bool(true)),
            section: is_table(self.resolved),
        }
    }
}

fn apply_of(schema: &Value) -> Option<ApplyClass> {
    schema
        .get("x-apply")
        .and_then(Value::as_str)
        .and_then(ApplyClass::parse)
}

/// Follows `$ref` (`#/$defs/Name`) and drops the `null` variant of an
/// optional value, so callers see the value's own schema.
#[must_use]
pub fn resolve(schema: &'static Value) -> &'static Value {
    resolve_in(&SCHEMA, schema)
}

fn resolve_in(root: &'static Value, schema: &'static Value) -> &'static Value {
    let mut current = schema;
    loop {
        if let Some(target) = current
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix('#'))
            .and_then(|pointer| root.pointer(pointer))
        {
            current = target;
            continue;
        }
        if let Some(inner) = non_null_variant(current) {
            current = inner;
            continue;
        }
        return current;
    }
}

/// `anyOf: [X, {type: null}]` -> `X`.
fn non_null_variant(schema: &'static Value) -> Option<&'static Value> {
    let variants = schema.get("anyOf")?.as_array()?;
    let mut rest = variants
        .iter()
        .filter(|v| v.get("type") != Some(&Value::from("null")));
    let only = rest.next()?;
    (rest.next().is_none() && variants.len() == 2).then_some(only)
}

/// Allowed values of an enum schema (`oneOf`/`anyOf` of `const`, or
/// `enum`), or `None` for other schemas.
#[must_use]
pub fn allowed_values(schema: &Value) -> Option<Vec<Value>> {
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        return Some(values.clone());
    }
    let variants = schema
        .get("oneOf")
        .or_else(|| schema.get("anyOf"))?
        .as_array()?;
    variants.iter().map(|v| v.get("const").cloned()).collect()
}

/// A short type label: `integer`, `string`, `boolean`, `one of "a", "b"`,
/// `array of string`, `table`.
#[must_use]
pub fn type_label(schema: &'static Value) -> String {
    if let Some(values) = allowed_values(schema) {
        let values: Vec<String> = values.iter().map(Value::to_string).collect();
        return format!("one of {}", values.join(", "));
    }
    if let Some(variants) = schema.get("anyOf").and_then(Value::as_array) {
        let labels: Vec<String> = variants.iter().map(|v| type_label(resolve(v))).collect();
        return labels.join(" or ");
    }
    match schema.get("type") {
        Some(Value::String(t)) if t == "array" => {
            let items = schema
                .get("items")
                .map_or_else(|| "any".to_owned(), |i| type_label(resolve(i)));
            format!("array of {items}")
        }
        Some(Value::String(t)) if t == "object" => "table".to_owned(),
        Some(Value::String(t)) => t.clone(),
        Some(Value::Array(types)) => types
            .iter()
            .filter_map(Value::as_str)
            .filter(|t| *t != "null")
            .collect::<Vec<_>>()
            .join(" or "),
        _ => "any".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{ApplyClass, keys, lookup, suggestions};

    #[test]
    fn nested_key_has_default_description_and_apply_class() {
        let info = lookup("agent.max_iterations").unwrap();
        assert_eq!(info.default, Some(&serde_json::json!(500)));
        assert_eq!(info.apply, ApplyClass::NextSession);
        assert!(!info.description.is_empty());
        assert!(!info.section);
    }

    #[test]
    fn map_entries_accept_any_name() {
        let info = lookup("providers.acme.max_retries").unwrap();
        assert_eq!(info.pattern, "providers.<name>.max_retries");
        assert_eq!(info.default, Some(&serde_json::json!(2)));
        assert!(lookup("providers.acme.base_url").is_some());
        assert!(lookup("providers.acme.bogus").is_none());
    }

    #[test]
    fn field_apply_class_overrides_its_section() {
        assert_eq!(
            lookup("browser.dialog_timeout_secs").unwrap().apply,
            ApplyClass::NextSession
        );
        assert_eq!(
            lookup("browser.inactivity_timeout_secs").unwrap().apply,
            ApplyClass::Live
        );
        assert_eq!(lookup("display.theme").unwrap().apply, ApplyClass::Live);
    }

    #[test]
    fn secret_name_keys_are_marked() {
        assert!(lookup("providers.acme.key_secret").unwrap().secret_ref);
        assert!(!lookup("providers.acme.base_url").unwrap().secret_ref);
    }

    #[test]
    fn misspelled_key_suggests_the_full_path() {
        assert_eq!(suggestions("agent.max_iteration"), ["agent.max_iterations"]);
        assert_eq!(suggestions("agnt"), ["agent"]);
    }

    #[test]
    fn every_value_key_is_listed_once_with_an_apply_class() {
        let all = keys();
        let names: Vec<&str> = all.iter().map(|k| k.pattern.as_str()).collect();
        assert!(names.contains(&"agent.max_iterations"));
        assert!(names.contains(&"providers.<name>.max_retries"));
        assert!(names.contains(&"tool_loop_guardrails.warn_after.exact_failure"));
        let mut sorted = names.clone();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len());
        assert!(all.iter().all(|k| !k.section));
    }
}
