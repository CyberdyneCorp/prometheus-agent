//! Typed delivery failures (spec: bot-mode, Typed delivery failures and
//! safe retry): every failed bot turn or delivery carries one reason code,
//! which decides whether it may be retried once.

/// Every reason code.
pub const REASONS: [&str; 13] = [
    "provider_auth_or_access",
    "provider_quota_limit",
    "provider_rate_limit",
    "provider_server_error",
    "context_overflow",
    "missing_config",
    "model_unavailable",
    "runtime_offline",
    "queued_expired",
    "delivery_timeout",
    "target_busy",
    "hop_limit",
    "unknown",
];

/// The reasons a failed delivery turn is retried for (once).
const RETRIED: [&str; 5] = [
    "runtime_offline",
    "delivery_timeout",
    "provider_rate_limit",
    "provider_server_error",
    "context_overflow",
];

/// Whether a delivery that failed with `reason` is retried (once).
#[must_use]
pub fn retried(reason: &str) -> bool {
    RETRIED.contains(&reason)
}

/// Codes whose failure is the profile's configuration.
const MISSING_CONFIG: [&str; 6] = [
    "provider.unknown",
    "provider.disabled",
    "provider.not_configured",
    "provider.missing_base_url",
    "provider.plugin_unavailable",
    "model.not_selected",
];

/// Codes whose failure is the model.
const MODEL_UNAVAILABLE: [&str; 6] = [
    "model.unknown",
    "model.retired",
    "model.not_configured",
    "model.capability_missing",
    "provider.local_runtime_down",
    "provider.external_cli_missing",
];

/// The reason of a failure: `failure` is `<dotted code>: <message>` as a
/// turn reports it, or a reason code itself.
#[must_use]
pub fn of_failure(failure: &str) -> &'static str {
    let (code, message) = failure.split_once(": ").unwrap_or((failure, ""));
    if let Some(reason) = REASONS.iter().find(|r| **r == code) {
        return reason;
    }
    by_code(code).unwrap_or_else(|| by_class(message))
}

fn by_code(code: &str) -> Option<&'static str> {
    if MISSING_CONFIG.contains(&code) || code.starts_with("config.") {
        return Some("missing_config");
    }
    if MODEL_UNAVAILABLE.contains(&code) {
        return Some("model_unavailable");
    }
    match code {
        "provider.auth_failed"
        | "provider.missing_credential"
        | "provider.metered_fallback_refused"
        | "model.data_use_unconfirmed" => Some("provider_auth_or_access"),
        "provider.budget_exhausted" | "budget.exhausted" => Some("provider_quota_limit"),
        "model.context_too_small" => Some("context_overflow"),
        "provider.call_failed" => None,
        _ if code.starts_with("daemon.") || code.starts_with("rpc.") => Some("runtime_offline"),
        _ => Some("unknown"),
    }
}

/// A provider call's class, as its message names it (`<provider>: <class>
/// error ...`).
fn by_class(message: &str) -> &'static str {
    let class = message
        .split(": ")
        .nth(1)
        .and_then(|rest| rest.split(" error").next())
        .unwrap_or_default();
    match class {
        "auth" | "entitlement" => "provider_auth_or_access",
        "billing" => "provider_quota_limit",
        "rate_limit" => "provider_rate_limit",
        "overloaded" | "server_error" | "timeout" | "network" => "provider_server_error",
        "context_overflow" => "context_overflow",
        "config" => "missing_config",
        _ => "unknown",
    }
}

/// The first line of `text`, redacted and shortened: what a sender sees.
#[must_use]
pub fn first_line(text: &str, redactor: &pmt_core::secret::Redactor) -> String {
    let line = text.lines().next().unwrap_or_default();
    let redacted = redactor.redact(line);
    match redacted.char_indices().nth(300) {
        Some((cut, _)) => format!("{}…", &redacted[..cut]),
        None => redacted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_failures_map_to_their_reason() {
        let cases = [
            (
                "provider.call_failed: mock: billing error (HTTP 402): mock: quota exhausted",
                "provider_quota_limit",
            ),
            (
                "provider.call_failed: mock: rate_limit error (HTTP 429): slow down",
                "provider_rate_limit",
            ),
            (
                "provider.call_failed: mock: server_error error (HTTP 500): boom",
                "provider_server_error",
            ),
            (
                "provider.call_failed: mock: context_overflow error (HTTP 400): too long",
                "context_overflow",
            ),
            (
                "provider.auth_failed: mock: auth error (HTTP 401): bad key",
                "provider_auth_or_access",
            ),
            (
                "provider.not_configured: no openrouter key",
                "missing_config",
            ),
            ("model.unknown: no such model", "model_unavailable"),
            ("delivery_timeout", "delivery_timeout"),
            ("something odd", "unknown"),
        ];
        for (failure, reason) in cases {
            assert_eq!(of_failure(failure), reason, "{failure}");
        }
        assert!(retried("provider_server_error"));
        assert!(!retried("provider_quota_limit"));
        assert!(!retried("missing_config"));
    }
}
