//! Scheduled deliveries through the daemon's messaging gateway (spec:
//! scheduled-jobs, Delivery targets; Durable delivery outbox; Push
//! notification flag, cron threads and delivery confirmation).
//!
//! A platform the profile does not enable fails rather than borrowing
//! another profile's adapter. A home-chat target with a configured cron
//! thread arrives as `<platform>::<thread>`. A send is confirmed by the
//! gateway outbox entry it becomes: sent with a platform message id is
//! verified, sent without one (or not settled in time) is unverified.

use std::sync::Arc;
use std::time::Duration;

use pmt_agent::turn::model::BoxFuture;
use pmt_core::error::{Error, codes};
use pmt_cron::deliver::{BOT_CHAT, SendError, Sender, Sent};
use pmt_gateway::messaging::SendRequest;
use pmt_store::Store;
use pmt_store::cron::DeliveryRow;
use pmt_store::gateway::outbox::OutboundState;

use super::send_tool::GatewayCell;

/// How long a send waits for its outbox entry to settle.
const CONFIRM_WAIT: Duration = Duration::from_secs(20);
/// How often the outbox entry is looked at.
const CONFIRM_POLL: Duration = Duration::from_millis(100);

/// Sends one profile's deliveries.
pub(super) struct GatewaySender {
    pub(super) gateway: GatewayCell,
    pub(super) profile: String,
    pub(super) store: Arc<Store>,
}

fn permanent(message: String) -> SendError {
    SendError {
        transient: false,
        message,
    }
}

impl GatewaySender {
    fn refused(&self, target: &str, err: &Error) -> SendError {
        let platform = target.split(':').next().unwrap_or(target);
        let message = if err.code().code == codes::GATEWAY_PLATFORM_NOT_CONFIGURED.code {
            format!("{platform} not configured for profile {}", self.profile)
        } else {
            err.message().to_owned()
        };
        SendError {
            transient: err.code().code == codes::STORE_BUSY.code,
            message,
        }
    }

    async fn messaging(&self, row: &DeliveryRow) -> Result<Sent, SendError> {
        let Some(gateway) = self.gateway.get() else {
            return Err(SendError {
                transient: true,
                message: "the messaging gateway is not running yet".to_owned(),
            });
        };
        let (target, thread) = match row.target.split_once("::") {
            Some((platform, thread)) => (platform, Some(thread.to_owned())),
            None => (row.target.as_str(), None),
        };
        let mut to = gateway
            .resolve_target(&self.profile, target)
            .await
            .map_err(|err| self.refused(target, &err))?;
        if thread.is_some() {
            to.thread_id = thread;
        }
        let entry = if row.warning {
            let request = SendRequest {
                target: to.label(),
                text: row.body.clone(),
                attachments: Vec::new(),
                warning: true,
            };
            gateway.send(&self.profile, &request).await
        } else {
            gateway.enqueue(&self.profile, &to, &row.body, &[])
        }
        .map_err(|err| self.refused(target, &err))?;
        self.confirm(entry.id).await
    }

    /// Waits for outbox entry `id` to settle.
    async fn confirm(&self, id: i64) -> Result<Sent, SendError> {
        let deadline = tokio::time::Instant::now() + CONFIRM_WAIT;
        while tokio::time::Instant::now() < deadline {
            let entry = self
                .store
                .outbound(id)
                .map_err(|e| permanent(e.message().to_owned()))?;
            match entry {
                Some(entry) => match entry.state {
                    OutboundState::Sent | OutboundState::Delivered | OutboundState::Read => {
                        return Ok(Sent {
                            message_id: entry.platform_message_id,
                        });
                    }
                    OutboundState::Failed => {
                        return Err(permanent(
                            entry.last_error.unwrap_or_else(|| "send failed".to_owned()),
                        ));
                    }
                    OutboundState::Unknown | OutboundState::Suppressed => {
                        return Ok(Sent { message_id: None });
                    }
                    _ => {}
                },
                None => return Ok(Sent { message_id: None }),
            }
            tokio::time::sleep(CONFIRM_POLL).await;
        }
        tracing::info!(entry = id, "cron delivery not confirmed in time");
        Ok(Sent { message_id: None })
    }
}

impl Sender for GatewaySender {
    fn send<'a>(&'a self, row: &'a DeliveryRow) -> BoxFuture<'a, Result<Sent, SendError>> {
        Box::pin(async move {
            if let Some(profile) = row.target.strip_prefix(BOT_CHAT) {
                return Err(permanent(format!(
                    "the Bot Chat of profile '{profile}' cannot be reached: Bot Chat deliveries \
                     are not available in this build"
                )));
            }
            self.messaging(row).await
        })
    }
}
