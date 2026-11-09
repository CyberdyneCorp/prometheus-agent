//! Scheduled deliveries through the daemon's messaging gateway (spec:
//! scheduled-jobs, Delivery targets; Durable delivery outbox; Push
//! notification flag, cron threads and delivery confirmation).
//!
//! A platform the profile does not enable fails rather than borrowing
//! another profile's adapter. A home-chat target with a configured cron
//! thread arrives as `<platform>::<thread>`. Output is sent as a final,
//! notifying message (overriding an adapter's quiet notification mode)
//! unless `cron.delivery.notify = false`, which sends it silently. A
//! continuable row first links the delivery to a conversation
//! (`cron_continue`); a `bot_chat:<profile>` row is admitted as a turn of
//! that profile's Bot Chat (`cron_admit`). A send is confirmed by the
//! gateway outbox entry it becomes: sent with a platform message id is
//! verified, sent without one (or not settled in time) is unverified; an
//! admitted Bot Chat turn is verified by its session.

use std::sync::Arc;
use std::time::Duration;

use pmt_agent::turn::model::BoxFuture;
use pmt_core::error::{Error, codes};
use pmt_cron::deliver::{BOT_CHAT, SendError, Sender, Sent};
use pmt_gateway::messaging::SendRequest;
use pmt_store::Store;
use pmt_store::cron::DeliveryRow;
use pmt_store::gateway::outbox::OutboundState;

use super::DaemonHost;
use super::cron_admit::BotChatDelivery;

/// How long a send waits for its outbox entry to settle.
const CONFIRM_WAIT: Duration = Duration::from_secs(20);
/// How often the outbox entry is looked at.
const CONFIRM_POLL: Duration = Duration::from_millis(100);

/// Sends one profile's deliveries.
pub(super) struct GatewaySender {
    pub(super) host: Arc<DaemonHost>,
    pub(super) profile: String,
    pub(super) store: Arc<Store>,
}

fn not_running() -> SendError {
    SendError {
        transient: true,
        message: "the messaging gateway is not running yet".to_owned(),
    }
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

    /// The job's name, for labels.
    fn job_name(&self, row: &DeliveryRow) -> String {
        self.store
            .cron_job(&row.job_id)
            .ok()
            .flatten()
            .map_or_else(|| row.job_id.clone(), |job| job.name)
    }

    async fn messaging(&self, row: &DeliveryRow) -> Result<Sent, SendError> {
        let gateway = self.host.gateway.get().ok_or_else(not_running)?;
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
        if row.continuable {
            let name = self.job_name(row);
            to = self
                .host
                .continue_delivery(gateway, &self.profile, to, (&name, &row.body))
                .await
                .map_err(|err| self.refused(target, &err))?;
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
            gateway.enqueue_notifying(&self.profile, &to, &row.body, &[], self.notify())
        }
        .map_err(|err| self.refused(target, &err))?;
        self.confirm(entry.id).await
    }

    /// `cron.delivery.notify` of the profile now.
    fn notify(&self) -> bool {
        self.host
            .profile(&self.profile)
            .map_or(true, |p| p.scope().config().cron.delivery.notify)
    }

    /// Admits a Bot Chat row as one turn of `profile`'s Bot Chat.
    async fn bot_chat(&self, profile: &str, row: &DeliveryRow) -> Result<Sent, SendError> {
        let gateway = self.host.gateway.get().ok_or_else(not_running)?;
        let delivery = BotChatDelivery {
            target: profile,
            receipt: &row.id,
            from: &self.profile,
            job_id: &row.job_id,
            text: &row.body,
        };
        let session = self
            .host
            .admit_bot_chat(gateway, &delivery)
            .await
            .map_err(|err| {
                permanent(format!(
                    "the Bot Chat of profile '{profile}' cannot be reached: {}",
                    err.message()
                ))
            })?;
        Ok(Sent {
            message_id: Some(session),
        })
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
            match row.target.strip_prefix(BOT_CHAT) {
                Some(profile) => self.bot_chat(profile, row).await,
                None => self.messaging(row).await,
            }
        })
    }
}
