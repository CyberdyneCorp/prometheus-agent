//! Telemetry is opt-in and HTTPS-only (spec: security-boundaries,
//! Telemetry is opt-in and HTTPS-only; observability, No outbound
//! telemetry by default).
//!
//! No vendor endpoint exists in the binary: the endpoint comes only from
//! `telemetry.endpoint`, and no network client is configured unless
//! `telemetry.enabled = true` names an `https://` endpoint (plain HTTP only
//! to a loopback address). What would be sent (redacted aggregate
//! counters) arrives with the M2 metrics catalog; until then an enabled
//! gate only yields the client a sender would use.

use std::net::IpAddr;

use pmt_config::schema::TelemetryConfig;
use pmt_core::error::{Error, codes};
use pmt_net::OutboundPolicy;

/// The resolved telemetry setting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Telemetry {
    /// Disabled (the default): nothing is sent and no client exists.
    Off,
    /// Enabled but refused (missing or unsafe endpoint): nothing is sent.
    Refused {
        /// Why, as logged.
        reason: String,
    },
    /// Enabled with an acceptable endpoint.
    Enabled {
        /// The configured endpoint.
        endpoint: String,
    },
}

impl Telemetry {
    /// Resolves `config`, logging a refusal at WARN.
    #[must_use]
    pub fn resolve(config: &TelemetryConfig) -> Self {
        if !config.enabled {
            return Self::Off;
        }
        let resolved = match config.endpoint.as_deref().map(str::trim) {
            None | Some("") => Self::Refused {
                reason:
                    "telemetry.enabled = true but telemetry.endpoint is not set; nothing is sent"
                        .to_owned(),
            },
            Some(endpoint) if acceptable(endpoint) => Self::Enabled {
                endpoint: endpoint.to_owned(),
            },
            Some(endpoint) => Self::Refused {
                reason: format!("refusing to send telemetry to {endpoint}"),
            },
        };
        if let Self::Refused { reason } = &resolved {
            tracing::warn!(code = codes::TELEMETRY_REFUSED.code, "{reason}");
        }
        resolved
    }

    /// The HTTP client a telemetry sender uses: `None` unless enabled, so
    /// nothing is configured to reach the network by default.
    ///
    /// # Errors
    ///
    /// `net.client_init`, `net.extra_ca`.
    pub fn client(&self, outbound: &OutboundPolicy) -> Result<Option<reqwest::Client>, Error> {
        match self {
            Self::Enabled { .. } => pmt_net::client(outbound).map(Some),
            Self::Off | Self::Refused { .. } => Ok(None),
        }
    }
}

/// `https://` anywhere, `http://` only to a loopback host.
fn acceptable(endpoint: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(endpoint) else {
        return false;
    };
    match url.scheme() {
        "https" => url.host_str().is_some(),
        "http" => url.host_str().is_some_and(|host| {
            let bare = host.trim_start_matches('[').trim_end_matches(']');
            bare.parse::<IpAddr>().map_or_else(
                |_| bare.eq_ignore_ascii_case("localhost"),
                pmt_net::listen::is_loopback,
            )
        }),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use pmt_config::schema::TelemetryConfig;

    use super::Telemetry;

    fn config(enabled: bool, endpoint: Option<&str>) -> TelemetryConfig {
        TelemetryConfig {
            enabled,
            endpoint: endpoint.map(str::to_owned),
        }
    }

    #[test]
    fn telemetry_is_off_by_default_and_builds_no_client() {
        let telemetry = Telemetry::resolve(&TelemetryConfig::default());
        assert_eq!(telemetry, Telemetry::Off);
        // Even a configured endpoint sends nothing while disabled.
        assert_eq!(
            Telemetry::resolve(&config(false, Some("https://t.example/ingest"))),
            Telemetry::Off
        );
        let before = pmt_net::clients_built();
        assert!(
            telemetry
                .client(&pmt_net::OutboundPolicy::default())
                .unwrap()
                .is_none()
        );
        assert_eq!(pmt_net::clients_built(), before);
    }

    #[test]
    fn a_plaintext_endpoint_is_refused_with_the_spec_message() {
        let refused = Telemetry::resolve(&config(true, Some("http://example.com/ingest")));
        assert_eq!(
            refused,
            Telemetry::Refused {
                reason: "refusing to send telemetry to http://example.com/ingest".to_owned()
            }
        );
        assert!(
            refused
                .client(&pmt_net::OutboundPolicy::default())
                .unwrap()
                .is_none()
        );
        for bad in [
            "ftp://example.com",
            "example.com/ingest",
            "http://10.0.0.1/x",
        ] {
            assert!(
                matches!(
                    Telemetry::resolve(&config(true, Some(bad))),
                    Telemetry::Refused { .. }
                ),
                "{bad}"
            );
        }
        assert!(matches!(
            Telemetry::resolve(&config(true, None)),
            Telemetry::Refused { .. }
        ));
    }

    #[test]
    fn https_and_loopback_http_endpoints_are_accepted() {
        for good in [
            "https://telemetry.example/ingest",
            "http://localhost:4318/v1",
            "http://127.0.0.1:9000/",
            "http://[::1]:9000/",
        ] {
            assert_eq!(
                Telemetry::resolve(&config(true, Some(good))),
                Telemetry::Enabled {
                    endpoint: good.to_owned()
                },
                "{good}"
            );
        }
    }
}
