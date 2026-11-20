//! `POST /api/cron/fire`, the managed scheduler's fires (spec:
//! scheduled-jobs, Managed scheduler contract): the bearer JWT must verify
//! against the JWKS of a served profile whose managed trigger is in
//! effect (asymmetric keys, `aud`, `purpose = "cron_fire"`, `exp`/`nbf`
//! within 30 s), else 401 `{"error":"invalid fire token"}`; during the
//! emergency stop the answer is 503 with `Retry-After: 60`; otherwise the
//! signalled occurrence goes through the at-most-once claim and the
//! answer is 200 whether it started a run or was already claimed. The
//! profile's next occurrence is armed after each fire.

use std::sync::Arc;

use jiff::Timestamp;
use pmt_cron::managed::{self, Fired};
use pmt_gateway::messaging::{AdapterFuture, Endpoint, HttpRequest, HttpResponse};
use serde_json::{Value, json};

use super::{DaemonHost, Profile};
use crate::cron_host::{self, HostRunner};

/// The endpoint over the daemon's served profiles.
pub(in crate::daemon_cmd) struct FireEndpoint {
    pub(in crate::daemon_cmd) host: Arc<DaemonHost>,
}

fn answer(status: u16, body: &Value) -> HttpResponse {
    let mut response = HttpResponse::status(status);
    response.content_type = Some("application/json".to_owned());
    response.body = body.to_string().into_bytes();
    response
}

/// The fire a request names: its job and instant (`fire_at` as RFC 3339
/// or Unix milliseconds) and the dedup key, which must match them.
fn fire_of(body: &[u8]) -> Option<(String, i64, String)> {
    let body: Value = serde_json::from_slice(body).ok()?;
    let job = body["job_id"].as_str()?.to_owned();
    let at = match &body["fire_at"] {
        Value::String(text) => text.parse::<Timestamp>().ok()?.as_millisecond(),
        other => other.as_i64()?,
    };
    let key = managed::dedup_key(&job, at);
    match body["dedup_key"].as_str() {
        Some(given) if given != key => None,
        _ => Some((job, at, key)),
    }
}

impl Endpoint for FireEndpoint {
    fn handle(&self, request: HttpRequest) -> AdapterFuture<'_, HttpResponse> {
        Box::pin(async move {
            if request.method != "POST" {
                return HttpResponse::status(405);
            }
            let invalid = || answer(401, &json!({"error": "invalid fire token"}));
            let Some(token) = request
                .header("authorization")
                .and_then(|h| h.strip_prefix("Bearer "))
            else {
                return invalid();
            };
            let Some((job, at, key)) = fire_of(&request.body) else {
                return answer(
                    400,
                    &json!({"error": "a fire names job_id, fire_at and its dedup_key"}),
                );
            };
            let now = Timestamp::now();
            let Some((name, profile)) = self.verifying(token, &key, now).await else {
                return invalid();
            };
            if pmt_core::pause::active(profile.scope().home()).is_some() {
                let mut paused = answer(503, &json!({"error": "paused"}));
                paused
                    .headers
                    .push(("retry-after".to_owned(), "60".to_owned()));
                return paused;
            }
            match self.fire(&profile, (&job, at), now).await {
                Ok(Fired::Unknown) => answer(404, &json!({"error": "unknown job"})),
                Ok(Fired::Run(_)) => answer(200, &json!({"status": "started", "dedup_key": key})),
                Ok(Fired::Nothing) => {
                    answer(200, &json!({"status": "already_handled", "dedup_key": key}))
                }
                Err(err) => {
                    tracing::warn!(profile = name.as_str(), error = %err, "managed fire failed");
                    answer(500, &json!({"error": "the fire could not be recorded"}))
                }
            }
        })
    }
}

impl DaemonHost {
    /// Whether a served profile's managed trigger is in effect.
    pub(in crate::daemon_cmd) fn receives_managed_fires(&self) -> bool {
        self.all_profiles()
            .iter()
            .any(|(_, p)| cron_host::managed::in_effect(&p.scope()).is_some())
    }
}

impl FireEndpoint {
    /// The served profile whose managed scheduler signed `token`.
    async fn verifying(
        &self,
        token: &str,
        key: &str,
        now: Timestamp,
    ) -> Option<(String, Arc<Profile>)> {
        for (name, profile) in self.host.all_profiles() {
            if cron_host::managed::verified(&profile.scope(), token, key, now).await {
                return Some((name, profile));
            }
        }
        None
    }

    /// Claims and starts the fire, then arms what comes next.
    async fn fire(
        &self,
        profile: &Arc<Profile>,
        fire: (&str, i64),
        now: Timestamp,
    ) -> Result<Fired, pmt_core::error::Error> {
        let scope = profile.scope();
        let zone = cron_host::profile_zone(&scope)?;
        let fired = managed::claim_fire(&profile.store, fire, now, &zone, &scope.config().cron)?;
        if let Fired::Run(claimed) = &fired {
            let (job, run) = (**claimed).clone();
            let runner = Arc::new(HostRunner::new(
                &scope,
                &self.host.launch,
                Arc::clone(&profile.store),
            ));
            let mut running = cron_host::Running::new();
            cron_host::start(&mut running, &runner, &profile.cron_permits(), job, run);
            pmt_core::spawn::spawn(super::cron::finish(running));
        }
        if let Err(err) = cron_host::managed::sync(&scope, &profile.store).await {
            tracing::warn!("{}", err.message());
        }
        Ok(fired)
    }
}
