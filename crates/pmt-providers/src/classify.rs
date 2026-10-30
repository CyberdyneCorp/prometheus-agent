//! Error classification and same-route retry policy (spec: model-providers,
//! Error classification and same-route retries).

use std::time::Duration;

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use reqwest::header::HeaderMap;
use serde::Serialize;

/// The class of a failed provider call; exactly one per failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// The credential was rejected.
    Auth,
    /// The account's plan is not entitled to the API.
    Entitlement,
    /// Payment or quota exhausted.
    Billing,
    /// Throttled.
    RateLimit,
    /// The provider is overloaded.
    Overloaded,
    /// The provider failed.
    ServerError,
    /// No answer in time (request or stream idle timeout).
    Timeout,
    /// Connection failure.
    Network,
    /// The prompt does not fit the model's window.
    ContextOverflow,
    /// The provider refused the content.
    ContentFilter,
    /// The request is malformed.
    InvalidRequest,
    /// Prometheus is misconfigured for this provider (wrong URL, unknown
    /// model).
    Config,
}

impl ErrorClass {
    /// Wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Entitlement => "entitlement",
            Self::Billing => "billing",
            Self::RateLimit => "rate_limit",
            Self::Overloaded => "overloaded",
            Self::ServerError => "server_error",
            Self::Timeout => "timeout",
            Self::Network => "network",
            Self::ContextOverflow => "context_overflow",
            Self::ContentFilter => "content_filter",
            Self::InvalidRequest => "invalid_request",
            Self::Config => "config",
        }
    }

    /// Retried on the same route.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::RateLimit | Self::Overloaded | Self::ServerError | Self::Timeout | Self::Network
        )
    }

    /// May activate the fallback chain once same-route retries are spent.
    #[must_use]
    pub const fn is_fallback_eligible(self) -> bool {
        self.is_retryable() || matches!(self, Self::Auth | Self::Entitlement | Self::Billing)
    }
}

/// A classified provider failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError {
    /// The class.
    pub class: ErrorClass,
    /// HTTP status, when there was a response.
    pub status: Option<u16>,
    /// What the provider said (or what failed).
    pub message: String,
    /// The provider's `Retry-After`.
    pub retry_after: Option<Duration>,
    /// When the provider says the limit resets.
    pub reset_at: Option<Timestamp>,
}

impl ProviderError {
    /// A failure without an HTTP response.
    #[must_use]
    pub fn new(class: ErrorClass, message: impl Into<String>) -> Self {
        Self {
            class,
            status: None,
            message: message.into(),
            retry_after: None,
            reset_at: None,
        }
    }

    /// The user-facing error for a call to `provider`.
    #[must_use]
    pub fn to_error(&self, provider: &str) -> Error {
        let status = self
            .status
            .map(|s| format!(" (HTTP {s})"))
            .unwrap_or_default();
        let message = format!(
            "{provider}: {} error{status}: {}",
            self.class.as_str(),
            self.message
        );
        match self.class {
            ErrorClass::Auth => Error::new(&codes::PROVIDER_AUTH_FAILED, message).with_hint(
                format!("check the {provider} credential: prometheus secrets list"),
            ),
            ErrorClass::Entitlement => Error::new(&codes::PROVIDER_AUTH_FAILED, message)
                .with_hint("the account's plan tier is not eligible for API access"),
            ErrorClass::InvalidRequest | ErrorClass::Config => {
                Error::new(&codes::PROVIDER_INVALID_REQUEST, message)
            }
            _ => Error::new(&codes::PROVIDER_CALL_FAILED, message),
        }
    }
}

const ENTITLEMENT: &[&str] = &[
    "plan",
    "tier",
    "not eligible",
    "does not have access",
    "subscription",
];
const BILLING: &[&str] = &[
    "insufficient_quota",
    "billing",
    "credit balance",
    "insufficient credits",
    "payment",
];
const CONTEXT: &[&str] = &[
    "context_length_exceeded",
    "maximum context length",
    "context window",
    "prompt is too long",
    "too many tokens",
    "reduce the length",
];
const FILTER: &[&str] = &[
    "content_filter",
    "content policy",
    "content_policy",
    "safety system",
];

fn mentions(body: &str, words: &[&str]) -> bool {
    words.iter().any(|w| body.contains(w))
}

/// Classifies an HTTP failure from its status and body.
#[must_use]
pub fn classify_status(status: u16, body: &str) -> ErrorClass {
    let lower = body.to_ascii_lowercase();
    match status {
        403 if mentions(&lower, ENTITLEMENT) => ErrorClass::Entitlement,
        401 | 403 => ErrorClass::Auth,
        402 => ErrorClass::Billing,
        404 => ErrorClass::Config,
        408 => ErrorClass::Timeout,
        413 => ErrorClass::ContextOverflow,
        429 if mentions(&lower, BILLING) => ErrorClass::Billing,
        429 => ErrorClass::RateLimit,
        503 | 529 => ErrorClass::Overloaded,
        500..=599 if lower.contains("overloaded") => ErrorClass::Overloaded,
        500..=599 => ErrorClass::ServerError,
        _ => classify_body(&lower),
    }
}

/// Classifies a 4xx request rejection (or an in-stream error) by its text.
fn classify_body(lower: &str) -> ErrorClass {
    if mentions(lower, CONTEXT) {
        ErrorClass::ContextOverflow
    } else if mentions(lower, FILTER) {
        ErrorClass::ContentFilter
    } else if mentions(lower, BILLING) {
        ErrorClass::Billing
    } else if lower.contains("overloaded") {
        ErrorClass::Overloaded
    } else if lower.contains("rate_limit") || lower.contains("rate limit") {
        ErrorClass::RateLimit
    } else if lower.contains("api_error") || lower.contains("server_error") {
        ErrorClass::ServerError
    } else {
        ErrorClass::InvalidRequest
    }
}

/// Classifies an error reported inside a stream (Anthropic `error` events,
/// Responses `response.failed`).
#[must_use]
pub fn classify_stream_error(kind_and_message: &str) -> ErrorClass {
    let lower = kind_and_message.to_ascii_lowercase();
    if lower.contains("authentication") {
        ErrorClass::Auth
    } else {
        classify_body(&lower)
    }
}

/// Builds the classified error of an HTTP failure response.
#[must_use]
pub fn from_response(
    status: u16,
    headers: &HeaderMap,
    body: &str,
    now: Timestamp,
) -> ProviderError {
    let retry_after = retry_after(headers, now);
    let reset_at = reset_at(headers, now).or_else(|| {
        retry_after.and_then(|d| {
            now.checked_add(jiff::SignedDuration::try_from(d).ok()?)
                .ok()
        })
    });
    ProviderError {
        class: classify_status(status, body),
        status: Some(status),
        message: body_message(body),
        retry_after,
        reset_at,
    }
}

/// The provider's error message (`error.message` / `message`), else the
/// start of the body.
#[must_use]
pub fn body_message(body: &str) -> String {
    let parsed: Option<serde_json::Value> = serde_json::from_str(body).ok();
    let message = parsed.as_ref().and_then(|v| {
        v.pointer("/error/message")
            .or_else(|| v.get("message"))
            .or_else(|| v.get("error").filter(|e| e.is_string()))
            .and_then(serde_json::Value::as_str)
    });
    let text = message.unwrap_or(body).trim();
    let mut short: String = text.chars().take(300).collect();
    if short.is_empty() {
        "no error message".clone_into(&mut short);
    }
    short
}

/// `Retry-After` as seconds or an HTTP date.
fn retry_after(headers: &HeaderMap, now: Timestamp) -> Option<Duration> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(secs) = value.parse::<f64>() {
        return (secs >= 0.0).then(|| Duration::from_secs_f64(secs));
    }
    let date = jiff::fmt::rfc2822::parse(value).ok()?.timestamp();
    let secs = date.duration_since(now).as_secs_f64();
    Some(Duration::from_secs_f64(secs.max(0.0)))
}

/// Anthropic's `anthropic-ratelimit-*-reset` (RFC 3339), the latest one.
fn reset_at(headers: &HeaderMap, now: Timestamp) -> Option<Timestamp> {
    headers
        .iter()
        .filter(|(name, _)| {
            let name = name.as_str();
            name.starts_with("anthropic-ratelimit-") && name.ends_with("-reset")
        })
        .filter_map(|(_, v)| v.to_str().ok()?.parse::<Timestamp>().ok())
        .filter(|t| *t > now)
        .max()
}

/// Same-route retry settings of one provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries after the first attempt (`providers.<id>.max_retries`).
    pub max_retries: u32,
    /// First backoff ceiling.
    pub base: Duration,
    /// Backoff ceiling cap.
    pub cap: Duration,
    /// Longest `Retry-After` honoured on the same route; a longer one ends
    /// same-route retries (the fallback chain takes over).
    pub retry_after_max: Duration,
}

impl RetryPolicy {
    /// The spec defaults with `max_retries`: backoff from 1 s, capped at
    /// 30 s, `Retry-After` up to 60 s.
    #[must_use]
    pub const fn new(max_retries: u32) -> Self {
        Self {
            max_retries,
            base: Duration::from_secs(1),
            cap: Duration::from_secs(30),
            retry_after_max: Duration::from_mins(1),
        }
    }

    /// The wait before retry number `retry` (0-based) after `error`, or
    /// `None` when the error must not be retried on this route.
    #[must_use]
    pub fn delay(&self, retry: u32, error: &ProviderError) -> Option<Duration> {
        if retry >= self.max_retries || !error.class.is_retryable() {
            return None;
        }
        if let Some(after) = error.retry_after {
            return (after <= self.retry_after_max).then_some(after);
        }
        let ceiling = self
            .base
            .saturating_mul(2u32.saturating_pow(retry))
            .min(self.cap);
        // Full jitter: uniform in [0, ceiling].
        let nanos = u64::try_from(ceiling.as_nanos()).unwrap_or(u64::MAX);
        Some(Duration::from_nanos(fastrand::u64(0..=nanos)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_map_to_one_class_each() {
        let cases = [
            (401, "", ErrorClass::Auth),
            (
                403,
                "{\"error\":{\"message\":\"Your plan tier lacks API access\"}}",
                ErrorClass::Entitlement,
            ),
            (403, "forbidden", ErrorClass::Auth),
            (402, "", ErrorClass::Billing),
            (
                429,
                "{\"error\":{\"code\":\"insufficient_quota\"}}",
                ErrorClass::Billing,
            ),
            (429, "slow down", ErrorClass::RateLimit),
            (529, "", ErrorClass::Overloaded),
            (
                500,
                "{\"type\":\"overloaded_error\"}",
                ErrorClass::Overloaded,
            ),
            (502, "", ErrorClass::ServerError),
            (408, "", ErrorClass::Timeout),
            (
                400,
                "maximum context length is 8192 tokens",
                ErrorClass::ContextOverflow,
            ),
            (400, "blocked by content_filter", ErrorClass::ContentFilter),
            (
                400,
                "tools.0.input_schema: invalid",
                ErrorClass::InvalidRequest,
            ),
            (404, "model not found", ErrorClass::Config),
        ];
        for (status, body, class) in cases {
            assert_eq!(classify_status(status, body), class, "{status} {body}");
        }
    }

    #[test]
    fn retryable_and_fallback_sets_follow_the_spec() {
        use ErrorClass::*;
        let all = [
            Auth,
            Entitlement,
            Billing,
            RateLimit,
            Overloaded,
            ServerError,
            Timeout,
            Network,
            ContextOverflow,
            ContentFilter,
            InvalidRequest,
            Config,
        ];
        let retry: Vec<_> = all.iter().filter(|c| c.is_retryable()).collect();
        assert_eq!(
            retry,
            [&RateLimit, &Overloaded, &ServerError, &Timeout, &Network]
        );
        let fallback: Vec<_> = all.iter().filter(|c| c.is_fallback_eligible()).collect();
        assert_eq!(
            fallback,
            [
                &Auth,
                &Entitlement,
                &Billing,
                &RateLimit,
                &Overloaded,
                &ServerError,
                &Timeout,
                &Network
            ]
        );
    }

    #[test]
    fn backoff_is_jittered_exponential_capped_and_honours_short_retry_after() {
        let policy = RetryPolicy::new(6);
        let error = ProviderError::new(ErrorClass::ServerError, "x");
        for retry in 0..6 {
            let ceiling = Duration::from_secs(1u64 << retry).min(Duration::from_secs(30));
            for _ in 0..50 {
                assert!(policy.delay(retry, &error).unwrap() <= ceiling);
            }
        }
        assert_eq!(policy.delay(6, &error), None);
        let mut throttled = ProviderError::new(ErrorClass::RateLimit, "x");
        throttled.retry_after = Some(Duration::from_secs(7));
        assert_eq!(policy.delay(0, &throttled), Some(Duration::from_secs(7)));
        throttled.retry_after = Some(Duration::from_secs(61));
        assert_eq!(policy.delay(0, &throttled), None);
        assert_eq!(
            policy.delay(0, &ProviderError::new(ErrorClass::Auth, "x")),
            None
        );
    }

    #[test]
    fn retry_after_seconds_and_dates_are_read() {
        let now: Timestamp = "2026-01-01T00:00:00Z".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "12".parse().unwrap());
        let e = from_response(429, &headers, "", now);
        assert_eq!(e.retry_after, Some(Duration::from_secs(12)));
        assert_eq!(e.reset_at, Some("2026-01-01T00:00:12Z".parse().unwrap()));
        headers.insert(
            "retry-after",
            "Thu, 01 Jan 2026 00:01:00 GMT".parse().unwrap(),
        );
        let e = from_response(429, &headers, "", now);
        assert_eq!(e.retry_after, Some(Duration::from_mins(1)));
    }
}
