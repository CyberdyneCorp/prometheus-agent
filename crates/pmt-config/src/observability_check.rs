//! Whole-config checks of `[observability]` the schema cannot express
//! (spec: observability, Opt-in OpenTelemetry export; Health endpoints):
//! enabled export names its collector, and the health listener is
//! loopback only.

use std::net::SocketAddr;

use pmt_core::error::{Error, codes};

use crate::schema::Config;

/// Checks `[observability.otel]` and `[observability.health]`.
///
/// # Errors
///
/// `config.invalid` naming the key.
pub fn check(config: &Config) -> Result<(), Error> {
    let otel = &config.observability.otel;
    if otel.enabled {
        match otel.endpoint.as_deref().map(str::trim) {
            None | Some("") => {
                return Err(invalid(
                    "observability.otel.enabled = true requires observability.otel.endpoint \
                     (the collector's OTLP URL)",
                ));
            }
            Some(url) if !url.starts_with("https://") && !url.starts_with("http://") => {
                return Err(invalid(format!(
                    "observability.otel.endpoint {url:?} is not an http:// or https:// URL"
                )));
            }
            Some(_) => {}
        }
    }
    let bind = &config.observability.health.bind;
    match bind.parse::<SocketAddr>() {
        Ok(addr) if addr.ip().is_loopback() => Ok(()),
        _ => Err(invalid(format!(
            "observability.health.bind {bind:?} must be a loopback address and port \
             (127.0.0.1:0, [::1]:9464)"
        ))),
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(&codes::CONFIG_INVALID, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_export_without_an_endpoint_is_a_config_error() {
        let mut config = Config::default();
        assert!(check(&config).is_ok());
        config.observability.otel.enabled = true;
        let err = check(&config).unwrap_err();
        assert_eq!(err.code(), &codes::CONFIG_INVALID);
        assert_eq!(err.kind().exit_code(), 78);
        assert!(err.to_string().contains("observability.otel.endpoint"));
        config.observability.otel.endpoint = Some("http://127.0.0.1:4318".into());
        assert!(check(&config).is_ok());
    }

    #[test]
    fn the_health_listener_stays_on_loopback() {
        let mut config = Config::default();
        config.observability.health.bind = "0.0.0.0:9464".into();
        assert!(check(&config).is_err());
        config.observability.health.bind = "[::1]:9464".into();
        assert!(check(&config).is_ok());
    }
}
