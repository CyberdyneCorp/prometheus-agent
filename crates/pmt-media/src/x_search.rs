//! `x_search` (spec: web-and-media-tools, `x_search` tool): read-only
//! discovery on X through the xAI Responses built-in X search.
//!
//! Arguments are validated before any billable call (handle lists, the
//! date window); the xAI API key is preferred over the `xai-oauth`
//! subscription token at call time; transient failures are retried with a
//! bounded backoff; an answer that came back without citations while a
//! narrowing filter was active is marked degraded.

use std::sync::Arc;
use std::time::Duration;

use jiff::civil::Date;
use pmt_core::capability::Capability as Cap;
use pmt_tools::descriptor::{
    CallContext, Effect, HandlerFuture, Owner, RegisteredTool, ResourceClaims, ToolDescriptor,
    ToolHandler,
};
use pmt_tools::probe::{FnProbe, signin};
use pmt_tools::result::{Status, ToolResult};
use reqwest::header::CONTENT_TYPE;
use serde_json::{Value, json};

use crate::http::{status_error, transport};
use crate::{MediaSession, MediaSessions};

/// The tool's toolset (off by default).
pub const TOOLSET: &str = "x_search";
/// Most handles per list.
pub const MAX_HANDLES: usize = 10;
/// The xAI API.
pub const XAI_URL: &str = "https://api.x.ai/v1";

struct Handler(Arc<MediaSessions>);

/// `x_search`, available when an xAI key or `xai-oauth` sign-in exists.
#[must_use]
pub fn registered(sessions: &Arc<MediaSessions>) -> RegisteredTool {
    let probe = FnProbe::new(
        "x_search.credential",
        |context: pmt_tools::probe::ProbeContext<'_>| {
            if (context.has_secret)("XAI_API_KEY") || (context.has_secret)(&signin("xai-oauth")) {
                Ok(())
            } else {
                Err("needs XAI_API_KEY or an xai-oauth sign-in".to_owned())
            }
        },
    );
    RegisteredTool::new(
        descriptor(),
        Owner::Builtin,
        Arc::new(Handler(Arc::clone(sessions))),
    )
    .with_probe(Arc::new(probe))
}

fn descriptor() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        "x_search",
        TOOLSET,
        "Search posts on X (read-only) and get a cited answer. Narrow by handles \
         (allowed_x_handles or excluded_x_handles, at most 10, not both) and by a \
         YYYY-MM-DD date window.",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "What to look for."},
                "allowed_x_handles": {"type": "array", "items": {"type": "string"}, "maxItems": MAX_HANDLES},
                "excluded_x_handles": {"type": "array", "items": {"type": "string"}, "maxItems": MAX_HANDLES},
                "from_date": {"type": "string", "description": "YYYY-MM-DD"},
                "to_date": {"type": "string", "description": "YYYY-MM-DD"},
                "enable_image_understanding": {"type": "boolean"},
                "enable_video_understanding": {"type": "boolean"}
            },
            "required": ["query"],
            "additionalProperties": false
        }),
    );
    d.effect = Effect::None;
    d.idempotent = true;
    d.claims = Some(ResourceClaims::none());
    d.capabilities = vec![Cap::parse("net.connect:*").expect("valid capability")];
    d
}

impl ToolHandler for Handler {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let session = self.0.of(context)?;
            let result = session.x_search(&arguments).await;
            pmt_tools::usage::record(session.setup().usage.as_ref(), context, "x_search", &result);
            Ok(result)
        })
    }
}

/// The xAI `x_search` tool of validated arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Search {
    /// The query.
    pub query: String,
    /// The tool definition sent to xAI.
    pub tool: Value,
    /// A handle or date filter narrows the search.
    pub narrowed: bool,
}

fn handles(arguments: &Value, key: &str) -> Result<Vec<String>, String> {
    let list: Vec<String> = arguments
        .get(key)
        .and_then(Value::as_array)
        .map(|l| {
            l.iter()
                .filter_map(Value::as_str)
                .map(|h| h.trim().trim_start_matches('@').to_owned())
                .filter(|h| !h.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if list.len() > MAX_HANDLES {
        return Err(format!("{key} takes at most {MAX_HANDLES} handles"));
    }
    Ok(list)
}

fn date(arguments: &Value, key: &str) -> Result<Option<Date>, String> {
    match arguments.get(key).and_then(Value::as_str) {
        None => Ok(None),
        Some(text) => text
            .parse::<Date>()
            .map(Some)
            .map_err(|_| format!("{key} ({text}) is not a YYYY-MM-DD date")),
    }
}

/// Validates `arguments` against `today` without contacting xAI.
///
/// # Errors
///
/// The first invalid argument.
pub fn validate(arguments: &Value, today: Date) -> Result<Search, String> {
    let query = arguments
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if query.trim().is_empty() {
        return Err("query is empty".to_owned());
    }
    let allowed = handles(arguments, "allowed_x_handles")?;
    let excluded = handles(arguments, "excluded_x_handles")?;
    if !allowed.is_empty() && !excluded.is_empty() {
        return Err("allowed_x_handles and excluded_x_handles cannot be combined".to_owned());
    }
    let (from, to) = (date(arguments, "from_date")?, date(arguments, "to_date")?);
    if let Some(from) = from
        && from > today
    {
        return Err(format!("from_date ({from}) is in the future"));
    }
    if let (Some(from), Some(to)) = (from, to)
        && from > to
    {
        return Err(format!("from_date ({from}) is after to_date ({to})"));
    }
    let flag = |key: &str| arguments.get(key).and_then(Value::as_bool).unwrap_or(false);
    let mut tool = json!({"type": "x_search",
        "enable_image_understanding": flag("enable_image_understanding"),
        "enable_video_understanding": flag("enable_video_understanding")});
    if !allowed.is_empty() {
        tool["allowed_x_handles"] = json!(allowed);
    }
    if !excluded.is_empty() {
        tool["excluded_x_handles"] = json!(excluded);
    }
    if let Some(from) = from {
        tool["from_date"] = json!(from.to_string());
    }
    if let Some(to) = to {
        tool["to_date"] = json!(to.to_string());
    }
    Ok(Search {
        query: query.to_owned(),
        tool,
        narrowed: !allowed.is_empty() || !excluded.is_empty() || from.is_some() || to.is_some(),
    })
}

/// The backoff before retry `attempt` (1-based): 1.5 s per attempt,
/// capped at 5 s.
#[must_use]
pub fn backoff(attempt: u32) -> Duration {
    Duration::from_millis((1_500 * u64::from(attempt)).min(5_000))
}

/// The answer text of a Responses reply.
fn answer_text(reply: &Value) -> String {
    let outputs = reply.get("output").and_then(Value::as_array);
    let mut text = String::new();
    for content in outputs
        .into_iter()
        .flatten()
        .filter_map(|o| o.get("content"))
    {
        for part in content.as_array().into_iter().flatten() {
            if let Some(t) = part.get("text").and_then(Value::as_str) {
                text.push_str(t);
            }
        }
    }
    text
}

impl MediaSession {
    /// Runs one `x_search` call.
    pub async fn x_search(&self, arguments: &Value) -> ToolResult {
        let today = jiff::Zoned::now().date();
        let search = match validate(arguments, today) {
            Ok(search) => search,
            Err(why) => return ToolResult::error(format!("x_search: {why}")),
        };
        let setup = self.setup();
        let access = match setup.credentials.xai_key_first() {
            Ok(access) => access,
            Err(why) => return ToolResult::error(format!("x_search unavailable: {why}")),
        };
        let config = &setup.config().x_search;
        let mut body = json!({"model": config.model,
            "input": [{"role": "user", "content": search.query}], "tools": [search.tool]});
        if let Some(effort) = &config.reasoning_effort {
            body["reasoning"] = json!({"effort": effort});
        }
        let url = format!("{}/responses", setup.endpoint("xai", XAI_URL));
        let reply = match self.x_post(&url, &access, &body).await {
            Ok(reply) => reply,
            Err(why) => return ToolResult::error(format!("x_search failed (xai): {why}")),
        };
        let citations: Vec<String> = pmt_tools::web::vendors::citations(&reply)
            .into_iter()
            .map(|hit| hit.url)
            .collect();
        let degraded = search.narrowed && citations.is_empty();
        let mut out = json!({"answer": answer_text(&reply), "citations": citations});
        if degraded {
            out["degraded"] = json!(true);
            out["note"] = json!(
                "a filter was active and no post was cited; the answer may not come from the filtered posts"
            );
        }
        let status = if degraded {
            Status::Degraded
        } else {
            Status::Ok
        };
        let mut result = ToolResult::text(status, out.to_string());
        if degraded {
            result.metadata.insert("degraded".into(), Value::Bool(true));
        }
        pmt_tools::usage::mark(&mut result, "xai", &config.model, &access.billing_source);
        result
    }

    /// Posts with retries on 5xx, timeouts and connection errors.
    async fn x_post(
        &self,
        url: &str,
        access: &crate::Access,
        body: &Value,
    ) -> Result<Value, String> {
        let config = &self.setup().config().x_search;
        let timeout = Duration::from_secs(config.timeout_secs.max(30));
        let mut attempt = 0;
        loop {
            let sent = self
                .setup()
                .client
                .post(url)
                .bearer_auth(access.bearer()?.expose())
                .header(CONTENT_TYPE, "application/json")
                .timeout(timeout)
                .body(body.to_string())
                .send()
                .await;
            let retryable = match sent {
                Ok(response) if response.status().is_server_error() => {
                    format!("HTTP {}", response.status().as_u16())
                }
                Ok(response) => {
                    let status = response.status().as_u16();
                    let bytes = response.bytes().await.map_err(|e| transport(&e))?;
                    if !(200..300).contains(&status) {
                        return Err(status_error(status, &bytes));
                    }
                    return serde_json::from_slice(&bytes)
                        .map_err(|e| format!("unreadable reply: {e}"));
                }
                Err(err) if err.is_timeout() || err.is_connect() || err.is_request() => {
                    transport(&err)
                }
                Err(err) => return Err(transport(&err)),
            };
            attempt += 1;
            if attempt > config.retries {
                return Err(retryable);
            }
            tokio::time::sleep(backoff(attempt)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn today() -> Date {
        "2026-11-09".parse().unwrap()
    }

    #[test]
    fn a_future_window_is_refused_locally() {
        let err = validate(&json!({"query": "x", "from_date": "2026-11-16"}), today()).unwrap_err();
        assert_eq!(err, "from_date (2026-11-16) is in the future");
        let err = validate(
            &json!({"query": "x", "from_date": "2026-11-05", "to_date": "2026-11-01"}),
            today(),
        )
        .unwrap_err();
        assert!(err.contains("is after to_date"));
        assert!(validate(&json!({"query": "x", "from_date": "nov 1"}), today()).is_err());
    }

    #[test]
    fn handles_are_stripped_capped_and_exclusive() {
        let search = validate(
            &json!({"query": "x", "allowed_x_handles": ["@someone", " other "]}),
            today(),
        )
        .unwrap();
        assert_eq!(
            search.tool["allowed_x_handles"],
            json!(["someone", "other"])
        );
        assert!(search.narrowed);
        let both = json!({"query": "x", "allowed_x_handles": ["a"], "excluded_x_handles": ["b"]});
        assert!(validate(&both, today()).is_err());
        let many: Vec<String> = (0..11).map(|i| format!("h{i}")).collect();
        assert!(validate(&json!({"query": "x", "excluded_x_handles": many}), today()).is_err());
        assert!(!validate(&json!({"query": "x"}), today()).unwrap().narrowed);
    }

    #[test]
    fn backoff_grows_by_one_and_a_half_seconds_up_to_five() {
        assert_eq!(backoff(1), Duration::from_millis(1_500));
        assert_eq!(backoff(3), Duration::from_millis(4_500));
        assert_eq!(backoff(4), Duration::from_secs(5));
    }
}
