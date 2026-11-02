//! Declarative payload filters (spec: platform-adapters, Webhook payload
//! filters and transform scripts): conditions over `payload.<path>`,
//! `event` and `headers.<Name>`, evaluated after the signature and event
//! checks. Validation already refused bad paths and patterns; a pattern
//! that still fails to compile matches nothing.

use std::path::Path;

use pmt_config::schema::WebhookFilter;
use pmt_gateway::messaging::HttpRequest;
use serde_json::Value;

/// What the conditions look at.
pub(crate) struct Subject<'a> {
    pub(crate) payload: &'a Value,
    pub(crate) event: Option<&'a str>,
    pub(crate) request: &'a HttpRequest,
    /// The profile directory (`in_file` paths are relative to it).
    pub(crate) profile_dir: &'a Path,
}

impl Subject<'_> {
    /// The value at `path`, when it resolves.
    fn get(&self, path: &str) -> Option<Value> {
        if path == "event" {
            return self.event.map(|e| Value::String(e.to_owned()));
        }
        if let Some(name) = path.strip_prefix("headers.") {
            let name = name.to_ascii_lowercase();
            return self
                .request
                .header(&name)
                .map(|v| Value::String(v.to_owned()));
        }
        let rest = path.strip_prefix("payload.")?;
        rest.split('.')
            .try_fold(self.payload, |value, key| match value {
                Value::Array(items) => key.parse::<usize>().ok().and_then(|i| items.get(i)),
                _ => value.get(key),
            })
            .filter(|v| !v.is_null())
            .cloned()
    }
}

/// The text a value compares as (strings unquoted).
fn text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Whether two values are equal, a string and a scalar comparing by text
/// (`"42"` equals `42`: headers are always strings).
fn same(a: &Value, b: &Value) -> bool {
    a == b || (!a.is_object() && !a.is_array() && text(a) == text(b))
}

/// Whether every condition holds.
pub(crate) fn matches(conditions: &[WebhookFilter], subject: &Subject<'_>) -> bool {
    conditions.iter().all(|c| holds(c, subject))
}

fn holds(condition: &WebhookFilter, subject: &Subject<'_>) -> bool {
    match condition {
        WebhookFilter::All(list) => list.iter().all(|c| holds(c, subject)),
        WebhookFilter::Any(list) => list.iter().any(|c| holds(c, subject)),
        WebhookFilter::Not(inner) => !holds(inner, subject),
        WebhookFilter::Exists(p) => subject.get(&p.path).is_some(),
        WebhookFilter::Missing(p) => subject.get(&p.path).is_none(),
        WebhookFilter::Equals(v) => subject.get(&v.path).is_some_and(|x| same(&x, &v.value)),
        WebhookFilter::NotEquals(v) => !subject.get(&v.path).is_some_and(|x| same(&x, &v.value)),
        WebhookFilter::Contains(v) => subject.get(&v.path).is_some_and(|x| match &x {
            Value::Array(items) => items.iter().any(|i| same(i, &v.value)),
            other => text(other).contains(&text(&v.value)),
        }),
        WebhookFilter::In(v) => subject
            .get(&v.path)
            .is_some_and(|x| v.values.iter().any(|candidate| same(&x, candidate))),
        WebhookFilter::InFile(f) => subject.get(&f.path).is_some_and(|x| {
            let wanted = text(&x);
            std::fs::read_to_string(subject.profile_dir.join(&f.file))
                .is_ok_and(|lines| lines.lines().any(|l| l.trim() == wanted))
        }),
        WebhookFilter::Regex(p) => subject
            .get(&p.path)
            .is_some_and(|x| regex::Regex::new(&p.pattern).is_ok_and(|re| re.is_match(&text(&x)))),
    }
}

#[cfg(test)]
mod tests {
    use pmt_config::schema::WebhookFilters;
    use serde_json::json;

    use super::*;

    fn request() -> HttpRequest {
        HttpRequest {
            method: "POST".into(),
            path: "/ci".into(),
            query: None,
            headers: vec![("x-trace".into(), "abc".into())],
            body: Vec::new(),
        }
    }

    fn check(filters: &str, payload: &Value) -> bool {
        let table: toml::Table = toml::from_str(&format!("filters = {filters}")).unwrap();
        let filters: WebhookFilters = table["filters"].clone().try_into().unwrap();
        let request = request();
        let dir = std::env::temp_dir();
        let subject = Subject {
            payload,
            event: Some("workflow_run"),
            request: &request,
            profile_dir: &dir,
        };
        matches(filters.conditions(), &subject)
    }

    #[test]
    fn only_failing_ci_runs_pass_the_equals_filter() {
        let filter =
            r#"{ equals = { path = "payload.workflow_run.conclusion", value = "failure" } }"#;
        assert!(check(
            filter,
            &json!({"workflow_run": {"conclusion": "failure"}})
        ));
        assert!(!check(
            filter,
            &json!({"workflow_run": {"conclusion": "success"}})
        ));
        assert!(!check(filter, &json!({})));
    }

    #[test]
    fn groups_combine_conditions_over_payload_event_and_headers() {
        let payload = json!({"labels": ["bug", "p1"], "ref": "refs/heads/main", "n": 42});
        assert!(check(
            r#"[{ contains = { path = "payload.labels", value = "bug" } }, { exists = { path = "headers.X-Trace" } }]"#,
            &payload
        ));
        assert!(check(
            r#"{ any = [{ missing = { path = "payload.ref" } }, { regex = { path = "payload.ref", pattern = "^refs/heads/(main|release)" } }] }"#,
            &payload
        ));
        assert!(check(
            r#"{ not = { in = { path = "event", values = ["push", "issues"] } } }"#,
            &payload
        ));
        assert!(check(
            r#"{ equals = { path = "payload.n", value = "42" } }"#,
            &payload
        ));
        assert!(check(
            r#"{ not_equals = { path = "payload.missing", value = 1 } }"#,
            &payload
        ));
    }
}
