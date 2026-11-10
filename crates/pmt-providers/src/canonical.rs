//! The canonical request and its hash (spec: replay-and-evaluation,
//! Canonical request hash; design D10).
//!
//! Every model call is content-addressed by `sha256:<hex>` over the
//! canonical JSON (keys sorted, no insignificant whitespace, numbers in
//! shortest round-trip form) of exactly what decides the answer: provider,
//! wire API, model, system prompt, messages, tool definitions, tool choice,
//! sampling parameters and response format. Transport fields (request ids,
//! auth headers, the streaming flag) never reach [`Request`], and prompt
//! cache markers are `#[serde(skip)]` on it, so a streamed and a
//! non-streamed form, or a request with and without cache breakpoints,
//! hash the same. The same canonical value is what the model-call cache
//! and session recordings store, and what replay divergence diffs show.

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use crate::request::{EXTRA_BODY, Request};
use crate::resolve::Route;

/// Sampling keys taken from the request's (or the route's) body
/// parameters; the output cap and reasoning effort come from the request.
const SAMPLING: [&str; 4] = ["temperature", "top_p", "stop", "seed"];

/// Where a call goes: the part of a route that enters the hash.
#[derive(Debug, Clone, Copy)]
pub struct CallTarget<'a> {
    /// Provider id.
    pub provider: &'a str,
    /// Wire API (`chat_completions`, `responses`, `anthropic_messages`,
    /// `gemini_native`...).
    pub wire: &'a str,
    /// Model id at the provider.
    pub model: &'a str,
    /// Body parameters the route adds to every request.
    pub extra: Option<&'a Map<String, Value>>,
}

impl<'a> CallTarget<'a> {
    /// The target of `route`.
    #[must_use]
    pub fn of(route: &'a Route) -> Self {
        Self {
            provider: &route.provider,
            wire: wire_api(route),
            model: &route.model,
            extra: route.body_extra.as_deref(),
        }
    }
}

/// The wire API `route` speaks.
#[must_use]
pub fn wire_api(route: &Route) -> &'static str {
    match route.bundled {
        Some(bundled) => bundled.as_str(),
        None => route.wire_format.as_str(),
    }
}

/// One body parameter: the request's override, else the route's, also
/// looked up one level down in `extra_body`.
fn parameter(request: &Request, target: &CallTarget<'_>, key: &str) -> Option<Value> {
    let lookup = |map: &Map<String, Value>| {
        map.get(key)
            .or_else(|| map.get(EXTRA_BODY).and_then(|extra| extra.get(key)))
            .cloned()
    };
    request
        .body_overrides
        .as_deref()
        .and_then(lookup)
        .or_else(|| target.extra.and_then(lookup))
}

/// The canonical form of `request` sent to `target`.
///
/// # Panics
///
/// Never: the request holds only plain data.
#[must_use]
pub fn canonical(request: &Request, target: &CallTarget<'_>) -> Value {
    let mut sampling = Map::new();
    for key in SAMPLING {
        if let Some(value) = parameter(request, target, key) {
            sampling.insert(key.to_owned(), value);
        }
    }
    if let Some(cap) = request.max_output_tokens {
        sampling.insert("max_output_tokens".to_owned(), cap.into());
    }
    if let Some(level) = request.reasoning {
        sampling.insert("reasoning_effort".to_owned(), level.as_str().into());
    }
    json!({
        "provider": target.provider,
        "wire_api": target.wire,
        "model": target.model,
        "system": plain(&request.system),
        "messages": plain(&request.messages),
        "tools": plain(&request.tools),
        "tool_choice": parameter(request, target, "tool_choice"),
        "sampling": sampling,
        "response_format": parameter(request, target, "response_format"),
    })
}

/// `value` as canonical JSON text: object keys sorted, no whitespace,
/// numbers as `serde_json` prints them (shortest round-trip form).
#[must_use]
pub fn to_text(value: &Value) -> String {
    let mut out = String::new();
    write(value, &mut out);
    out
}

fn write(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// `sha256:<hex>` of a canonical value's text.
#[must_use]
pub fn hash_value(value: &Value) -> String {
    format!(
        "sha256:{}",
        crate::request::hex(&Sha256::digest(to_text(value)))
    )
}

/// The `request_hash` of `request` sent to `target`.
#[must_use]
pub fn hash(request: &Request, target: &CallTarget<'_>) -> String {
    hash_value(&canonical(request, target))
}

fn plain<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).expect("plain data serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::{Message, Role, SystemBlock};

    fn target<'a>() -> CallTarget<'a> {
        CallTarget {
            provider: "p",
            wire: "responses",
            model: "m",
            extra: None,
        }
    }

    #[test]
    fn keys_are_sorted_and_whitespace_free() {
        let value = json!({"b": [1, {"z": 0.1, "a": "x y"}], "a": null});
        assert_eq!(to_text(&value), r#"{"a":null,"b":[1,{"a":"x y","z":0.1}]}"#);
    }

    #[test]
    fn sampling_parameters_and_tool_choice_enter_the_hash() {
        let base = Request {
            model: "m".into(),
            system: vec![SystemBlock {
                text: "s".into(),
                cache_breakpoint: false,
            }],
            messages: vec![Message::text(Role::User, "hi")],
            ..Request::default()
        };
        let plain = hash(&base, &target());
        let mut hot = base.clone();
        let overrides = json!({"temperature": 0.9, "tool_choice": "required"});
        hot.body_overrides = Some(std::sync::Arc::new(overrides.as_object().unwrap().clone()));
        let hashed = hash(&hot, &target());
        assert_ne!(hashed, plain);
        let canonical = canonical(&hot, &target());
        assert_eq!(canonical["sampling"]["temperature"], 0.9);
        assert_eq!(canonical["tool_choice"], "required");
        // A route parameter counts the same as a request one.
        let route_extra = overrides.as_object().unwrap().clone();
        let routed = CallTarget {
            extra: Some(&route_extra),
            ..target()
        };
        assert_eq!(hash(&base, &routed), hashed);
        let other_wire = CallTarget {
            wire: "chat_completions",
            ..target()
        };
        assert_ne!(hash(&base, &other_wire), plain);
    }
}
