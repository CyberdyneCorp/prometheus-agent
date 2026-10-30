//! Dotted-key edits on a format-preserving `toml_edit` document: setting a
//! value keeps its comments and every untouched byte, and missing parent
//! tables are created as implicit tables (`[providers.acme]` without an
//! empty `[providers]`).

use toml_edit::{ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value};

use crate::schema::FallbackEntry;

/// Sets `key` to `value`, creating missing parent tables.
///
/// # Errors
///
/// A message when a parent of `key` exists but is not a table.
pub(crate) fn set(doc: &mut DocumentMut, key: &str, value: Value) -> Result<(), String> {
    let segments: Vec<&str> = key.split('.').collect();
    set_in(doc.as_item_mut(), &segments, value, key)
}

fn set_in(item: &mut Item, segments: &[&str], mut value: Value, key: &str) -> Result<(), String> {
    let (first, rest) = segments.split_first().expect("a key has a segment");
    let inline = item.is_inline_table();
    let table = item
        .as_table_like_mut()
        .ok_or_else(|| format!("cannot set '{key}': a parent key is not a table"))?;
    if rest.is_empty() {
        // Keep the old value's inline comment and spacing.
        if let Some(old) = table.get(first).and_then(Item::as_value) {
            *value.decor_mut() = old.decor().clone();
        }
        table.insert(first, Item::Value(value));
        return Ok(());
    }
    if table.get(first).is_none() {
        table.insert(first, new_table(inline));
    }
    let child = table.get_mut(first).expect("inserted above");
    set_in(child, rest, value, key)
}

fn new_table(inline: bool) -> Item {
    if inline {
        Item::Value(Value::InlineTable(InlineTable::new()))
    } else {
        let mut table = Table::new();
        table.set_implicit(true);
        Item::Table(table)
    }
}

/// Removes `key`; returns whether it was present.
pub(crate) fn remove(doc: &mut DocumentMut, key: &str) -> bool {
    let (parent, leaf) = key.rsplit_once('.').unwrap_or(("", key));
    let mut item = doc.as_item_mut();
    if !parent.is_empty() {
        for segment in parent.split('.') {
            let Some(next) = item.as_table_like_mut().and_then(|t| t.get_mut(segment)) else {
                return false;
            };
            item = next;
        }
    }
    item.as_table_like_mut()
        .and_then(|t| t.remove(leaf))
        .is_some()
}

/// Replaces `fallback.chain` with `chain` as an array of tables
/// (`[[fallback.chain]]`), or removes it when `chain` is empty.
pub(crate) fn set_fallback_chain(doc: &mut DocumentMut, chain: &[FallbackEntry]) {
    if chain.is_empty() {
        remove(doc, "fallback.chain");
        return;
    }
    let mut tables = ArrayOfTables::new();
    for entry in chain {
        let mut table = Table::new();
        table.insert("provider", toml_edit::value(entry.provider.as_str()));
        table.insert("model", toml_edit::value(entry.model.as_str()));
        if let Some(url) = &entry.base_url {
            table.insert("base_url", toml_edit::value(url.as_str()));
        }
        if let Some(secret) = &entry.credential {
            table.insert("credential", toml_edit::value(secret.0.as_str()));
        }
        tables.push(table);
    }
    let root = doc.as_table_mut();
    if !root.contains_key("fallback") {
        let mut table = Table::new();
        table.set_implicit(true);
        root.insert("fallback", Item::Table(table));
    }
    if let Some(fallback) = root.get_mut("fallback").and_then(Item::as_table_like_mut) {
        fallback.insert("chain", Item::ArrayOfTables(tables));
    }
}

#[cfg(test)]
mod tests {
    use toml_edit::DocumentMut;

    use super::{remove, set};

    fn edited(text: &str, f: impl FnOnce(&mut DocumentMut)) -> String {
        let mut doc: DocumentMut = text.parse().unwrap();
        f(&mut doc);
        doc.to_string()
    }

    #[test]
    fn setting_a_value_keeps_comments_and_other_bytes() {
        let text =
            "# my model\n[model]\ndefault = \"x\"  # pinned\n\n[display]\ntheme = \"default\"\n";
        let out = edited(text, |doc| {
            set(doc, "display.theme", "mono".into()).unwrap();
        });
        assert_eq!(
            out,
            "# my model\n[model]\ndefault = \"x\"  # pinned\n\n[display]\ntheme = \"mono\"\n"
        );
        let out = edited(text, |doc| set(doc, "model.default", "y".into()).unwrap());
        assert!(out.contains("default = \"y\"  # pinned\n"), "{out}");
    }

    #[test]
    fn missing_parents_become_implicit_tables() {
        let out = edited("", |doc| {
            set(doc, "providers.acme.base_url", "http://x".into()).unwrap();
        });
        assert_eq!(out, "[providers.acme]\nbase_url = \"http://x\"\n");
    }

    #[test]
    fn inline_parents_stay_inline() {
        let out = edited(
            "[prompt]\nplatform_hints = { slack = { append = \"a\" } }\n",
            |doc| {
                set(doc, "prompt.platform_hints.slack.replace", "r".into()).unwrap();
            },
        );
        assert!(out.contains("replace = \"r\""), "{out}");
        assert!(out.contains("platform_hints = {"), "{out}");
    }

    #[test]
    fn a_scalar_parent_is_an_error() {
        let mut doc: DocumentMut = "agent = 3\n".parse().unwrap();
        assert!(set(&mut doc, "agent.max_iterations", 4.into()).is_err());
    }

    #[test]
    fn remove_reports_presence() {
        let mut doc: DocumentMut = "[agent]\nmax_iterations = 4\n".parse().unwrap();
        assert!(remove(&mut doc, "agent.max_iterations"));
        assert!(!remove(&mut doc, "agent.max_iterations"));
        assert!(!remove(&mut doc, "nothing.here"));
    }
}
