//! YAML frontmatter to JSON values (the event parser the file tools'
//! syntax check already links), with the line of the first error.
//!
//! Frontmatter is a small mapping, so a JSON value is all the skills code
//! needs: scalars resolve by the YAML 1.2 core schema (plain `true`, `12`,
//! `1.5`, `null`/`~`; quoted scalars stay strings), mapping keys become
//! strings, anchors and aliases are expanded, and only the first document
//! is read.

use std::collections::HashMap;

use saphyr_parser::{Event, Parser, ScalarStyle};
use serde_json::{Map, Number, Value};

/// Why frontmatter did not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YamlError {
    /// 1-based line within the YAML text.
    pub line: usize,
    /// What the parser said.
    pub reason: String,
}

enum Frame {
    Seq(Vec<Value>, usize),
    Map(Map<String, Value>, Option<String>, usize),
}

/// Parses the first document of `text`; an empty document is `null`.
///
/// # Errors
///
/// The parser's first error with its line.
pub fn parse(text: &str) -> Result<Value, YamlError> {
    let mut stack: Vec<Frame> = Vec::new();
    let mut anchors: HashMap<usize, Value> = HashMap::new();
    let mut root: Option<Value> = None;
    for event in Parser::new_from_str(text) {
        let (event, span) = event.map_err(|e| YamlError {
            line: e.marker().line(),
            reason: e.info().to_owned(),
        })?;
        let value = match event {
            Event::DocumentEnd if root.is_some() => break,
            Event::Scalar(text, style, anchor, tag) => {
                let forced_string = tag.is_some_and(|t| t.suffix == "str");
                let value = scalar(&text, style, forced_string);
                remember(&mut anchors, anchor, &value);
                value
            }
            Event::Alias(id) => anchors.get(&id).cloned().ok_or_else(|| YamlError {
                line: span.start.line(),
                reason: "alias to an unknown anchor".to_owned(),
            })?,
            Event::SequenceStart(anchor, _) => {
                stack.push(Frame::Seq(Vec::new(), anchor));
                continue;
            }
            Event::MappingStart(anchor, _) => {
                stack.push(Frame::Map(Map::new(), None, anchor));
                continue;
            }
            Event::SequenceEnd | Event::MappingEnd => {
                let (value, anchor) = match stack.pop() {
                    Some(Frame::Seq(items, anchor)) => (Value::Array(items), anchor),
                    Some(Frame::Map(map, _, anchor)) => (Value::Object(map), anchor),
                    None => continue,
                };
                remember(&mut anchors, anchor, &value);
                value
            }
            _ => continue,
        };
        push(&mut stack, &mut root, value);
    }
    Ok(root.unwrap_or(Value::Null))
}

fn remember(anchors: &mut HashMap<usize, Value>, anchor: usize, value: &Value) {
    if anchor != 0 {
        anchors.insert(anchor, value.clone());
    }
}

/// Places a finished value in its parent (or as the document root).
fn push(stack: &mut [Frame], root: &mut Option<Value>, value: Value) {
    match stack.last_mut() {
        None => *root = Some(value),
        Some(Frame::Seq(items, _)) => items.push(value),
        Some(Frame::Map(map, key, _)) => match key.take() {
            None => *key = Some(key_text(&value)),
            Some(k) => {
                map.insert(k, value);
            }
        },
    }
}

fn key_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// A scalar by the core schema.
fn scalar(text: &str, style: ScalarStyle, forced_string: bool) -> Value {
    if forced_string || style != ScalarStyle::Plain {
        return Value::String(text.to_owned());
    }
    match text {
        "" | "~" | "null" | "Null" | "NULL" => return Value::Null,
        "true" | "True" | "TRUE" => return Value::Bool(true),
        "false" | "False" | "FALSE" => return Value::Bool(false),
        _ => {}
    }
    number(text).unwrap_or_else(|| Value::String(text.to_owned()))
}

fn number(text: &str) -> Option<Value> {
    if let Some(hex) = text.strip_prefix("0x") {
        return i64::from_str_radix(hex, 16).ok().map(Value::from);
    }
    if let Some(oct) = text.strip_prefix("0o") {
        return i64::from_str_radix(oct, 8).ok().map(Value::from);
    }
    let digits = text.strip_prefix(['-', '+']).unwrap_or(text);
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        return text.parse::<i64>().ok().map(Value::from);
    }
    let numeric = digits
        .bytes()
        .next()
        .is_some_and(|b| b.is_ascii_digit() || b == b'.')
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
    use serde_json::json;

    use super::*;

    #[test]
    fn mappings_sequences_and_core_scalars() {
        let value = parse(
            "name: axolotl\ncount: 12\nratio: 1.5\non: true\nnothing: ~\nquoted: \"12\"\n\
             tags: [mlops, 'x']\nmetadata:\n  hermes:\n    tags:\n      - a\n",
        )
        .unwrap();
        assert_eq!(
            value,
            json!({
                "name": "axolotl", "count": 12, "ratio": 1.5, "on": true, "nothing": null,
                "quoted": "12", "tags": ["mlops", "x"],
                "metadata": {"hermes": {"tags": ["a"]}}
            })
        );
    }

    #[test]
    fn anchors_expand_and_empty_is_null() {
        assert_eq!(
            parse("a: &x [1, 2]\nb: *x\n").unwrap(),
            json!({"a": [1, 2], "b": [1, 2]})
        );
        assert_eq!(parse("").unwrap(), Value::Null);
    }

    #[test]
    fn an_unclosed_flow_sequence_reports_its_line() {
        let err = parse("name: x\ndescription: y\ntags: [a, b\n").unwrap_err();
        assert!(err.line >= 3, "{err:?}");
        assert!(!err.reason.is_empty());
    }
}
