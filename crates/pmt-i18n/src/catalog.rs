//! One catalog file: nested tables of strings flattened to dotted keys.
//!
//! Values are strings only. A file holding anything else is invalid as a
//! whole (its author meant something this host cannot render), while a
//! foreign catalog being converted (a Hermes YAML file) skips just the
//! entries that are not text and names them.

use std::collections::{BTreeMap, BTreeSet};

use toml::{Table, Value};

/// Dotted key → text.
pub type Entries = BTreeMap<String, String>;

/// Parses catalog TOML.
///
/// # Errors
///
/// A message naming the parse error or the first key whose value is not a
/// string.
pub fn parse(text: &str) -> Result<Entries, String> {
    let table: Table = text
        .parse()
        .map_err(|e: toml::de::Error| format!("invalid TOML: {}", e.message().trim_end()))?;
    let mut entries = Entries::new();
    flatten("", &table, &mut entries)?;
    Ok(entries)
}

fn flatten(prefix: &str, table: &Table, out: &mut Entries) -> Result<(), String> {
    for (key, value) in table {
        let dotted = join(prefix, key);
        match value {
            Value::String(text) => {
                out.insert(dotted, text.clone());
            }
            Value::Table(inner) => flatten(&dotted, inner, out)?,
            _ => return Err(format!("{dotted}: value is not text")),
        }
    }
    Ok(())
}

fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_owned()
    } else {
        format!("{prefix}.{key}")
    }
}

/// Flattens a JSON value (a YAML catalog read as JSON): strings become
/// entries, mappings nest, and every other value is skipped and its key
/// returned.
#[must_use]
pub fn from_json(value: &serde_json::Value) -> (Entries, Vec<String>) {
    let mut entries = Entries::new();
    let mut skipped = Vec::new();
    walk_json("", value, &mut entries, &mut skipped);
    (entries, skipped)
}

fn walk_json(
    prefix: &str,
    value: &serde_json::Value,
    out: &mut Entries,
    skipped: &mut Vec<String>,
) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, inner) in map {
                walk_json(&join(prefix, key), inner, out, skipped);
            }
        }
        serde_json::Value::String(text) => {
            out.insert(prefix.to_owned(), text.clone());
        }
        // An empty document is an empty catalog.
        serde_json::Value::Null if prefix.is_empty() => {}
        _ => skipped.push(prefix.to_owned()),
    }
}

/// Catalog TOML for `entries`, nested by key segment. An entry whose key
/// is also the prefix of another (`a = "x"` beside `a.b = "y"`) cannot nest
/// and is written as a quoted dotted key of its parent table.
#[must_use]
pub fn to_toml(entries: &Entries) -> String {
    let mut root = Table::new();
    for (key, text) in entries {
        insert(&mut root, key, text);
    }
    toml::to_string(&root).unwrap_or_default()
}

fn insert(table: &mut Table, key: &str, text: &str) {
    let Some((head, rest)) = key.split_once('.') else {
        table.insert(key.to_owned(), Value::String(text.to_owned()));
        return;
    };
    let slot = table
        .entry(head.to_owned())
        .or_insert_with(|| Value::Table(Table::new()));
    match slot {
        Value::Table(inner) => insert(inner, rest, text),
        _ => {
            table.insert(key.to_owned(), Value::String(text.to_owned()));
        }
    }
}

/// The `{placeholder}` names of a value: `{count}` (core) or `{0}` (TUI and
/// desktop). Braces around anything else are literal text.
#[must_use]
pub fn placeholders(text: &str) -> BTreeSet<&str> {
    let mut found = BTreeSet::new();
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        match after.find('}') {
            Some(end) if is_name(&after[..end]) => {
                found.insert(&after[..end]);
                rest = &after[end + 1..];
            }
            _ => rest = after,
        }
    }
    found
}

fn is_name(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Substitutes `{name}` with its argument's text in one pass (an argument
/// containing braces is never expanded again); unknown names stay as
/// written.
#[must_use]
pub fn render(template: &str, args: &[(&str, &dyn std::fmt::Display)]) -> String {
    let mut text = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        text.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let value = after.find('}').and_then(|end| {
            args.iter()
                .find(|(n, _)| *n == &after[..end])
                .map(|a| (end, a.1))
        });
        if let Some((end, value)) = value {
            text.push_str(&value.to_string());
            rest = &after[end + 1..];
        } else {
            text.push('{');
            rest = after;
        }
    }
    text.push_str(rest);
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_tables_flatten_to_dotted_keys() {
        let entries =
            parse("[approval]\ndenied = \"Nope.\"\n[slash.status]\nmodel = \"m\"\n").unwrap();
        assert_eq!(entries["approval.denied"], "Nope.");
        assert_eq!(entries["slash.status.model"], "m");
    }

    #[test]
    fn a_value_that_is_not_text_invalidates_the_file() {
        let err = parse("[approval]\ndenied = 3\n").unwrap_err();
        assert_eq!(err, "approval.denied: value is not text");
    }

    #[test]
    fn placeholders_are_names_or_positions_in_braces() {
        let names: Vec<_> = placeholders("{count} agents in {0}; {not a name} {}")
            .into_iter()
            .collect();
        assert_eq!(names, ["0", "count"]);
    }

    #[test]
    fn json_entries_that_are_not_text_are_skipped_and_named() {
        let value = serde_json::json!({"approval": {"denied": "Nein", "count": 3, "list": ["a"]}});
        let (entries, skipped) = from_json(&value);
        assert_eq!(entries["approval.denied"], "Nein");
        assert_eq!(skipped, ["approval.count", "approval.list"]);
    }

    #[test]
    fn toml_output_round_trips() {
        let mut entries = Entries::new();
        entries.insert("approval.denied".into(), "Nope.".into());
        entries.insert("gateway.stopped".into(), "Halt \"now\"".into());
        assert_eq!(parse(&to_toml(&entries)).unwrap(), entries);
    }

    #[test]
    fn render_substitutes_named_and_positional_arguments() {
        assert_eq!(render("{count} Agenten", &[("count", &3)]), "3 Agenten");
        assert_eq!(render("{0} / {1}", &[("0", &"a"), ("1", &"b")]), "a / b");
    }
}
