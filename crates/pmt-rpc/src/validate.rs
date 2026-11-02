//! Values against their contract's JSON Schema (spec: workspace-architecture,
//! Strict parameter validation and contract violations; Protocol handshake
//! and versioning). The schemas are the ones schemars generates for the
//! contract types, so only the keywords those use are interpreted: `$ref`
//! into `$defs`, `type`, `properties`, `required`, `additionalProperties`,
//! `items`, `enum`, `const`, `anyOf` / `oneOf` and the `x-since` annotation.
//! Every violation names the JSON pointer of the offending value.

use std::fmt::Write as _;

use serde_json::{Map, Value};

/// Why a value does not match its schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// JSON pointer of the offending value (`/sesion_id`); empty for the
    /// root.
    pub path: String,
    /// The value names a field the schema does not declare.
    pub unknown_field: bool,
    /// What is wrong.
    pub message: String,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let at = if self.path.is_empty() {
            "/"
        } else {
            &self.path
        };
        write!(f, "{at}: {}", self.message)
    }
}

/// Checks `value` against `schema`, resolving `$ref`s in `defs`.
///
/// # Errors
///
/// The first violation found, depth first.
pub fn validate(value: &Value, schema: &Value, defs: &Map<String, Value>) -> Result<(), Violation> {
    Walker { defs }.check(value, schema, &mut String::new())
}

/// Removes from `value` every object field whose schema carries an
/// `x-since` minor newer than `minor`, so an older client never receives
/// fields it does not know.
pub fn strip_newer(value: &mut Value, schema: &Value, defs: &Map<String, Value>, minor: u16) {
    Walker { defs }.strip(value, schema, minor);
}

struct Walker<'a> {
    defs: &'a Map<String, Value>,
}

fn violation(path: &str, message: impl Into<String>) -> Violation {
    Violation {
        path: path.to_owned(),
        unknown_field: false,
        message: message.into(),
    }
}

/// `~` and `/` escaped as RFC 6901 requires.
fn pointer_segment(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

impl Walker<'_> {
    fn resolve<'s>(&'s self, schema: &'s Value) -> &'s Value {
        let mut schema = schema;
        while let Some(target) = schema.get("$ref").and_then(Value::as_str) {
            let name = target.rsplit('/').next().unwrap_or_default();
            match self.defs.get(name) {
                Some(def) => schema = def,
                None => break,
            }
        }
        schema
    }

    fn check(&self, value: &Value, schema: &Value, path: &mut String) -> Result<(), Violation> {
        let schema = self.resolve(schema);
        if schema == &Value::Bool(true) {
            return Ok(());
        }
        if let Some(branches) = schema.get("anyOf").or_else(|| schema.get("oneOf")) {
            return self.check_branches(value, branches, path);
        }
        if let Some(expected) = schema.get("const")
            && expected != value
        {
            return Err(violation(path, format!("expected {expected}")));
        }
        if let Some(Value::Array(allowed)) = schema.get("enum")
            && !allowed.contains(value)
        {
            let names: Vec<String> = allowed.iter().map(Value::to_string).collect();
            return Err(violation(
                path,
                format!("expected one of {}", names.join(", ")),
            ));
        }
        if let Some(types) = schema.get("type")
            && !type_matches(value, types)
        {
            return Err(violation(path, format!("expected {}", type_name(types))));
        }
        match value {
            Value::Object(object) => self.check_object(object, schema, path),
            Value::Array(items) => match schema.get("items") {
                Some(item_schema) => items.iter().enumerate().try_for_each(|(i, item)| {
                    let len = path.len();
                    let _ = write!(path, "/{i}");
                    let checked = self.check(item, item_schema, path);
                    path.truncate(len);
                    checked
                }),
                None => Ok(()),
            },
            _ => Ok(()),
        }
    }

    /// Passes when any branch passes; otherwise reports the first branch
    /// that is not `null` (the interesting one for an `Option`).
    fn check_branches(
        &self,
        value: &Value,
        branches: &Value,
        path: &mut String,
    ) -> Result<(), Violation> {
        let branches = branches.as_array().map(Vec::as_slice).unwrap_or_default();
        let mut first = None;
        for branch in branches {
            match self.check(value, branch, path) {
                Ok(()) => return Ok(()),
                Err(err)
                    if first.is_none()
                        && self.resolve(branch).get("type") != Some(&Value::from("null")) =>
                {
                    first = Some(err);
                }
                Err(_) => {}
            }
        }
        Err(first.unwrap_or_else(|| violation(path, "matches none of the allowed shapes")))
    }

    fn check_object(
        &self,
        object: &Map<String, Value>,
        schema: &Value,
        path: &mut String,
    ) -> Result<(), Violation> {
        let properties = schema.get("properties").and_then(Value::as_object);
        let closed = schema.get("additionalProperties") == Some(&Value::Bool(false));
        for (key, field) in object {
            let len = path.len();
            path.push('/');
            path.push_str(&pointer_segment(key));
            let checked = match properties.and_then(|p| p.get(key)) {
                Some(field_schema) => self.check(field, field_schema, path),
                None if closed => Err(Violation {
                    path: path.clone(),
                    unknown_field: true,
                    message: format!("unknown field `{key}`"),
                }),
                None => match schema.get("additionalProperties") {
                    Some(extra @ Value::Object(_)) => self.check(field, extra, path),
                    _ => Ok(()),
                },
            };
            path.truncate(len);
            checked?;
        }
        // Unknown fields first: a misspelled key is reported as itself,
        // not as the required field it was meant to be.
        if let Some(Value::Array(required)) = schema.get("required") {
            for name in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(name) {
                    let at = format!("{path}/{}", pointer_segment(name));
                    return Err(violation(&at, format!("missing required field `{name}`")));
                }
            }
        }
        Ok(())
    }

    fn strip(&self, value: &mut Value, schema: &Value, minor: u16) {
        let schema = self.resolve(schema);
        if let Some(branches) = schema
            .get("anyOf")
            .or_else(|| schema.get("oneOf"))
            .and_then(Value::as_array)
        {
            // The branch the value matches says which fields it has.
            let matching = branches
                .iter()
                .find(|branch| self.check(value, branch, &mut String::new()).is_ok());
            if let Some(branch) = matching {
                self.strip(value, branch, minor);
            }
            return;
        }
        match value {
            Value::Object(object) => self.strip_object(object, schema, minor),
            Value::Array(items) => {
                if let Some(item_schema) = schema.get("items") {
                    for item in items {
                        self.strip(item, item_schema, minor);
                    }
                }
            }
            _ => {}
        }
    }

    fn strip_object(&self, object: &mut Map<String, Value>, schema: &Value, minor: u16) {
        let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
            return;
        };
        let since = |key: &str| {
            properties
                .get(key)
                .map(|p| self.resolve(p))
                .and_then(|p| p.get("x-since"))
                .and_then(Value::as_u64)
        };
        object.retain(|key, _| since(key).is_none_or(|since| since <= u64::from(minor)));
        for (key, field) in object.iter_mut() {
            if let Some(field_schema) = properties.get(key) {
                self.strip(field, field_schema, minor);
            }
        }
    }
}

fn type_matches(value: &Value, types: &Value) -> bool {
    match types {
        Value::String(name) => is_type(value, name),
        Value::Array(names) => names
            .iter()
            .filter_map(Value::as_str)
            .any(|name| is_type(value, name)),
        _ => true,
    }
}

fn is_type(value: &Value, name: &str) -> bool {
    match name {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64(),
        _ => true,
    }
}

fn type_name(types: &Value) -> String {
    match types {
        Value::String(name) => name.clone(),
        Value::Array(names) => names
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" or "),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, Value, json};

    use super::{strip_newer, validate};

    fn defs() -> Map<String, Value> {
        let defs = json!({
            "Inner": {
                "type": "object",
                "properties": {"n": {"type": "integer"}},
                "required": ["n"],
                "additionalProperties": false
            }
        });
        defs.as_object().unwrap().clone()
    }

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {"type": "string"},
                "inner": {"anyOf": [{"$ref": "#/$defs/Inner"}, {"type": "null"}]},
                "mode": {"type": "string", "enum": ["a", "b"]},
                "list": {"type": "array", "items": {"$ref": "#/$defs/Inner"}},
                "late": {"type": "string", "x-since": 4}
            },
            "required": ["session_id"],
            "additionalProperties": false
        })
    }

    fn err(value: &Value) -> (String, bool) {
        let v = validate(value, &schema(), &defs()).unwrap_err();
        (v.path, v.unknown_field)
    }

    #[test]
    fn violations_name_the_offending_path() {
        assert!(
            validate(
                &json!({"session_id": "s", "inner": null}),
                &schema(),
                &defs()
            )
            .is_ok()
        );
        assert_eq!(err(&json!({})), ("/session_id".into(), false));
        assert_eq!(err(&json!({"sesion_id": "s"})), ("/sesion_id".into(), true));
        assert_eq!(
            err(&json!({"session_id": "s", "sesion_id": "s"})),
            ("/sesion_id".into(), true)
        );
        assert_eq!(
            err(&json!({"session_id": 4})),
            ("/session_id".into(), false)
        );
        assert_eq!(
            err(&json!({"session_id": "s", "inner": {"n": "x"}})),
            ("/inner/n".into(), false)
        );
        assert_eq!(
            err(&json!({"session_id": "s", "list": [{"n": 1}, {"m": 1}]})),
            ("/list/1/m".into(), true)
        );
        assert_eq!(
            err(&json!({"session_id": "s", "mode": "c"})),
            ("/mode".into(), false)
        );
    }

    #[test]
    fn newer_fields_are_stripped_for_older_minors() {
        let mut value = json!({"session_id": "s", "late": "x"});
        strip_newer(&mut value, &schema(), &defs(), 2);
        assert_eq!(value, json!({"session_id": "s"}));
        let mut value = json!({"session_id": "s", "late": "x"});
        strip_newer(&mut value, &schema(), &defs(), 4);
        assert_eq!(value["late"], "x");
    }
}
