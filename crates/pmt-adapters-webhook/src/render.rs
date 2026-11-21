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

/// Characters of an MCP event's `data` a turn input carries.
pub(crate) const DATA_CHARS: usize = 32_000;

/// The input of an MCP event occurrence whose template has `{data}`: the
/// rest of the template renders first and the event's `data` (pretty
/// JSON, cut at [`DATA_CHARS`] with a note) goes in afterwards, so text
/// inside it is never read as a placeholder. `None` without `{data}`.
pub(crate) fn render_event(template: Option<&str>, payload: &Value) -> Option<String> {
    let template = template.filter(|t| t.contains("{data}"))?;
    let data = serde_json::to_string_pretty(&payload["data"]).unwrap_or_default();
    let total = data.chars().count();
    let data = if total > DATA_CHARS {
        format!(
            "{}\n[truncated: the first {DATA_CHARS} of {total} characters]",
            cut(&data, DATA_CHARS)
        )
    } else {
        data
    };
    let parts: Vec<String> = template
        .split("{data}")
        .map(|part| {
            if part.is_empty() {
                String::new()
            } else {
                render(Some(part), payload)
            }
        })
        .collect();
    Some(parts.join(&data))
}

/// A coalescing key: a dot path (`pull_request.number`, optionally under
/// `payload.`) or a template of them; `None` when any path resolves to
/// nothing, so unrelated events never share a group.
pub(crate) fn key(spec: &str, payload: &Value) -> Option<String> {
    let spec = spec.trim();
    let template = if spec.contains('{') {
        spec.to_owned()
    } else {
        format!("{{{spec}}}")
    };
    let mut out = String::new();
    let mut rest = template.as_str();
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let close = after.find('}')?;
        let path = &after[..close];
        let value = resolve(payload, path)
            .or_else(|| {
                path.strip_prefix("payload.")
                    .and_then(|p| resolve(payload, p))
            })
            .filter(|v| !v.is_null())?;
        out.push_str(&show(value));
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    Some(out)
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
    fn coalescing_keys_resolve_every_path_or_nothing() {
        let payload = json!({"ref": "main", "repository": {"full_name": "o/r"}, "number": 7});
        assert_eq!(key("payload.ref", &payload).as_deref(), Some("main"));
        assert_eq!(key("ref", &payload).as_deref(), Some("main"));
        assert_eq!(
            key("{repository.full_name}#{number}", &payload).as_deref(),
            Some("o/r#7")
        );
        assert_eq!(key("pull_request.number", &payload), None);
    }

    #[test]
    fn event_data_goes_in_after_rendering_and_is_cut_with_a_note() {
        let payload = json!({"eventId": "e1", "data": {"note": "{__raw__} {eventId}"}});
        let text = render_event(Some("Event {eventId}:\n{data}\nEnd"), &payload).unwrap();
        assert!(text.starts_with("Event e1:\n"), "{text}");
        assert!(text.contains("{__raw__} {eventId}"), "{text}");
        assert!(text.ends_with("\nEnd"));
        let big = json!({"data": "x".repeat(40_000)});
        let text = render_event(Some("{data}"), &big).unwrap();
        assert!(text.contains("[truncated: the first 32000 of 40002 characters]"));
        assert_eq!(render_event(Some("no data"), &payload), None);
    }

    #[test]
    fn raw_and_default_inputs_are_the_payload_cut_at_4000() {
        let payload = json!({"blob": "z".repeat(5000)});
        assert_eq!(render(None, &payload).chars().count(), 4000);
        assert_eq!(render(Some("{__raw__}"), &payload).chars().count(), 4000);
    }
}
