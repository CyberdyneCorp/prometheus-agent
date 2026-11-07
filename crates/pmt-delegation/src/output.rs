//! The child's structured answer (spec: subagent-delegation,
//! `output_schema` validation never discards results): the schema goes
//! into the child's prompt, the child ends with a JSON answer, which is
//! validated; one correction attempt is allowed, and a second failure
//! keeps the raw answer as the summary with the schema errors.

use serde_json::Value;

use crate::schema_check;

/// The prompt lines that ask for a schema-shaped answer.
#[must_use]
pub fn instructions(schema: &Value) -> String {
    format!(
        "End your work with a final answer that is a single JSON value matching this JSON \
         Schema (you may wrap it in a ```json block):\n{}",
        serde_json::to_string_pretty(schema).unwrap_or_default()
    )
}

/// The message of the one correction attempt.
#[must_use]
pub fn correction(errors: &[String]) -> String {
    format!(
        "Your final answer does not match the required JSON Schema: {}. Reply with only the \
         corrected JSON value.",
        errors.join("; ")
    )
}

/// The JSON value an answer carries: the last fenced `json` block, else
/// the whole text, else the last `{...}` or `[...]` span.
#[must_use]
pub fn extract(answer: &str) -> Option<Value> {
    if let Some(block) = last_fenced(answer)
        && let Ok(value) = serde_json::from_str(block.trim())
    {
        return Some(value);
    }
    if let Ok(value) = serde_json::from_str(answer.trim()) {
        return Some(value);
    }
    [('{', '}'), ('[', ']')]
        .into_iter()
        .find_map(|(open, close)| {
            let start = answer.find(open)?;
            let end = answer.rfind(close)?;
            (end > start)
                .then(|| serde_json::from_str(&answer[start..=end]).ok())
                .flatten()
        })
}

fn last_fenced(answer: &str) -> Option<&str> {
    let start = answer.rfind("```json")? + "```json".len();
    let rest = &answer[start..];
    rest.find("```").map(|end| &rest[..end])
}

/// The answer checked against `schema`: the parsed value, or the errors.
///
/// # Errors
///
/// Why the answer does not match (no JSON at all, or the violations).
pub fn check(schema: &Value, answer: &str) -> Result<Value, Vec<String>> {
    let Some(value) = extract(answer) else {
        return Err(vec!["the answer contains no JSON value".to_owned()]);
    };
    let errors = schema_check::validate(schema, &value);
    if errors.is_empty() {
        Ok(value)
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn answers_are_read_from_fences_whole_text_or_the_last_object() {
        assert_eq!(
            extract("done\n```json\n{\"a\": 1}\n```"),
            Some(json!({"a": 1}))
        );
        assert_eq!(extract("[1, 2]"), Some(json!([1, 2])));
        assert_eq!(
            extract("Result: {\"ok\": true} thanks"),
            Some(json!({"ok": true}))
        );
        assert_eq!(extract("no json here"), None);
    }

    #[test]
    fn checks_report_schema_errors() {
        let schema = json!({"type": "object", "required": ["n"]});
        assert_eq!(check(&schema, "{\"n\": 1}").unwrap(), json!({"n": 1}));
        assert!(check(&schema, "{}").unwrap_err()[0].contains("'n'"));
        assert!(check(&schema, "prose").is_err());
    }
}
