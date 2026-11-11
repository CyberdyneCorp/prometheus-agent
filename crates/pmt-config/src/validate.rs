//! Strict, schema-driven validation of a `config.toml` document (spec:
//! configuration-and-profiles, Strict validation). It reports every problem
//! at once with its key and position: unknown keys (with "did you mean"),
//! machine-wide sections, wrong types, unknown enum values (with the
//! allowed values), out-of-range numbers and credential-looking values in
//! non-secret keys (spec: Secrets are not configuration). Only a document
//! without issues is deserialized into [`crate::Config`].

use std::ops::Range;

use pmt_core::error::{ErrorCode, codes};
use serde_json::Value;

use crate::catalog::{self, allowed_values, resolve, type_label};
use crate::credential;
use crate::schema::{MACHINE_WIDE_GATEWAY_KEYS, MACHINE_WIDE_SECTIONS, is_secret_name};

/// One validation problem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// `config.unknown_key`, `config.invalid` or
    /// `config.credential_in_file`.
    pub code: &'static ErrorCode,
    /// The dotted key the problem is about.
    pub key: String,
    /// Byte span in the source text, when the value came from a file.
    pub span: Option<Range<usize>>,
    /// What is wrong, naming the key.
    pub message: String,
}

/// A TOML value with its source span, independent of the parser so file
/// contents and `--set` values validate the same way.
#[derive(Debug, Clone)]
pub struct Node {
    /// The value.
    pub kind: Kind,
    /// Byte span of the value (or key, for tables) in the source.
    pub span: Option<Range<usize>>,
}

/// The shape of a [`Node`].
#[derive(Debug, Clone)]
pub enum Kind {
    /// A string, integer, float, boolean or datetime as JSON (datetimes as
    /// strings) with its TOML type name.
    Scalar(Value, &'static str),
    /// An array.
    Array(Vec<Node>),
    /// A table: key, key span, value.
    Table(Vec<(String, Option<Range<usize>>, Node)>),
}

impl Node {
    /// Converts a parsed `toml_edit` item.
    #[must_use]
    pub fn from_item(item: &toml_edit::Item) -> Self {
        let span = item.span();
        match item {
            toml_edit::Item::Value(value) => Self::from_value(value),
            toml_edit::Item::ArrayOfTables(array) => Self {
                kind: Kind::Array(
                    array
                        .iter()
                        .map(|t| Self::from_table_like(t, t.span()))
                        .collect(),
                ),
                span,
            },
            toml_edit::Item::Table(table) => Self::from_table_like(table, table.span()),
            toml_edit::Item::None => Self {
                kind: Kind::Table(Vec::new()),
                span,
            },
        }
    }

    /// Converts a parsed `toml_edit` value.
    #[must_use]
    pub fn from_value(value: &toml_edit::Value) -> Self {
        use toml_edit::Value as V;
        let span = value.span();
        let kind = match value {
            V::String(s) => Kind::Scalar(Value::from(s.value().as_str()), "string"),
            V::Integer(i) => Kind::Scalar(Value::from(*i.value()), "integer"),
            V::Float(f) => Kind::Scalar(Value::from(*f.value()), "float"),
            V::Boolean(b) => Kind::Scalar(Value::from(*b.value()), "boolean"),
            V::Datetime(d) => Kind::Scalar(Value::from(d.value().to_string()), "datetime"),
            V::Array(array) => Kind::Array(array.iter().map(Self::from_value).collect()),
            V::InlineTable(table) => return Self::from_table_like(table, span),
        };
        Self { kind, span }
    }

    fn from_table_like(table: &dyn toml_edit::TableLike, span: Option<Range<usize>>) -> Self {
        let entries = table
            .iter()
            .map(|(key, item)| {
                let key_span = table.get_key_value(key).and_then(|(k, _)| k.span());
                (key.to_owned(), key_span, Self::from_item(item))
            })
            .collect();
        Self {
            kind: Kind::Table(entries),
            span,
        }
    }

    fn toml_type(&self) -> &'static str {
        match &self.kind {
            Kind::Scalar(_, name) => name,
            Kind::Array(_) => "array",
            Kind::Table(_) => "table",
        }
    }
}

/// Validates a whole document against the `config.toml` schema.
#[must_use]
pub fn document(root: &Node) -> Vec<Issue> {
    let mut issues = Vec::new();
    for (key, span) in machine_wide_keys(root) {
        issues.push(Issue {
            code: &codes::CONFIG_UNKNOWN_KEY,
            message: format!(
                "'{key}' is machine-wide and only valid in <home>/daemon.toml, not a profile config.toml"
            ),
            key,
            span,
        });
    }
    let mut checker = Checker {
        issues: &mut issues,
    };
    checker.check(root, catalog::schema(), "");
    crate::validate_platforms::check(root, &mut issues);
    issues.retain(|i| i.code != &codes::CONFIG_UNKNOWN_KEY || !is_machine_wide_duplicate(i));
    issues.sort_by_key(|i| i.span.as_ref().map_or(usize::MAX, |s| s.start));
    issues.dedup();
    issues
}

/// The machine-wide check already reported these sections; drop the
/// generic "unknown key" for them.
fn is_machine_wide_duplicate(issue: &Issue) -> bool {
    let machine_wide = MACHINE_WIDE_SECTIONS.contains(&issue.key.as_str())
        || issue
            .key
            .strip_prefix("gateway.")
            .is_some_and(|k| MACHINE_WIDE_GATEWAY_KEYS.contains(&k));
    machine_wide && issue.message.starts_with("unknown key")
}

/// Root sections and `[gateway]` keys of a profile document that belong in
/// `daemon.toml` (spec: messaging-gateway, Profiles served by one daemon).
fn machine_wide_keys(root: &Node) -> Vec<(String, Option<Range<usize>>)> {
    let mut found = Vec::new();
    let Kind::Table(entries) = &root.kind else {
        return found;
    };
    for (key, span, node) in entries {
        if MACHINE_WIDE_SECTIONS.contains(&key.as_str()) {
            found.push((key.clone(), span.clone()));
        } else if key == "gateway"
            && let Kind::Table(gateway) = &node.kind
        {
            for (sub, span, _) in gateway {
                if MACHINE_WIDE_GATEWAY_KEYS.contains(&sub.as_str()) {
                    found.push((format!("gateway.{sub}"), span.clone()));
                }
            }
        }
    }
    found
}

/// Validates one value for `key` (a `--set` or `config set` argument).
/// An unknown key yields one `config.unknown_key` issue.
#[must_use]
pub fn value(key: &str, node: &Node) -> Vec<Issue> {
    let Some(info) = catalog::lookup(key) else {
        return vec![unknown_key(key, None)];
    };
    let mut issues = Vec::new();
    Checker {
        issues: &mut issues,
    }
    .check(node, info.schema, key);
    issues
}

/// The `config.unknown_key` issue for `key`, with "did you mean".
#[must_use]
pub fn unknown_key(key: &str, span: Option<Range<usize>>) -> Issue {
    let suggestions: Vec<String> = catalog::suggestions(key);
    let names: Vec<&str> = suggestions.iter().map(String::as_str).collect();
    let message = if names.is_empty() {
        format!("unknown key '{key}'")
    } else {
        format!(
            "unknown key '{key}' (did you mean {}?)",
            pmt_core::registry::or_list(&names)
        )
    };
    Issue {
        code: &codes::CONFIG_UNKNOWN_KEY,
        key: key.to_owned(),
        span,
        message,
    }
}

pub(crate) struct Checker<'a> {
    pub(crate) issues: &'a mut Vec<Issue>,
}

impl Checker<'_> {
    fn invalid(&mut self, key: &str, node: &Node, message: String) {
        self.issues.push(Issue {
            code: &codes::CONFIG_INVALID,
            key: key.to_owned(),
            span: node.span.clone(),
            message,
        });
    }

    pub(crate) fn check(&mut self, node: &Node, schema: &'static Value, key: &str) {
        let schema = resolve(schema);
        if let Some(allowed) = allowed_values(schema) {
            return self.check_enum(node, &allowed, key);
        }
        if let Some(variants) = schema.get("anyOf").and_then(Value::as_array) {
            return self.check_any_of(node, schema, variants, key);
        }
        if let Some(variants) = schema.get("oneOf").and_then(Value::as_array) {
            return self.check_one_of(node, variants, key);
        }
        if !type_matches(schema, node) {
            let message = format!(
                "{key}: expected {}, found {}{}",
                type_label(schema),
                node.toml_type(),
                accepted_range(schema)
                    .map(|range| format!(" (accepted: {range})"))
                    .unwrap_or_default()
            );
            return self.invalid(key, node, message);
        }
        match &node.kind {
            Kind::Table(entries) => self.check_table(entries, schema, key),
            Kind::Array(items) => {
                if let Some(item_schema) = schema.get("items") {
                    for item in items {
                        self.check(item, item_schema, key);
                    }
                }
            }
            Kind::Scalar(value, _) => self.check_scalar(node, value, schema, key),
        }
    }

    fn check_table(
        &mut self,
        entries: &[(String, Option<Range<usize>>, Node)],
        schema: &'static Value,
        key: &str,
    ) {
        let properties = schema.get("properties").and_then(Value::as_object);
        let additional = schema.get("additionalProperties");
        for (name, key_span, child) in entries {
            let child_key = if key.is_empty() {
                name.clone()
            } else {
                format!("{key}.{name}")
            };
            match (properties.and_then(|p| p.get(name)), additional) {
                (Some(child_schema), _) => self.check(child, child_schema, &child_key),
                (None, Some(entry)) if entry.is_object() => self.check(child, entry, &child_key),
                (None, Some(Value::Bool(true))) => self.scan_free_form(child, &child_key),
                _ => self.issues.push(unknown_key(&child_key, key_span.clone())),
            }
        }
    }

    fn check_enum(&mut self, node: &Node, allowed: &[Value], key: &str) {
        let same_type = |value: &Value| {
            allowed
                .iter()
                .any(|a| std::mem::discriminant(a) == std::mem::discriminant(value))
        };
        let value = match &node.kind {
            Kind::Scalar(value, _) if same_type(value) => value,
            _ => {
                let message = format!(
                    "{key}: expected one of {}, found {}",
                    list(allowed),
                    node.toml_type()
                );
                return self.invalid(key, node, message);
            }
        };
        if let Some(text) = value.as_str()
            && self.refuse_credential(node, text, key)
        {
            return;
        }
        if !allowed.contains(value) {
            let message = unknown_value_message(key, value, allowed);
            self.invalid(key, node, message);
        }
    }

    /// An untagged value: the variant whose type fits decides; when none
    /// fits, the error lists every accepted type.
    fn check_any_of(
        &mut self,
        node: &Node,
        schema: &'static Value,
        variants: &'static [Value],
        key: &str,
    ) {
        let fitting = variants
            .iter()
            .map(resolve)
            .find(|variant| match allowed_values(variant) {
                Some(_) => matches!(node.kind, Kind::Scalar(Value::String(_), _)),
                None => type_matches(variant, node),
            });
        if let Some(variant) = fitting {
            self.check(node, variant, key);
        } else {
            let message = format!(
                "{key}: expected {}, found {}",
                type_label(schema),
                node.toml_type()
            );
            self.invalid(key, node, message);
        }
    }

    /// A tagged value (a table with one variant key, such as a webhook
    /// filter): the variant whose keys the table uses decides; when none
    /// does, the error lists the variants' keys.
    fn check_one_of(&mut self, node: &Node, variants: &'static [Value], key: &str) {
        let keys_of = |variant: &Value| -> Vec<String> {
            variant
                .get("properties")
                .and_then(Value::as_object)
                .map(|p| p.keys().cloned().collect())
                .unwrap_or_default()
        };
        let fitting = variants.iter().map(resolve).find(|variant| {
            let Kind::Table(entries) = &node.kind else {
                return type_matches(variant, node);
            };
            let keys = keys_of(variant);
            type_matches(variant, node) && entries.iter().all(|(name, ..)| keys.contains(name))
        });
        if let Some(variant) = fitting {
            return self.check(node, variant, key);
        }
        let names: Vec<String> = variants.iter().map(resolve).flat_map(keys_of).collect();
        let message = format!(
            "{key}: expected a table with one of the keys {}, found {}",
            names.join(", "),
            node.toml_type()
        );
        self.invalid(key, node, message);
    }

    /// A schema-free table (plugin or skill settings) has no types to
    /// check, but a credential is still refused anywhere inside it.
    fn scan_free_form(&mut self, node: &Node, key: &str) {
        match &node.kind {
            Kind::Scalar(Value::String(text), _) => {
                self.refuse_credential(node, text, key);
            }
            Kind::Scalar(..) => {}
            Kind::Array(items) => items.iter().for_each(|item| self.scan_free_form(item, key)),
            Kind::Table(entries) => {
                for (name, _, child) in entries {
                    self.scan_free_form(child, &format!("{key}.{name}"));
                }
            }
        }
    }

    /// Pushes the credential refusal when `text` looks like one. The
    /// message names the key only: a rejected value is never echoed.
    fn refuse_credential(&mut self, node: &Node, text: &str, key: &str) -> bool {
        let refused = credential::looks_like_credential(text);
        if refused {
            self.issues.push(Issue {
                code: &codes::CONFIG_CREDENTIAL_IN_FILE,
                key: key.to_owned(),
                span: node.span.clone(),
                message: credential::message(key),
            });
        }
        refused
    }

    /// Secret-reference keys get the credential check too: a pasted value
    /// that happens to fit the name pattern (an AWS key id) is still a
    /// credential, and the not-a-name error never repeats the value.
    fn check_scalar(&mut self, node: &Node, value: &Value, schema: &Value, key: &str) {
        if let Some(message) = range_error(value, schema) {
            return self.invalid(key, node, format!("{key}: {message}"));
        }
        let Some(text) = value.as_str() else { return };
        if self.refuse_credential(node, text, key) {
            return;
        }
        if schema.get("x-capability") == Some(&Value::Bool(true))
            && let Err(err) = pmt_core::capability::Capability::parse(text)
        {
            return self.invalid(key, node, format!("{key}: {}", err.message()));
        }
        if schema.get("x-key-chord") == Some(&Value::Bool(true))
            && let Err(reason) = crate::keys::Chord::parse(text)
        {
            return self.invalid(key, node, format!("{key}: {reason}"));
        }
        let is_secret_ref = schema.get("x-secret-ref") == Some(&Value::Bool(true));
        if is_secret_ref && !is_secret_name(text) {
            let message = format!(
                "{key}: not a secret name (expected the name of a stored secret matching ^[A-Z][A-Z0-9_]{{0,127}}$)"
            );
            self.invalid(key, node, message);
        }
    }
}

fn list(values: &[Value]) -> String {
    values
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The message for a value outside `allowed`; `cron.provider` words it
/// as its spec does (scheduled-jobs, Pluggable trigger provider).
fn unknown_value_message(key: &str, value: &Value, allowed: &[Value]) -> String {
    if key == "cron.provider" {
        let names: Vec<&str> = allowed.iter().filter_map(Value::as_str).collect();
        let shown = value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_owned);
        return format!(
            "unknown cron.provider '{shown}' (expected {})",
            names.join("|")
        );
    }
    format!("{key}: unknown value {value} (allowed: {})", list(allowed))
}

fn type_matches(schema: &Value, node: &Node) -> bool {
    let toml = node.toml_type();
    let accepts = |t: &str| match t {
        "object" => toml == "table",
        "number" => toml == "float" || toml == "integer",
        other => other == toml,
    };
    match schema.get("type") {
        Some(Value::String(t)) => accepts(t),
        Some(Value::Array(types)) => types.iter().filter_map(Value::as_str).any(accepts),
        _ => true,
    }
}

/// The range rule `value` breaks, worded as the spec's examples
/// (`must be between 1 and 10`).
fn range_error(value: &Value, schema: &Value) -> Option<String> {
    let number = value.as_f64()?;
    let bound = |name: &str| schema.get(name).and_then(Value::as_f64);
    let (min, max) = integer_bounds(schema);
    let min = bound("minimum").or(min);
    let max = bound("maximum").or(max);
    let (ex_min, ex_max) = (bound("exclusiveMinimum"), bound("exclusiveMaximum"));
    let out_of_range = min.is_some_and(|m| number < m)
        || max.is_some_and(|m| number > m)
        || ex_min.is_some_and(|m| number <= m)
        || ex_max.is_some_and(|m| number >= m);
    if !out_of_range {
        return None;
    }
    let show = |n: f64| Value::from(n).to_string().trim_end_matches(".0").to_owned();
    Some(match (min, max, ex_min, ex_max) {
        (_, _, Some(lo), Some(hi)) => {
            format!(
                "must be greater than {} and less than {}",
                show(lo),
                show(hi)
            )
        }
        (Some(lo), Some(hi), ..) => format!("must be between {} and {}", show(lo), show(hi)),
        (Some(lo), None, ..) => format!("must be at least {}", show(lo)),
        (None, Some(hi), ..) => format!("must be at most {}", show(hi)),
        _ => "is out of range".to_owned(),
    })
}

/// The declared range of a numeric key (`1 to 64`), when it has both
/// bounds: a wrongly typed value is told what it may be.
fn accepted_range(schema: &Value) -> Option<String> {
    let min = schema.get("minimum").and_then(Value::as_f64)?;
    let max = schema.get("maximum").and_then(Value::as_f64)?;
    let show = |n: f64| Value::from(n).to_string().trim_end_matches(".0").to_owned();
    Some(format!("{} to {}", show(min), show(max)))
}

/// Bounds implied by an unsigned integer `format`.
fn integer_bounds(schema: &Value) -> (Option<f64>, Option<f64>) {
    match schema.get("format").and_then(Value::as_str) {
        Some("uint32") => (Some(0.0), Some(f64::from(u32::MAX))),
        Some("uint64" | "uint") => (Some(0.0), None),
        _ => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::{Issue, Node, document};

    fn issues(text: &str) -> Vec<Issue> {
        let doc: toml_edit::Document<String> = text.parse().unwrap();
        document(&Node::from_item(doc.as_item()))
    }

    fn messages(text: &str) -> Vec<String> {
        issues(text).into_iter().map(|i| i.message).collect()
    }

    #[test]
    fn defaults_and_known_keys_are_valid() {
        assert!(issues("").is_empty());
        let text = "schema_version = 1\n[agent]\nmax_iterations = 40\n[approvals]\nmode = \"off\"\n\
                    [providers.acme]\nbase_url = \"http://x\"\nkey_secret = \"ACME_KEY\"\n\
                    [prompt.platform_hints.slack]\nappend = \"hi\"\n[agent.personalities]\npirate = \"arr\"\n\
                    [skills.config.foo]\nanything = [1, 2]\n[models]\nfast = \"openrouter/x\"\n\
                    [models.big]\nprovider = \"anthropic\"\ncontext_window = 200000\n";
        assert_eq!(issues(text), []);
    }

    #[test]
    fn credentials_in_the_file_are_refused_except_in_secret_keys() {
        let found = issues(
            "[display]\ntheme = \"sk-live-123456\"\n\
             [skills.config.foo]\nlist = [\"ok\", \"ghp_abcdefghijklmnop\"]\n\
             [providers.acme]\nkey_secret = \"ACME_KEY\"\n",
        );
        let found: Vec<(&str, &str)> = found
            .iter()
            .map(|i| (i.code.code, i.key.as_str()))
            .collect();
        assert_eq!(
            found,
            [
                ("config.credential_in_file", "display.theme"),
                ("config.credential_in_file", "skills.config.foo.list"),
            ]
        );
    }

    #[test]
    fn credentials_in_secret_and_enum_keys_are_refused_without_the_value() {
        let text = "[approvals]\nmode = \"sk-live-123456789\"\n\
                    [providers.x]\nkey_secret = \"AKIAIOSFODNN7EXAMPLE\"\n\
                    [providers.y]\nkey_secret = \"xoxb-123456789012-abcdef\"\n";
        let found = issues(text);
        let found: Vec<(&str, &str)> = found
            .iter()
            .map(|i| (i.code.code, i.key.as_str()))
            .collect();
        assert_eq!(
            found,
            [
                ("config.credential_in_file", "approvals.mode"),
                ("config.credential_in_file", "providers.x.key_secret"),
                ("config.credential_in_file", "providers.y.key_secret"),
            ]
        );
        for message in messages(text) {
            for value in ["sk-live", "AKIAIOSFODNN7", "xoxb-1234"] {
                assert!(!message.contains(value), "{message}");
            }
        }
    }

    #[test]
    fn misspelled_key_suggests_the_closest_key() {
        assert_eq!(
            messages("[agent]\nmax_iteration = 40\n"),
            ["unknown key 'agent.max_iteration' (did you mean 'agent.max_iterations'?)"]
        );
    }

    #[test]
    fn unknown_enum_value_lists_allowed_values() {
        let found = issues("[approvals]\nmode = \"of\"\n");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, "approvals.mode");
        assert_eq!(found[0].code.code, "config.invalid");
        assert_eq!(
            found[0].message,
            "approvals.mode: unknown value \"of\" (allowed: \"manual\", \"smart\", \"off\")"
        );
        assert_eq!(found[0].span, Some(19..23));
    }

    /// Scenario: Invalid strategy fails validation (spec: model-providers).
    #[test]
    fn unknown_pool_strategy_names_the_key_and_allowed_values() {
        let found = messages("[providers.openrouter]\npool_strategy = \"weighted\"\n");
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].starts_with(
                "providers.openrouter.pool_strategy: unknown value \"weighted\" (allowed: "
            ),
            "{found:?}"
        );
        assert!(found[0].contains("\"round_robin\""), "{found:?}");
    }

    /// Scenario: Unknown key in a platform hint (spec: agent-turn-loop,
    /// Platform hint overrides): only `replace` and `append` exist; the
    /// message names the full key, like every unknown-key error.
    #[test]
    fn unknown_platform_hint_key_is_refused() {
        let found = issues("[prompt.platform_hints.slack]\nprepend = \"x\"\n");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0].message,
            "unknown key 'prompt.platform_hints.slack.prepend'"
        );
        assert_eq!(found[0].code.kind.exit_code(), 78);
    }

    #[test]
    fn boolean_is_not_an_approval_mode() {
        assert_eq!(
            messages("[approvals]\nmode = false\n"),
            ["approvals.mode: expected one of \"manual\", \"smart\", \"off\", found boolean"]
        );
        for key in ["cron_mode", "single_query_mode", "unattended_mode"] {
            assert_eq!(
                messages(&format!("[approvals]\n{key} = false\n")),
                [format!(
                    "approvals.{key}: expected one of \"deny\", \"approve\", found boolean"
                )]
            );
        }
    }

    /// Scenario: Unknown provider name (spec: scheduled-jobs, Pluggable
    /// trigger provider).
    #[test]
    fn an_unknown_cron_provider_names_the_accepted_ones() {
        let found = issues("[cron]\nprovider = \"cronos\"\n");
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found[0].message,
            "unknown cron.provider 'cronos' (expected builtin|managed)"
        );
        assert_eq!(found[0].code.kind.exit_code(), 78);
    }

    /// Scenario: Invalid value fails validation (subagent-delegation).
    #[test]
    fn a_wrongly_typed_delegation_limit_names_the_key_and_its_range() {
        assert_eq!(
            messages("[delegation]\nmax_concurrent_children = \"lots\"\n"),
            [
                "delegation.max_concurrent_children: expected integer, found string (accepted: 1 to 64)"
            ]
        );
        assert_eq!(
            messages("[delegation]\nmax_spawn_depth = 9\n"),
            ["delegation.max_spawn_depth: must be between 1 and 4"]
        );
    }

    #[test]
    fn range_errors_use_the_spec_wording() {
        assert_eq!(
            messages("[compression]\nmax_attempts = 50\n"),
            ["compression.max_attempts: must be between 1 and 10"]
        );
        assert_eq!(
            messages("[compression]\nthreshold = 1.0\n"),
            ["compression.threshold: must be greater than 0 and less than 1"]
        );
        assert_eq!(
            messages("[agent]\nmax_iterations = -3\n"),
            ["agent.max_iterations: must be between 0 and 100000"]
        );
    }

    /// Scenario: Invalid interval is a validation error (spec: skills,
    /// Curator configuration and scheduling).
    #[test]
    fn curator_seconds_below_one_name_their_key() {
        for key in [
            "interval_secs",
            "min_idle_secs",
            "stale_after_secs",
            "archive_after_secs",
        ] {
            let found = messages(&format!("[curator]\n{key} = 0\n"));
            assert_eq!(found.len(), 1, "{found:?}");
            assert!(
                found[0].starts_with(&format!("curator.{key}: ")),
                "{found:?}"
            );
        }
        assert!(messages("[curator]\ninterval_secs = 60\n").is_empty());
    }

    #[test]
    fn unknown_tail_mode_names_lean_and_ratio() {
        assert_eq!(
            messages("[compression]\ntail_mode = \"fat\"\n"),
            ["compression.tail_mode: unknown value \"fat\" (allowed: \"lean\", \"ratio\")"]
        );
    }

    #[test]
    fn unknown_capabilities_are_refused_naming_the_string() {
        let messages = messages(
            "[security]\ncapabilities = [\"fs.read:<workspace>/**\", \"filesystem.write:/tmp\"]\n",
        );
        assert_eq!(
            messages,
            ["security.capabilities: unknown capability 'filesystem.write:/tmp'"]
        );
    }

    #[test]
    fn wrong_types_and_bad_secret_names() {
        assert_eq!(
            messages("[display]\ntheme = 3\n[providers.x]\nkey_secret = \"lower\"\n"),
            [
                "display.theme: expected string, found integer",
                "providers.x.key_secret: not a secret name (expected the name of a stored secret matching ^[A-Z][A-Z0-9_]{0,127}$)",
            ]
        );
    }

    #[test]
    fn untagged_values_pick_the_variant_by_type() {
        assert!(issues("[agent]\nintent_continuation = true\n").is_empty());
        assert!(issues("[agent]\nintent_continuation = [\"gpt\"]\n").is_empty());
        assert_eq!(
            messages("[agent]\nintent_continuation = \"sometimes\"\n"),
            ["agent.intent_continuation: unknown value \"sometimes\" (allowed: \"auto\")"]
        );
        assert_eq!(
            messages("[agent]\nintent_continuation = 3\n"),
            [
                "agent.intent_continuation: expected one of \"auto\" or boolean or array of string, found integer"
            ]
        );
    }

    #[test]
    fn machine_wide_sections_are_refused() {
        assert_eq!(
            messages("[update]\nauto_apply = true\n"),
            [
                "'update' is machine-wide and only valid in <home>/daemon.toml, not a profile config.toml"
            ]
        );
    }

    #[test]
    fn daemon_wide_keys_in_a_profile_config_point_to_daemon_toml() {
        assert_eq!(
            messages("[daemon.rpc]\nbind = \"127.0.0.1:0\"\n"),
            [
                "'daemon' is machine-wide and only valid in <home>/daemon.toml, not a profile config.toml"
            ]
        );
        assert_eq!(
            messages("[gateway]\non_all_adapters_down = \"exit\"\n[gateway.relay]\nurl = \"x\"\n"),
            [
                "'gateway.on_all_adapters_down' is machine-wide and only valid in <home>/daemon.toml, not a profile config.toml",
                "'gateway.relay' is machine-wide and only valid in <home>/daemon.toml, not a profile config.toml",
            ]
        );
    }

    #[test]
    fn gateway_values_are_validated() {
        assert_eq!(
            messages("[busy]\ninput_mode = \"loud\"\n"),
            [
                "busy.input_mode: unknown value \"loud\" (allowed: \"queue\", \"steer\", \"interrupt\")"
            ]
        );
        assert_eq!(
            messages("[gateway.platforms.telegram.busy]\ninput_mode = \"loud\"\n").len(),
            1
        );
        let negative = messages("[gateway.sessions]\nroute_max_age_secs = -1\n");
        assert_eq!(negative.len(), 1, "{negative:?}");
        assert!(negative[0].starts_with("gateway.sessions.route_max_age_secs"));
        let unknown = messages("[gateway.notifications]\nbackground = \"loud\"\n");
        assert!(unknown[0].contains("\"concise\""), "{unknown:?}");
    }

    #[test]
    fn every_problem_is_reported_in_file_order() {
        let found = messages("[agent]\nmax_iteration = 1\n[approvals]\nmode = \"x\"\n[bogus]\n");
        assert_eq!(found.len(), 3, "{found:?}");
        assert!(found[0].contains("agent.max_iteration"));
        assert!(found[1].contains("approvals.mode"));
        assert!(found[2].contains("'bogus'"));
    }
}
