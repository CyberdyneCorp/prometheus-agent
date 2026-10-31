//! Configuration layering (spec: configuration-and-profiles, Configuration
//! layering): schema defaults < the profile `config.toml` < `--set
//! key=value` overrides, which are never persisted. Every effective value
//! records the layer that supplied it, for `config explain`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};
use serde_json::Value;

use crate::catalog;
use crate::credential;
use crate::edit;
use crate::parse;
use crate::schema::Config;
use crate::validate::{self, Kind, Node};

/// Where an effective value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The schema default.
    Default,
    /// The profile `config.toml`, at this 1-based line.
    File {
        /// The file.
        path: PathBuf,
        /// The line of the key.
        line: usize,
    },
    /// A `--set` override of this invocation.
    Override,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Default => f.write_str("default"),
            Self::File { path, line } => {
                let name = path.file_name().map_or_else(
                    || path.display().to_string(),
                    |n| n.to_string_lossy().into_owned(),
                );
                write!(f, "{name}:{line}")
            }
            Self::Override => f.write_str("--set"),
        }
    }
}

/// Typed `--set key=value` overrides of one invocation.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    entries: Vec<(String, toml_edit::Value)>,
}

impl Overrides {
    /// Parses `key=value` arguments; each value is typed by its key's
    /// schema (see [`typed_value`]).
    ///
    /// # Errors
    ///
    /// `config.invalid_argument` (exit 2) for a malformed argument, an
    /// unknown key or an invalid value.
    pub fn parse<S: AsRef<str>>(args: &[S]) -> Result<Self, Error> {
        let mut entries = Vec::new();
        for arg in args {
            let arg = arg.as_ref();
            let (key, raw) = arg.split_once('=').ok_or_else(|| {
                Error::new(
                    &codes::CONFIG_INVALID_ARGUMENT,
                    format!("--set expects key=value, got '{arg}'"),
                )
            })?;
            let key = key.trim();
            entries.push((key.to_owned(), typed_value(key, raw)?));
        }
        Ok(Self { entries })
    }

    /// Whether there are no overrides.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Types a command-line value by its key's schema: a key that takes a
/// string takes the text as is (unless it is a quoted TOML string), any
/// other key parses it as a TOML value (`10`, `true`, `["a"]`). So
/// `approvals.mode=off` is the string `"off"` and
/// `agent.max_iterations=10` the integer 10.
///
/// # Errors
///
/// `config.invalid_argument` (exit 2): unknown key, a section instead of a
/// value, or a value the schema rejects (type, enum, range);
/// `config.credential_value` (exit 2) for a credential-looking value in a
/// non-secret key.
pub fn typed_value(key: &str, raw: &str) -> Result<toml_edit::Value, Error> {
    let info =
        catalog::lookup(key).ok_or_else(|| argument_error(&[validate::unknown_key(key, None)]))?;
    if info.section {
        return Err(Error::new(
            &codes::CONFIG_INVALID_ARGUMENT,
            format!("'{key}' is a section; set one of its keys"),
        ));
    }
    let parsed = raw.trim().parse::<toml_edit::Value>();
    let value = match parsed {
        Ok(value) if !takes_string(info.schema) || value.is_str() => value,
        _ if accepts_string(info.schema) => toml_edit::Value::from(raw),
        Ok(value) => value,
        Err(_) => toml_edit::Value::from(raw),
    };
    let issues = validate::value(key, &Node::from_value(&value));
    if issues.is_empty() {
        Ok(value)
    } else {
        Err(argument_error(&issues))
    }
}

/// Every issue of a command-line value is a usage error (exit 2); a
/// credential keeps its own code so scripts can tell it apart.
fn argument_error(issues: &[validate::Issue]) -> Error {
    let lines: Vec<&str> = issues.iter().map(|i| i.message.as_str()).collect();
    let credential = issues
        .iter()
        .any(|i| i.code == &codes::CONFIG_CREDENTIAL_IN_FILE);
    if credential {
        Error::new(&codes::CONFIG_CREDENTIAL_VALUE, lines.join("\n")).with_hint(credential::HINT)
    } else {
        Error::new(&codes::CONFIG_INVALID_ARGUMENT, lines.join("\n"))
            .with_hint("see the key with: prometheus config explain <key>")
    }
}

/// Strings only: plain strings and string enums.
fn takes_string(schema: &'static Value) -> bool {
    let only_string = match schema.get("type") {
        Some(Value::String(t)) => t == "string",
        Some(Value::Array(types)) => types.iter().filter(|t| *t != "null").all(|t| t == "string"),
        _ => false,
    };
    only_string || catalog::allowed_values(schema).is_some_and(|v| v.iter().all(Value::is_string))
}

/// A string is one of the accepted shapes (untagged values included).
fn accepts_string(schema: &'static Value) -> bool {
    takes_string(schema)
        || schema
            .get("anyOf")
            .and_then(Value::as_array)
            .is_some_and(|variants| variants.iter().any(|v| takes_string(catalog::resolve(v))))
}

/// The effective configuration of a profile and where each value came from.
#[derive(Debug, Clone)]
pub struct Layered {
    config: Config,
    effective: Value,
    path: PathBuf,
    file_keys: BTreeMap<String, usize>,
    override_keys: BTreeSet<String>,
}

impl Layered {
    /// Validates `text` (the contents of `path`) and applies `overrides`.
    ///
    /// # Errors
    ///
    /// The file's validation errors (exit 78), or `config.invalid_argument`
    /// when the overrides do not fit the file (exit 2).
    pub(crate) fn build(path: &Path, text: &str, overrides: &Overrides) -> Result<Self, Error> {
        let file_config = parse::config(path, text)?;
        let spanned = parse::document(path, text)?;
        let mut file_keys = BTreeMap::new();
        collect_leaves(
            &Node::from_item(spanned.as_item()),
            "",
            text,
            &mut file_keys,
        );
        let config = if overrides.is_empty() {
            file_config
        } else {
            apply(text, overrides)?
        };
        let effective = serde_json::to_value(&config).expect("a config serializes");
        Ok(Self {
            config,
            effective,
            path: path.to_owned(),
            file_keys,
            override_keys: overrides.entries.iter().map(|(k, _)| k.clone()).collect(),
        })
    }

    /// The effective configuration.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The effective value of `key` as JSON; `None` when unset.
    #[must_use]
    pub fn value(&self, key: &str) -> Option<&Value> {
        let mut pointer = String::new();
        for segment in key.split('.') {
            pointer.push('/');
            pointer.push_str(&segment.replace('~', "~0").replace('/', "~1"));
        }
        self.effective.pointer(&pointer)
    }

    /// The layer that supplied `key` (or, for a section, any key in it).
    #[must_use]
    pub fn source(&self, key: &str) -> Source {
        let covers = |k: &String| k == key || k.starts_with(&format!("{key}."));
        if self.override_keys.iter().any(covers) {
            return Source::Override;
        }
        match self.file_keys.iter().find(|(k, _)| covers(k)) {
            Some((_, line)) => Source::File {
                path: self.path.clone(),
                line: *line,
            },
            None => Source::Default,
        }
    }

    /// Every key set by the file or an override whose effective value
    /// differs from its default, in key order.
    #[must_use]
    pub fn changed(&self) -> Vec<String> {
        let keys: BTreeSet<&String> = self.file_keys.keys().chain(&self.override_keys).collect();
        keys.into_iter()
            .filter(|key| {
                let default = catalog::lookup(key).and_then(|info| info.default);
                self.value(key) != default
            })
            .cloned()
            .collect()
    }
}

/// The file with the overrides applied, as a typed config.
fn apply(text: &str, overrides: &Overrides) -> Result<Config, Error> {
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
        Error::new(&codes::CONFIG_PARSE, e.message().to_owned())
    })?;
    for (key, value) in &overrides.entries {
        edit::set(&mut doc, key, value.clone())
            .map_err(|message| Error::new(&codes::CONFIG_INVALID_ARGUMENT, message))?;
    }
    toml::from_str(&doc.to_string()).map_err(|e| {
        Error::new(
            &codes::CONFIG_INVALID_ARGUMENT,
            format!("--set: {}", e.message()),
        )
    })
}

/// Dotted keys of every value in the file (tables are walked, arrays are
/// values) with the 1-based line of their key.
fn collect_leaves(node: &Node, prefix: &str, text: &str, out: &mut BTreeMap<String, usize>) {
    let Kind::Table(entries) = &node.kind else {
        return;
    };
    for (name, key_span, child) in entries {
        let key = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        if matches!(child.kind, Kind::Table(_)) {
            collect_leaves(child, &key, text, out);
        } else {
            let line = key_span
                .as_ref()
                .map_or(0, |span| parse::line_col(text, span.start).0);
            out.insert(key, line);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Layered, Overrides, Source, typed_value};
    use crate::schema::ApprovalMode;

    fn layered(text: &str, sets: &[&str]) -> Layered {
        Layered::build(
            Path::new("/p/config.toml"),
            text,
            &Overrides::parse(sets).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn layers_defaults_file_then_overrides() {
        let text = "[agent]\nmax_iterations = 40\n";
        let config = layered(text, &[]);
        assert_eq!(config.config().agent.max_iterations, 40);
        assert_eq!(config.config().agent.tool_concurrency, 8);
        let over = layered(text, &["agent.max_iterations=10"]);
        assert_eq!(over.config().agent.max_iterations, 10);
        assert_eq!(over.source("agent.max_iterations"), Source::Override);
    }

    #[test]
    fn source_names_the_file_line() {
        let text = "# c\n[agent]\nmax_iterations = 40\n";
        let config = layered(text, &[]);
        let source = config.source("agent.max_iterations");
        assert_eq!(source.to_string(), "config.toml:3");
        assert_eq!(config.source("agent.tool_concurrency"), Source::Default);
        assert_eq!(config.source("agent").to_string(), "config.toml:3");
    }

    #[test]
    fn set_values_are_typed_by_their_key() {
        assert_eq!(
            typed_value("agent.max_iterations", "10")
                .unwrap()
                .as_integer(),
            Some(10)
        );
        assert_eq!(
            typed_value("approvals.mode", "off").unwrap().as_str(),
            Some("off")
        );
        assert_eq!(
            typed_value("display.timestamp_format", "%H:%M")
                .unwrap()
                .as_str(),
            Some("%H:%M")
        );
        assert_eq!(
            typed_value("model.default", "1.5").unwrap().as_str(),
            Some("1.5")
        );
        assert_eq!(
            typed_value("display.theme", "\"mono\"").unwrap().as_str(),
            Some("mono")
        );
        assert_eq!(
            typed_value("agent.intent_continuation", "true")
                .unwrap()
                .as_bool(),
            Some(true)
        );
        assert_eq!(
            typed_value("agent.intent_continuation", "auto")
                .unwrap()
                .as_str(),
            Some("auto")
        );
        assert!(
            typed_value("tools.enabled_toolsets", "[\"core\", \"web\"]")
                .unwrap()
                .is_array()
        );
        let config = layered("", &["approvals.mode=off"]);
        assert_eq!(config.config().approvals.mode, ApprovalMode::Off);
    }

    #[test]
    fn invalid_set_values_are_usage_errors() {
        let err = typed_value("agent.max_iterations", "ten").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert_eq!(
            err.message(),
            "agent.max_iterations: expected integer, found string"
        );
        let err = typed_value("agent.max_iterations", "-3").unwrap_err();
        assert_eq!(
            err.message(),
            "agent.max_iterations: must be between 0 and 100000"
        );
        let err = typed_value("approvals.mode", "false").unwrap_err();
        assert_eq!(
            err.message(),
            "approvals.mode: unknown value \"false\" (allowed: \"manual\", \"smart\", \"off\")"
        );
        let err = typed_value("agent.max_iteration", "1").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert_eq!(
            err.message(),
            "unknown key 'agent.max_iteration' (did you mean 'agent.max_iterations'?)"
        );
        assert_eq!(typed_value("agent", "1").unwrap_err().exit_code(), 2);
        assert!(Overrides::parse(&["noequals"]).is_err());
    }

    #[test]
    fn changed_lists_values_that_differ_from_defaults() {
        let config = layered(
            "[agent]\nmax_iterations = 500\ntool_concurrency = 4\n",
            &["display.theme=mono"],
        );
        assert_eq!(
            config.changed(),
            ["agent.tool_concurrency", "display.theme"]
        );
    }
}
