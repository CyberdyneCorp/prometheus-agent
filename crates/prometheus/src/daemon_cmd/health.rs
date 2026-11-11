//! The daemon's health surface (spec: observability, Health endpoints;
//! Component health model): the loopback health listener and the store
//! components reported while profiles open.

use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_core::cancel::CancelToken;
use pmt_core::error::{Error, codes};
use pmt_obs::health::{self, State};

/// Binds and serves the health listener of the launch profile's
/// `[observability.health]` and `[observability.metrics_endpoint]`; its
/// URL (`http://127.0.0.1:<port>`).
///
/// # Errors
///
/// `config.parse` for a bad bind address, `net.listen*`.
pub(super) async fn listen(scope: &ProfileScope, stop: CancelToken) -> Result<String, Error> {
    let config = scope.config();
    let addr = config.observability.health.bind.parse().map_err(|e| {
        Error::new(
            &codes::CONFIG_PARSE,
            format!("observability.health.bind: {e}"),
        )
    })?;
    let listener = pmt_gateway::health_http::listen(addr).await?;
    let local = listener
        .local_addr()
        .map_err(|e| Error::new(&codes::NET_LISTEN, format!("daemon health: {e}")))?;
    let options = pmt_gateway::health_http::HealthOptions {
        metrics: config.observability.metrics_endpoint.enabled,
    };
    pmt_gateway::health_http::serve(listener, options, stop);
    let url = format!("http://{local}");
    tracing::info!(health_url = url.as_str(), "health listener ready");
    Ok(url)
}

/// Marks every served profile's store and the scheduler as not ready
/// until they open and start.
pub(super) fn starting(profiles: &[ProfileScope]) {
    let health = health::global();
    for scope in profiles {
        health.report(Some(scope.name().as_str()), "store", State::Down, "opening");
    }
    health.report(None, "scheduler", State::Down, "starting");
}

/// Opens profile `name`'s store with `open` (schema migrations included),
/// reporting the store component down until it is open and migrated;
/// `delay` is the debug-build test seam standing in for a slow migration.
pub(super) fn opening_store<T>(
    name: &str,
    delay: Option<Duration>,
    open: impl FnOnce() -> Result<T, Error>,
) -> Result<T, Error> {
    let health = health::global();
    health.report(Some(name), "store", State::Down, "opening and migrating");
    if let Some(delay) = delay {
        std::thread::sleep(delay);
    }
    match open() {
        Ok(opened) => {
            health.report(Some(name), "store", State::Ok, "open");
            Ok(opened)
        }
        Err(err) => {
            health.report(Some(name), "store", State::Down, err.message());
            Err(err)
        }
    }
}
