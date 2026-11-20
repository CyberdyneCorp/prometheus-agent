//! A conversation's turns in proxy mode (spec: api-server, Gateway proxy
//! mode): no local session or agent; the turn runs on the remote API
//! server and its answer streams back through the lane like a local
//! turn's, so delivery, typing and the started/finished marks are the
//! same. A refused key puts the profile's adapters in `fatal`
//! (`proxy_auth`); an unreachable remote answers with
//! [`UNAVAILABLE`](crate::messaging::upstream::UNAVAILABLE) and queues
//! nothing.

use pmt_rpc::catalog::contracts::{TurnError, TurnFinished};

use super::{Lane, Msg, Pending};
use crate::messaging::adapter::AdapterError;
use crate::messaging::upstream::{AUTH_CODE, Failure, UNAVAILABLE, Upstream};

impl Lane {
    /// The remote API server this profile's turns run on, when it is in
    /// proxy mode.
    pub(super) fn upstream(&self) -> Option<Upstream> {
        let scope = self.config()?;
        let proxy = &scope.config().gateway.proxy;
        let url = proxy.url.as_deref()?;
        let key =
            proxy
                .key_secret
                .as_ref()
                .and_then(|name| match scope.secrets().store().get(&name.0) {
                    Ok(found) => found.map(|secret| secret.expose().to_owned()),
                    Err(err) => {
                        tracing::warn!(error = %err, "the proxy key secret could not be read");
                        None
                    }
                });
        Some(Upstream::new(url, key, scope.dir()))
    }

    /// Runs `pending` on `upstream`.
    pub(super) async fn forward(&mut self, pending: Pending, upstream: Upstream) {
        let turn_id = format!("proxy_{}", crate::tickets::random_hex(12));
        let text = pending.text.clone();
        self.started(turn_id.clone(), pending, false).await;
        let (tx, key) = (self.tx.clone(), self.key.to_string());
        let (shared, profile) = (std::sync::Arc::clone(&self.shared), self.profile.clone());
        pmt_core::spawn::spawn(async move {
            let deltas = tx.clone();
            let outcome = upstream
                .turn(&key, &text, |piece| {
                    if let Some(lane) = deltas.upgrade() {
                        let _ = lane.send(Msg::Delta(piece));
                    }
                })
                .await;
            if outcome == Err(Failure::Auth) {
                let error = AdapterError {
                    retryable: false,
                    code: AUTH_CODE.to_owned(),
                    message: "the upstream agent refused gateway.proxy.key_secret".to_owned(),
                };
                crate::messaging::adapters::fail_profile(&shared, &profile, &error).await;
            }
            if let Some(lane) = tx.upgrade() {
                let _ = lane.send(Msg::Finished(finished(turn_id, outcome)));
            }
        });
    }
}

fn finished(turn_id: String, outcome: Result<Option<String>, Failure>) -> TurnFinished {
    let failed = |message: String| TurnError {
        kind: "unavailable".to_owned(),
        code: "gateway.proxy_failed".to_owned(),
        message,
        hint: None,
    };
    let (exit_reason, final_response, error) = match outcome {
        Ok(reply) => ("completed", reply, None),
        Err(Failure::Unreachable) => ("failed", Some(UNAVAILABLE.to_owned()), None),
        Err(Failure::Auth) => (
            "failed",
            None,
            Some(failed(
                "the upstream agent refused this gateway's key".to_owned(),
            )),
        ),
        Err(Failure::Failed(why)) => ("failed", None, Some(failed(why))),
    };
    TurnFinished {
        turn_id,
        exit_reason: exit_reason.to_owned(),
        final_response,
        error,
        usage: None,
        report: None,
    }
}
