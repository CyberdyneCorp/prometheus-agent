//! Injected request faults, classified exactly like the provider
//! responses they stand for: each becomes the HTTP status, headers and
//! error body a real provider sends, run through
//! [`classify::from_response`], so the retry policy and the fallback chain
//! see the same [`ProviderError`] they would in production.

use jiff::Timestamp;
use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

use super::script::Fault;
use crate::classify::{self, ProviderError};
use crate::transport;

/// The classified failure `fault` stands for.
pub(super) fn error(fault: Fault) -> ProviderError {
    let mut headers = HeaderMap::new();
    let (status, kind, message) = match fault {
        Fault::Timeout => {
            return transport::timed_out("no response within the request timeout (mock fault)");
        }
        Fault::RateLimit { retry_after_secs } => {
            if let Some(secs) = retry_after_secs {
                headers.insert(RETRY_AFTER, HeaderValue::from(secs));
            }
            (429, "rate_limit_error", "mock: rate limited")
        }
        Fault::ServerError { status } => (status, "server_error", "mock: server error"),
        Fault::AuthError => (401, "authentication_error", "mock: invalid credential"),
        Fault::ContextOverflow => (
            400,
            "invalid_request_error",
            "mock: the prompt exceeds the model's maximum context length (context_length_exceeded)",
        ),
    };
    let body = serde_json::json!({"error": {"type": kind, "message": message}}).to_string();
    classify::from_response(status, &headers, &body, Timestamp::now())
}
