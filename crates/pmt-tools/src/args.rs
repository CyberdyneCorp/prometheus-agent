//! Tool argument parsing, coercion and schema validation (spec:
//! tool-system, Dispatch pipeline).
//!
//! Models send arguments as JSON text. The dispatcher parses it (anything
//! but an object is an error; empty text means `{}`), coerces string
//! scalars to the types the schema asks for (`"3"` -> `3`, `"true"` ->
//! `true`), then validates. Every violation is reported with its JSON
//! pointer, for example `/path: expected string`.
//!
//! The validator covers the JSON Schema keywords tool schemas use: `type`
//! (one or a list), `properties`, `required`, `additionalProperties`
//! (boolean or schema), `items`, `enum`, `const`, `minimum`, `maximum`,
//! `minLength`, `maxLength`, `minItems` and `maxItems`. Other keywords are
//! ignored, as JSON Schema prescribes for unknown keywords.

use serde_json::{Map, Value};

/// One schema violation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// JSON pointer of the offending value (`""` for the root).
    pub pointer: String,
    /// What is wrong.
    pub message: String,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let pointer = if self.pointer.is_empty() {
            "/"
        } else {
            &self.pointer
        };
        write!(f, "{pointer}: {}", self.message)
    }
}

/// Parses model-sent argument text into an object.
///
/// # Errors
///
/// A message for the model when the text is not JSON or not an object.
pub fn parse(raw: &str) -> Result<Map<String, Value>, String> {
    if raw.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(other) => Err(format!(
            "arguments must be a JSON object, got {}",
            type_name(&other)
        )),
        Err(err) => Err(format!("arguments are not valid JSON: {err}")),
    }
}

/// Converts string scalars to the scalar type `schema` asks for, in place,
/// wherever the string parses as that type and the schema does not also
/// accept strings.
pub fn coerce(schema: &Value, value: &mut Value) {
    match value {
        Value::String(text) => {
            if let Some(coerced) = coerce_scalar(schema, text) {
                *value = coerced;
            }
        }
        Value::Object(map) => coerce_properties(schema, map),
        Value::Array(items) => coerce_items(schema, items),
        _ => {}
    }
}

fn coerce_items(schema: &Value, items: &mut [Value]) {
    if let Some(item_schema) = schema.get("items") {
        for item in items {
            coerce(item_schema, item);
        }
    }
}

fn coerce_properties(schema: &Value, map: &mut Map<String, Value>) {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return;
    };
    for (key, child) in map.iter_mut() {
        if let Some(child_schema) = properties.get(key) {
            coerce(child_schema, child);
        }
    }
}

fn coerce_scalar(schema: &Value, text: &str) -> Option<Value> {
    let types = types_of(schema);
    if types.is_empty() || types.contains(&"string") {
        return None;
    }
    let text = text.trim();
    types.iter().find_map(|t| match *t {
        "integer" => text.parse::<i64>().ok().map(Value::from),
        "number" => text
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
            .map(Value::from),
        "boolean" => match text {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => None,
        },
        _ => None,
    })
}

fn types_of(schema: &Value) -> Vec<&str> {
    match schema.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(list)) => list.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    }
}

/// Every violation of `schema` by `value`, in document order.
#[must_use]
pub fn validate(schema: &Value, value: &Value) -> Vec<Violation> {
    let mut out = Vec::new();
    check(schema, value, "", &mut out);
    out
}

fn check(schema: &Value, value: &Value, pointer: &str, out: &mut Vec<Violation>) {
    let mut push = |message: String| {
        out.push(Violation {
            pointer: pointer.to_owned(),
            message,
        });
    };
    let types = types_of(schema);
    if !types.is_empty() && !types.iter().any(|t| matches_type(t, value)) {
        push(format!("expected {}", types.join(" or ")));
        return;
    }
    if let Some(message) = enum_violation(schema, value) {
        push(message);
    }
    for message in bound_violations(schema, value) {
        push(message);
    }
    match value {
        Value::Object(map) => check_object(schema, map, pointer, out),
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for (i, item) in items.iter().enumerate() {
                    check(item_schema, item, &format!("{pointer}/{i}"), out);
                }
            }
        }
        _ => {}
    }
}

fn check_object(schema: &Value, map: &Map<String, Value>, pointer: &str, out: &mut Vec<Violation>) {
    let properties = schema.get("properties").and_then(Value::as_object);
    for required in schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !map.contains_key(required) {
            out.push(Violation {
                pointer: format!("{pointer}/{}", escape(required)),
                message: "required property is missing".into(),
            });
        }
    }
    let additional = schema.get("additionalProperties");
    for (key, child) in map {
        let child_pointer = format!("{pointer}/{}", escape(key));
        match properties.and_then(|p| p.get(key)) {
            Some(child_schema) => check(child_schema, child, &child_pointer, out),
            None => match additional {
                Some(Value::Bool(false)) => out.push(Violation {
                    pointer: child_pointer,
                    message: "unknown property".into(),
                }),
                Some(extra @ Value::Object(_)) => check(extra, child, &child_pointer, out),
                _ => {}
            },
        }
    }
}

fn enum_violation(schema: &Value, value: &Value) -> Option<String> {
    if let Some(constant) = schema.get("const")
        && constant != value
    {
        return Some(format!("expected {constant}"));
    }
    let allowed = schema.get("enum")?.as_array()?;
    if allowed.contains(value) {
        return None;
    }
    let list: Vec<String> = allowed.iter().map(Value::to_string).collect();
    Some(format!("expected one of {}", list.join(", ")))
}

fn bound_violations(schema: &Value, value: &Value) -> Vec<String> {
    let bound = |key: &str| schema.get(key).and_then(Value::as_f64);
    let mut out = Vec::new();
    let mut at_least = |key: &str, actual: f64, what: &str| {
        if let Some(min) = bound(key)
            && actual < min
        {
            out.push(format!("{what} must be at least {min}"));
        }
    };
    let (size, what) = match value {
        Value::Number(n) => (n.as_f64(), "value"),
        Value::String(s) => (Some(count(s.chars().count())), "length"),
        Value::Array(items) => (Some(count(items.len())), "item count"),
        _ => (None, ""),
    };
    let Some(size) = size else {
        return out;
    };
    let (min_key, max_key) = match value {
        Value::Number(_) => ("minimum", "maximum"),
        Value::String(_) => ("minLength", "maxLength"),
        _ => ("minItems", "maxItems"),
    };
    at_least(min_key, size, what);
    if let Some(max) = bound(max_key)
        && size > max
    {
        out.push(format!("{what} must be at most {max}"));
    }
    out
}

/// A length as `f64` (lengths beyond `u32::MAX` saturate).
fn count(n: usize) -> f64 {
    f64::from(u32::try_from(n).unwrap_or(u32::MAX))
}

fn matches_type(name: &str, value: &Value) -> bool {
    match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        _ => true,
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// RFC 6901 escaping of one pointer segment.
fn escape(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::{coerce, parse, validate};

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "minLength": 1},
                "offset": {"type": "integer", "minimum": 1},
                "lines": {"type": ["integer", "null"]},
                "force": {"type": "boolean"},
                "mode": {"enum": ["replace", "append"]},
                "tags": {"type": "array", "items": {"type": "string"}, "maxItems": 2}
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    fn errors(value: &Value) -> Vec<String> {
        validate(&schema(), value)
            .iter()
            .map(ToString::to_string)
            .collect()
    }

    #[test]
    fn wrong_type_is_reported_by_pointer() {
        assert_eq!(errors(&json!({"path": 3})), ["/path: expected string"]);
        assert!(errors(&json!({"path": "a"})).is_empty());
    }

    #[test]
    fn every_violation_is_listed() {
        let found = errors(&json!({
            "offset": 0, "lines": "x", "mode": "insert", "tags": ["a", 2, "c"], "extra": 1
        }));
        assert_eq!(
            found,
            [
                "/path: required property is missing",
                "/extra: unknown property",
                "/lines: expected integer or null",
                "/mode: expected one of \"replace\", \"append\"",
                "/offset: value must be at least 1",
                "/tags: item count must be at most 2",
                "/tags/1: expected string",
            ]
        );
    }

    #[test]
    fn string_scalars_are_coerced_to_schema_types() {
        let mut value = json!({"path": "7", "offset": "12", "force": "true", "lines": "null?"});
        coerce(&schema(), &mut value);
        assert_eq!(
            value,
            json!({"path": "7", "offset": 12, "force": true, "lines": "null?"})
        );
    }

    #[test]
    fn arguments_must_be_an_object() {
        assert!(parse("").unwrap().is_empty());
        assert_eq!(
            parse("[1]").unwrap_err(),
            "arguments must be a JSON object, got an array"
        );
        assert!(
            parse("{")
                .unwrap_err()
                .starts_with("arguments are not valid JSON")
        );
        assert_eq!(parse(r#"{"a":1}"#).unwrap()["a"], 1);
    }
}
