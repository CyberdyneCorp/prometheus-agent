//! The turn input of a webhook event (spec: platform-adapters, Webhook
//! routes): a route's `prompt` template with `{a.b.c}` paths into the
//! payload (strings as they are, other scalars as JSON, nested values as
//! JSON cut at 2,000 characters, a path that resolves to nothing kept
//! literally) and `{__raw__}` for the whole payload cut at 4,000; without
//! a template, the payload's JSON cut at 4,000.

use serde_json::Value;

const NESTED_CHARS: usize = 2000;
const RAW_CHARS: usize = 4000;

fn cut(text: &str, chars: usize) -> String {
    text.chars().take(chars).collect()
}

fn resolve<'a>(payload: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(payload, |value, key| match value {
        Value::Object(map) => map.get(key),
        Value::Array(items) => items.get(key.parse::<usize>().ok()?),
        _ => None,
    })
}

fn show(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Object(_) | Value::Array(_) => cut(&value.to_string(), NESTED_CHARS),
        other => other.to_string(),
    }
}

/// The input of `payload` under `template`.
#[must_use]
pub(crate) fn render(template: Option<&str>, payload: &Value) -> String {
    let Some(template) = template else {
        return cut(&payload.to_string(), RAW_CHARS);
    };
    let mut out = String::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            rest = &rest[open..];
            break;
        };
        let path = &after[..close];
        let valid = !path.is_empty()
            && path
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | '-'));
        match (valid, path) {
            (true, "__raw__") => out.push_str(&cut(&payload.to_string(), RAW_CHARS)),
            (true, _) => match resolve(payload, path) {
                Some(value) => out.push_str(&show(value)),
                None => out.push_str(&rest[open..=open + 1 + close]),
            },
            (false, _) => out.push_str(&rest[open..=open + 1 + close]),
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn prompt_rendering_reads_dot_paths() {
        let payload = json!({"pull_request": {"number": 42, "title": "Fix x"}});
        assert_eq!(
            render(
                Some("Review PR #{pull_request.number}: {pull_request.title}"),
                &payload
            ),
            "Review PR #42: Fix x"
        );
    }

    #[test]
    fn unresolved_paths_stay_and_nested_values_are_json() {
        let payload = json!({"a": {"b": [1, 2]}, "list": ["x", "y"]});
        assert_eq!(
            render(Some("{missing.key} {a.b} {list.1}"), &payload),
            "{missing.key} [1,2] y"
        );
        assert_eq!(
            render(Some("{} {not a path} {"), &payload),
            "{} {not a path} {"
        );
    }

    #[test]
    fn raw_and_default_inputs_are_the_payload_cut_at_4000() {
        let payload = json!({"blob": "z".repeat(5000)});
        assert_eq!(render(None, &payload).chars().count(), 4000);
        assert_eq!(render(Some("{__raw__}"), &payload).chars().count(), 4000);
    }
}
