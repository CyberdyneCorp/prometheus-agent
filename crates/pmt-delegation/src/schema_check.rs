//! `output_schema` support (spec: subagent-delegation, Task validation;
//! `output_schema` validation never discards results): [`check`] decides
//! whether a schema is a well-formed draft 2020-12 JSON Schema (every
//! keyword it uses has the shape the meta-schema requires), and
//! [`validate`] checks an answer against it.
//!
//! [`validate`] interprets the assertion keywords children's schemas use:
//! `type`, `enum`, `const`, the numeric, string-length, array-size and
//! property-count bounds, `pattern`, `required`, `properties`,
//! `additionalProperties`, `items`, `prefixItems`, `allOf`, `anyOf`,
//! `oneOf`, `not` and local `$ref`s into `$defs`. Annotations and
//! keywords it does not know are ignored, as JSON Schema prescribes.

use regex::Regex;
use serde_json::{Map, Value};

const TYPES: &[&str] = &[
    "null", "boolean", "object", "array", "number", "string", "integer",
];
const SCHEMA_MAPS: &[&str] = &[
    "properties",
    "patternProperties",
    "$defs",
    "definitions",
    "dependentSchemas",
];
const SUBSCHEMAS: &[&str] = &[
    "items",
    "additionalProperties",
    "not",
    "if",
    "then",
    "else",
    "contains",
    "propertyNames",
    "unevaluatedItems",
    "unevaluatedProperties",
];
const SCHEMA_ARRAYS: &[&str] = &["prefixItems", "allOf", "anyOf", "oneOf"];
const COUNTS: &[&str] = &[
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
    "minProperties",
    "maxProperties",
    "minContains",
    "maxContains",
];
const NUMBERS: &[&str] = &["minimum", "maximum", "exclusiveMinimum", "exclusiveMaximum"];
const DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";

/// Checks that `schema` is a well-formed draft 2020-12 schema.
///
/// # Errors
///
/// What is wrong, with its JSON pointer.
pub fn check(schema: &Value) -> Result<(), String> {
    if let Some(dialect) = schema.get("$schema")
        && dialect.as_str().map(|d| d.trim_end_matches('#')) != Some(DIALECT)
    {
        return Err(format!("/$schema: only {DIALECT} is supported"));
    }
    check_at(schema, "")
}

fn check_at(schema: &Value, at: &str) -> Result<(), String> {
    let object = match schema {
        Value::Bool(_) => return Ok(()),
        Value::Object(object) => object,
        _ => return Err(format!("{}: a schema is an object or a boolean", root(at))),
    };
    for (key, value) in object {
        let here = format!("{at}/{key}");
        check_keyword(key, value, &here)?;
    }
    Ok(())
}

fn check_keyword(key: &str, value: &Value, at: &str) -> Result<(), String> {
    let wrong = |what: &str| Err(format!("{at}: must be {what}"));
    match key {
        "type" => check_type(value, at),
        k if SCHEMA_MAPS.contains(&k) => match value.as_object() {
            Some(map) => map
                .iter()
                .try_for_each(|(name, s)| check_at(s, &format!("{at}/{name}"))),
            None => wrong("an object of schemas"),
        },
        k if SUBSCHEMAS.contains(&k) => check_at(value, at),
        k if SCHEMA_ARRAYS.contains(&k) => match value.as_array() {
            Some(items) if !items.is_empty() => items
                .iter()
                .enumerate()
                .try_for_each(|(i, s)| check_at(s, &format!("{at}/{i}"))),
            _ => wrong("a non-empty array of schemas"),
        },
        "required" => match value.as_array() {
            Some(items) if items.iter().all(Value::is_string) && unique(items) => Ok(()),
            _ => wrong("an array of unique strings"),
        },
        "enum" => value
            .as_array()
            .map_or_else(|| wrong("an array"), |_| Ok(())),
        "pattern" => match value.as_str().map(Regex::new) {
            Some(Ok(_)) => Ok(()),
            _ => wrong("a valid regular expression"),
        },
        "multipleOf" => match value.as_f64() {
            Some(n) if n > 0.0 => Ok(()),
            _ => wrong("a number greater than 0"),
        },
        "uniqueItems" => value
            .as_bool()
            .map_or_else(|| wrong("a boolean"), |_| Ok(())),
        "$ref" | "$id" | "$anchor" | "title" | "description" | "format" => {
            value.as_str().map_or_else(|| wrong("a string"), |_| Ok(()))
        }
        k if COUNTS.contains(&k) => match value.as_u64() {
            Some(_) => Ok(()),
            None => wrong("a non-negative integer"),
        },
        k if NUMBERS.contains(&k) => value.as_f64().map_or_else(|| wrong("a number"), |_| Ok(())),
        _ => Ok(()),
    }
}

fn check_type(value: &Value, at: &str) -> Result<(), String> {
    let known = |v: &Value| v.as_str().is_some_and(|t| TYPES.contains(&t));
    let ok = match value {
        Value::String(_) => known(value),
        Value::Array(items) => !items.is_empty() && items.iter().all(known) && unique(items),
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{at}: must be one of {} or a list of them",
            TYPES.join(", ")
        ))
    }
}

fn unique(items: &[Value]) -> bool {
    items
        .iter()
        .enumerate()
        .all(|(i, a)| items[..i].iter().all(|b| a != b))
}

fn root(at: &str) -> &str {
    if at.is_empty() { "/" } else { at }
}

/// The violations of `value` against `schema` (empty when it matches).
#[must_use]
pub fn validate(schema: &Value, value: &Value) -> Vec<String> {
    let defs = schema
        .get("$defs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    Walker { defs: &defs }.check(schema, value, "", &mut out);
    out
}

struct Walker<'a> {
    defs: &'a Map<String, Value>,
}

impl Walker<'_> {
    fn check(&self, schema: &Value, value: &Value, at: &str, out: &mut Vec<String>) {
        let schema = self.resolve(schema);
        let object = match schema {
            Value::Bool(false) => return out.push(format!("{}: no value is allowed", root(at))),
            Value::Object(object) => object,
            _ => return,
        };
        if let Some(types) = object.get("type")
            && !type_matches(types, value)
        {
            return out.push(format!("{}: expected {}", root(at), type_text(types)));
        }
        if let Some(Value::Array(allowed)) = object.get("enum")
            && !allowed.contains(value)
        {
            out.push(format!("{}: not one of the allowed values", root(at)));
        }
        if let Some(expected) = object.get("const")
            && expected != value
        {
            out.push(format!("{}: must be {expected}", root(at)));
        }
        bounds(object, value, at, out);
        self.combinators(object, value, at, out);
        match value {
            Value::Object(map) => self.object(object, map, at, out),
            Value::Array(items) => self.array(object, items, at, out),
            _ => {}
        }
    }

    fn resolve<'s>(&'s self, schema: &'s Value) -> &'s Value {
        schema
            .get("$ref")
            .and_then(Value::as_str)
            .and_then(|r| r.strip_prefix("#/$defs/"))
            .and_then(|name| self.defs.get(name))
            .unwrap_or(schema)
    }

    fn combinators(
        &self,
        object: &Map<String, Value>,
        value: &Value,
        at: &str,
        out: &mut Vec<String>,
    ) {
        let passes = |s: &Value| {
            let mut inner = Vec::new();
            self.check(s, value, at, &mut inner);
            inner.is_empty()
        };
        if let Some(Value::Array(all)) = object.get("allOf") {
            for s in all {
                self.check(s, value, at, out);
            }
        }
        if let Some(Value::Array(any)) = object.get("anyOf")
            && !any.iter().any(passes)
        {
            out.push(format!("{}: matches none of anyOf", root(at)));
        }
        if let Some(Value::Array(one)) = object.get("oneOf")
            && one.iter().filter(|s| passes(s)).count() != 1
        {
            out.push(format!("{}: must match exactly one of oneOf", root(at)));
        }
        if let Some(not) = object.get("not")
            && passes(not)
        {
            out.push(format!("{}: matches a schema it must not", root(at)));
        }
    }

    fn object(
        &self,
        schema: &Map<String, Value>,
        map: &Map<String, Value>,
        at: &str,
        out: &mut Vec<String>,
    ) {
        if let Some(Value::Array(required)) = schema.get("required") {
            for name in required.iter().filter_map(Value::as_str) {
                if !map.contains_key(name) {
                    out.push(format!("{}: missing required property '{name}'", root(at)));
                }
            }
        }
        let properties = schema.get("properties").and_then(Value::as_object);
        for (name, value) in map {
            let here = format!("{at}/{name}");
            match properties.and_then(|p| p.get(name)) {
                Some(property) => self.check(property, value, &here, out),
                None => match schema.get("additionalProperties") {
                    Some(Value::Bool(false)) => {
                        out.push(format!("{here}: property is not allowed"));
                    }
                    Some(extra) => self.check(extra, value, &here, out),
                    None => {}
                },
            }
        }
    }

    fn array(&self, schema: &Map<String, Value>, items: &[Value], at: &str, out: &mut Vec<String>) {
        let prefix = schema
            .get("prefixItems")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        for (i, item) in items.iter().enumerate() {
            let here = format!("{at}/{i}");
            if let Some(s) = prefix.get(i).or_else(|| schema.get("items")) {
                self.check(s, item, &here, out);
            }
        }
    }
}

fn bounds(object: &Map<String, Value>, value: &Value, at: &str, out: &mut Vec<String>) {
    let limit = |key: &str| object.get(key).and_then(Value::as_f64);
    #[allow(clippy::cast_precision_loss)] // sizes far below 2^52
    let size = match value {
        Value::String(s) => Some(("minLength", "maxLength", s.chars().count() as f64)),
        Value::Array(a) => Some(("minItems", "maxItems", a.len() as f64)),
        Value::Object(o) => Some(("minProperties", "maxProperties", o.len() as f64)),
        _ => None,
    };
    if let Some((min, max, n)) = size
        && (limit(min).is_some_and(|m| n < m) || limit(max).is_some_and(|m| n > m))
    {
        out.push(format!("{}: size {n} is out of bounds", root(at)));
    }
    if let Some(n) = value.as_f64() {
        let low = limit("minimum").is_some_and(|m| n < m)
            || limit("exclusiveMinimum").is_some_and(|m| n <= m);
        let high = limit("maximum").is_some_and(|m| n > m)
            || limit("exclusiveMaximum").is_some_and(|m| n >= m);
        if low || high {
            out.push(format!("{}: {n} is out of range", root(at)));
        }
    }
    if let (Some(pattern), Some(text)) = (
        object.get("pattern").and_then(Value::as_str),
        value.as_str(),
    ) && Regex::new(pattern).is_ok_and(|re| !re.is_match(text))
    {
        out.push(format!("{}: does not match {pattern}", root(at)));
    }
}

fn type_matches(types: &Value, value: &Value) -> bool {
    match types {
        Value::String(name) => is_type(name, value),
        Value::Array(names) => names
            .iter()
            .filter_map(Value::as_str)
            .any(|n| is_type(n, value)),
        _ => true,
    }
}

fn is_type(name: &str, value: &Value) -> bool {
    match name {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "number" => value.is_number(),
        "integer" => {
            value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|f| f.fract() == 0.0)
        }
        _ => false,
    }
}

fn type_text(types: &Value) -> String {
    match types {
        Value::Array(names) => names
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" or "),
        other => other.as_str().unwrap_or("?").to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn well_formed_schemas_pass_and_broken_ones_name_the_keyword() {
        check(
            &json!({"type": "object", "properties": {"n": {"type": "integer"}}, "required": ["n"]}),
        )
        .unwrap();
        check(&json!(true)).unwrap();
        assert!(
            check(&json!({"type": "objekt"}))
                .unwrap_err()
                .contains("/type")
        );
        assert!(
            check(&json!({"required": "n"}))
                .unwrap_err()
                .contains("/required")
        );
        assert!(
            check(&json!({"properties": {"a": 3}}))
                .unwrap_err()
                .contains("/properties/a")
        );
        assert!(check(&json!({"pattern": "("})).is_err());
        assert!(check(&json!({"$schema": "http://json-schema.org/draft-04/schema#"})).is_err());
    }

    #[test]
    fn answers_are_checked_against_the_schema() {
        let schema = json!({
            "type": "object",
            "properties": {"score": {"type": "integer", "minimum": 0}, "tags": {"type": "array", "items": {"type": "string"}}},
            "required": ["score"],
            "additionalProperties": false
        });
        assert!(validate(&schema, &json!({"score": 3, "tags": ["a"]})).is_empty());
        let errors = validate(&schema, &json!({"score": -1, "tags": [1], "extra": true}));
        assert_eq!(errors.len(), 3, "{errors:?}");
        assert_eq!(
            validate(&schema, &json!({})),
            ["/: missing required property 'score'"]
        );
    }
}
