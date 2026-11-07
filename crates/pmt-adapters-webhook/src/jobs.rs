//! Webhook-triggered scheduled job runs (spec: scheduled-jobs,
//! Webhook-triggered job runs). A route with `cron_job` names a job of the
//! adapter's profile, checked when the routes load (an unknown job keeps
//! the adapter from connecting). A verified event fires the job through
//! the same at-most-once claim as a manual run, its rendered prompt as the
//! run's transient `## Run Context`, and is answered 202 at once; the
//! job's own delivery, skills and toolsets apply. An event for a paused
//! job or one already running (a burst) is logged and dropped.

use std::collections::BTreeMap;

use pmt_config::schema::WebhookRoute;
use pmt_gateway::messaging::{AdapterError, AdapterLink, CronFire, HttpResponse};
use serde_json::json;

use super::{Call, PLATFORM, Webhook, status};

/// Refuses the routes whose `cron_job` the profile does not have.
pub(super) async fn check_routes(
    link: &dyn AdapterLink,
    routes: &BTreeMap<String, WebhookRoute>,
) -> Result<(), AdapterError> {
    for (name, route) in routes {
        let Some(job) = &route.cron_job else {
            continue;
        };
        if let Err(why) = link.find_cron_job(job).await {
            return Err(AdapterError {
                retryable: false,
                code: "webhook_cron_job".to_owned(),
                message: format!("gateway.platforms.webhook.routes.{name}.cron_job: {why}"),
            });
        }
    }
    Ok(())
}

impl Webhook {
    /// Fires `job` with the rendered event text.
    pub(super) async fn fire_job(&self, call: &Call<'_>, job: &str) -> (HttpResponse, bool) {
        let Some(link) = self.link.lock().expect("link poisoned").clone() else {
            return (HttpResponse::status(503), false);
        };
        match link.fire_cron_job(job, call.text).await {
            Ok(CronFire::Queued { job_id, run_id }) => {
                tracing::info!(
                    platform = PLATFORM,
                    route = call.route_name,
                    job = job_id,
                    run = run_id,
                    "webhook event fired a scheduled job"
                );
                let body = json!({"status": "accepted", "route": call.route_name,
                                  "event": call.event, "delivery_id": call.delivery,
                                  "job_id": job_id, "run_id": run_id});
                (status(202, &body), true)
            }
            Ok(CronFire::Dropped { reason }) => {
                tracing::info!(
                    platform = PLATFORM,
                    route = call.route_name,
                    job,
                    "webhook event dropped: {reason}"
                );
                let body = json!({"status": "dropped", "route": call.route_name,
                                  "reason": reason, "delivery_id": call.delivery});
                (status(200, &body), true)
            }
            Err(why) => {
                tracing::warn!(
                    platform = PLATFORM,
                    route = call.route_name,
                    job,
                    "webhook event could not fire its job: {why}"
                );
                (
                    status(500, &json!({"error": "the job could not be fired"})),
                    false,
                )
            }
        }
    }
}
