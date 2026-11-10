//! A live, authenticated metadata request to a provider (spec:
//! observability, Configuration-centric status: `prometheus status
//! --deep`): the model listing with the route's credential, timed.

use std::time::{Duration, Instant};

use crate::resolve::Route;

/// `GET <models_url>` with `route`'s credential within `within`: the
/// latency of a successful answer, or why it failed (never the
/// credential).
///
/// # Errors
///
/// The HTTP status or transport failure.
pub async fn models(
    client: &reqwest::Client,
    route: &Route,
    models_url: &str,
    within: Duration,
) -> Result<Duration, String> {
    let started = Instant::now();
    let request = crate::transport::authenticate(client.get(models_url).timeout(within), route);
    let response = request.send().await.map_err(|e| {
        if e.is_timeout() {
            format!("no answer within {}s", within.as_secs())
        } else {
            format!("unreachable: {}", e.without_url())
        }
    })?;
    let status = response.status();
    if status.is_success() {
        Ok(started.elapsed())
    } else {
        Err(format!("HTTP {}", status.as_u16()))
    }
}
