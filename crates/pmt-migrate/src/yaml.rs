//! Hermes `config.yaml` as Hermes read it: `PyYAML` resolves plain scalars
//! by YAML 1.1, so an unquoted `off` is the boolean `false` (spec:
//! hermes-migration, Ambiguous YAML values resolve fail-closed). Besides
//! the resolved values the loader records every path where YAML 1.1 turned
//! a word into something else than the YAML 1.2 core schema (and a reader)
//! would: `yes`/`no`/`on`/`off`/`y`/`n` booleans and leading-zero octal
//! integers.

use std::collections::{BTreeSet, HashMap};

use saphyr_parser::{Event, Parser, ScalarStyle};
use serde_json::{Map, Number, Value};

/// A parsed document.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Doc {
    /// The values as Hermes saw them.
    pub value: Value,
    /// Dotted paths of the YAML-1.1-only coercions.
    pub coerced: BTreeSet<String>,
}

#[derive(Debug, Clone)]
enum Node {
    Map(Vec<(String, Node)>),
    Seq(Vec<Node>),
    Scalar { text: String, plain: bool },
}

enum Frame {
    Seq(Vec<Node>, usize),
    Map(Vec<(String, Node)>, Option<String>, usize),
}

/// Parses the first document of `text`.
///
/// # Errors
///
/// The parser's first error, with its 1-based line.
pub fn parse(text: &str) -> Result<Doc, String> {
    let root = tree(text)?;
    let mut doc = Doc::default();
    doc.value = resolve(&root, "", &mut doc.coerced);
    Ok(doc)
}

fn tree(text: &str) -> Result<Node, String> {
    let mut stack: Vec<Frame> = Vec::new();
    let mut anchors: HashMap<usize, Node> = HashMap::new();
    let mut root: Option<Node> = None;
    for event in Parser::new_from_str(text) {
        let (event, span) =
            event.map_err(|e| format!("line {}: {}", e.marker().line(), e.info()))?;
        let node = match event {
            Event::DocumentEnd if root.is_some() => break,
            Event::Scalar(text, style, anchor, tag) => {
                let forced_string = tag.is_some_and(|t| t.suffix == "str");
                let node = Node::Scalar {
                    text: text.to_string(),
                    plain: style == ScalarStyle::Plain && !forced_string,
                };
                remember(&mut anchors, anchor, &node);
                node
            }
            Event::Alias(id) => anchors
                .get(&id)
                .cloned()
                .ok_or_else(|| format!("line {}: alias to an unknown anchor", span.start.line()))?,
            Event::SequenceStart(anchor, _) => {
                stack.push(Frame::Seq(Vec::new(), anchor));
                continue;
            }
            Event::MappingStart(anchor, _) => {
                stack.push(Frame::Map(Vec::new(), None, anchor));
                continue;
            }
            Event::SequenceEnd | Event::MappingEnd => {
                let (node, anchor) = match stack.pop() {
                    Some(Frame::Seq(items, anchor)) => (Node::Seq(items), anchor),
                    Some(Frame::Map(entries, _, anchor)) => (Node::Map(entries), anchor),
                    None => continue,
                };
                remember(&mut anchors, anchor, &node);
                node
            }
            _ => continue,
        };
        push(&mut stack, &mut root, node);
    }
    Ok(root.unwrap_or(Node::Map(Vec::new())))
}

fn remember(anchors: &mut HashMap<usize, Node>, anchor: usize, node: &Node) {
    if anchor != 0 {
        anchors.insert(anchor, node.clone());
    }
}

fn push(stack: &mut [Frame], root: &mut Option<Node>, node: Node) {
    match stack.last_mut() {
        None => *root = Some(node),
        Some(Frame::Seq(items, _)) => items.push(node),
        Some(Frame::Map(entries, key, _)) => match key.take() {
            None => *key = Some(key_text(&node)),
            Some(k) => entries.push((k, node)),
        },
    }
}

fn key_text(node: &Node) -> String {
    match node {
        Node::Scalar { text, .. } => text.clone(),
        _ => String::new(),
    }
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

fn resolve(node: &Node, path: &str, coerced: &mut BTreeSet<String>) -> Value {
    match node {
        Node::Map(entries) => {
            let mut map = Map::new();
            for (k, v) in entries {
                let value = resolve(v, &join(path, k), coerced);
                map.insert(k.clone(), value);
            }
            Value::Object(map)
        }
        Node::Seq(items) => Value::Array(
            items
                .iter()
                .enumerate()
                .map(|(i, n)| resolve(n, &join(path, &i.to_string()), coerced))
                .collect(),
        ),
        Node::Scalar { text, plain } => {
            if !plain {
                return Value::String(text.clone());
            }
            let (value, only_yaml11) = scalar11(text);
            if only_yaml11 {
                coerced.insert(path.to_owned());
            }
            value
        }
    }
}

/// A plain scalar by `PyYAML`'s YAML 1.1 resolver; the flag says the
/// result differs from the YAML 1.2 core schema.
fn scalar11(text: &str) -> (Value, bool) {
    match text {
        "" | "~" | "null" | "Null" | "NULL" => return (Value::Null, false),
        "true" | "True" | "TRUE" => return (Value::Bool(true), false),
        "false" | "False" | "FALSE" => return (Value::Bool(false), false),
        "yes" | "Yes" | "YES" | "on" | "On" | "ON" | "y" | "Y" => {
            return (Value::Bool(true), true);
        }
        "no" | "No" | "NO" | "off" | "Off" | "OFF" | "n" | "N" => {
            return (Value::Bool(false), true);
        }
        _ => {}
    }
    let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
    if digits.len() > 1
        && digits.starts_with('0')
        && digits.bytes().all(|b| (b'0'..=b'7').contains(&b))
        && let Ok(n) = i64::from_str_radix(digits, 8)
    {
        let n = if text.starts_with('-') { -n } else { n };
        return (Value::from(n), true);
    }
    (
        number(text).unwrap_or_else(|| Value::String(text.to_owned())),
        false,
    )
}

fn number(text: &str) -> Option<Value> {
    if let Some(hex) = text.strip_prefix("0x") {
        return i64::from_str_radix(hex, 16).ok().map(Value::from);
    }
    let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        return text.parse::<i64>().ok().map(Value::from);
    }
    let numeric = digits
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_digit() || b == b'.')
        && digits.contains(|c: char| c.is_ascii_digit())
        && digits
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'-' | b'+'));
    if numeric {
        return text
            .parse::<f64>()
            .ok()
            .and_then(Number::from_f64)
            .map(Value::Number);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_1_1_words_become_booleans_and_are_flagged() {
        let doc = parse(
            "approvals:\n  cron_mode: off\n  mode: 'off'\nflag: yes\nreal: true\nmode: 0755\nn: 12\n",
        )
        .unwrap();
        assert_eq!(doc.value["approvals"]["cron_mode"], Value::Bool(false));
        assert_eq!(doc.value["approvals"]["mode"], Value::from("off"));
        assert_eq!(doc.value["flag"], Value::Bool(true));
        assert_eq!(doc.value["real"], Value::Bool(true));
        assert_eq!(doc.value["mode"], Value::from(493));
        assert_eq!(doc.value["n"], Value::from(12));
        let coerced: Vec<&str> = doc.coerced.iter().map(String::as_str).collect();
        assert_eq!(coerced, ["approvals.cron_mode", "flag", "mode"]);
    }

    #[test]
    fn anchors_floats_and_lists_resolve() {
        let doc = parse("a: &x {b: 1.5}\nc: *x\nd: [one, 2]\n").unwrap();
        assert_eq!(doc.value["c"]["b"], Value::from(1.5));
        assert_eq!(doc.value["d"][1], Value::from(2));
        assert!(parse("a: [").is_err());
    }
}
