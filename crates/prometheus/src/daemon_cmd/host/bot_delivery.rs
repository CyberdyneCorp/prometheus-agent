//! Bot message deliveries in the daemon (spec: bot-mode, `message_agent`
//! tool; Typed delivery failures and safe retry; Bot Mode off switch).
//!
//! Every second the loop looks at each served profile's records. A queued
//! ingress record runs as the target Bot Chat's next turn once the chat
//! is idle (after any running turn and queued prompts); a target busy for
//! longer than `bot_mode.turn_wait_secs` fails it with `target_busy`, and
//! one older than `bot_mode.envelope_ttl_secs` fails with
//! `queued_expired`. A failed turn is retried once, for the transient
//! reasons only, by undoing it and running the same message again (the
//! transcript holds it once). Messages for peers go through the peer's
//! API, messages for other connections through the desktop relay. When a
//! delivery settles, the sender's Bot Chat gets its outcome as a turn.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_gateway::Author;
use pmt_rpc::contracts::SubmitParams;
use pmt_store::bot_messages::{BotMessage, Direction, Outcome};

use super::{DaemonHost, Profile};
use crate::bot_host::reasons;
use crate::bot_host::send::{DELIVERED_KIND, OUTCOME_KIND, open_store};

/// How often the records are looked at.
const TICK: Duration = Duration::from_secs(1);

/// Deliveries running now (each runs once at a time).
#[derive(Default)]
pub(super) struct Running(Mutex<HashSet<String>>);

impl Running {
    fn start(&self, id: &str) -> bool {
        self.0
            .lock()
            .expect("running poisoned")
            .insert(id.to_owned())
    }

    fn end(&self, id: &str) {
        self.0.lock().expect("running poisoned").remove(id);
    }
}

/// The deliveries loop (and the room driver's tick), until the host is
/// gone.
pub(super) async fn schedule(host: std::sync::Weak<DaemonHost>) {
    let running = Arc::new(Running::default());
    loop {
        tokio::time::sleep(TICK).await;
        let Some(host) = host.upgrade() else {
            return;
        };
        for (_, profile) in host.all_profiles() {
            if let Err(err) = tick(&host, &profile, &running) {
                tracing::warn!("bot deliveries: {}", err.message());
            }
        }
        host.drive_rooms();
    }
}

fn age_secs(created: Timestamp) -> u64 {
    u64::try_from(Timestamp::now().duration_since(created).as_secs()).unwrap_or(0)
}

fn tick(
    host: &Arc<DaemonHost>,
    profile: &Arc<Profile>,
    running: &Arc<Running>,
) -> Result<(), Error> {
    for row in profile.store.bot_messages_in(Direction::In, "queued")? {
        let (host, profile) = (Arc::clone(host), Arc::clone(profile));
        let id = row.delivery_id.clone();
        spawn_once(running, id.clone(), async move {
            if let Err(err) = Box::pin(host.deliver_in(&profile, row)).await {
                tracing::warn!(delivery = id, "bot delivery: {}", err.message());
            }
        });
    }
    for row in profile.store.bot_messages_in(Direction::Out, "queued")? {
        if row.route != "local" {
            let (host, profile) = (Arc::clone(host), Arc::clone(profile));
            spawn_once(running, row.delivery_id.clone(), async move {
                Box::pin(host.deliver_out(&profile, &row)).await;
            });
        }
    }
    if listens_to_peers(profile) {
        // Peer listings older than five minutes are asked again, so the
        // protocol roster of Bot Chats learns of peer agents without a
        // desktop listing the roster.
        let (host, profile) = (Arc::clone(host), Arc::clone(profile));
        let name = profile.scope().name().to_string();
        spawn_once(running, format!("peers:{name}"), async move {
            Box::pin(host.refresh_peers(&profile, &name, false)).await;
        });
    }
    for row in profile.store.bot_messages_unnotified()? {
        let (host, profile) = (Arc::clone(host), Arc::clone(profile));
        spawn_once(running, format!("notify:{}", row.delivery_id), async move {
            if let Err(err) = Box::pin(host.notify_sender(&profile, &row)).await {
                tracing::warn!(
                    delivery = row.delivery_id,
                    "bot outcome not told: {}",
                    err.message()
                );
            }
        });
    }
    Ok(())
}

/// Whether `profile` speaks the bot protocol and has peers to list.
fn listens_to_peers(profile: &Profile) -> bool {
    let scope = profile.scope();
    let config = scope.config();
    config.bot_mode.enabled && config.bot_mode.protocol && !config.peers.is_empty()
}

/// Runs `work` unless a task with the same key is still running, so a
/// slow delivery is never started twice by later ticks.
fn spawn_once(
    running: &Arc<Running>,
    key: String,
    work: impl std::future::Future<Output = ()> + Send + 'static,
) {
    if running.start(&key) {
        let running = Arc::clone(running);
        pmt_core::spawn::spawn(async move {
            work.await;
            running.end(&key);
        });
    }
}

/// A failure with its redacted text.
fn failed(reason: &str, error: &str) -> Outcome {
    Outcome::Failed {
        reason: reason.to_owned(),
        error: error.to_owned(),
    }
}

impl DaemonHost {
    /// Runs an ingress record as the target Bot Chat's next turn.
    async fn deliver_in(&self, target: &Arc<Profile>, row: BotMessage) -> Result<(), Error> {
        let config = target.scope().config().bot_mode.clone();
        let outcome = if !config.enabled {
            failed("missing_config", "Bot Mode is off for this bot")
        } else if config.envelope_ttl_secs > 0
            && age_secs(row.created_at) > config.envelope_ttl_secs
        {
            failed(
                "queued_expired",
                "the message waited too long to be delivered",
            )
        } else {
            let (session, _) = self.ensure_bot_chat(target).await?;
            if self.chat_busy(target, &session).await {
                if age_secs(row.created_at) <= config.turn_wait_secs {
                    return Ok(());
                }
                failed(
                    "target_busy",
                    "the bot stayed busy longer than bot_mode.turn_wait_secs",
                )
            } else {
                if !target.store.bot_message_claim(&row.delivery_id, &session)? {
                    return Ok(());
                }
                Box::pin(self.run_delivery(target, &row, &session)).await
            }
        };
        self.settle(target, &row, &outcome)
    }

    /// Whether the Bot Chat runs a turn or holds queued input.
    async fn chat_busy(&self, profile: &Profile, session: &str) -> bool {
        let Ok(id) = super::session_id(session) else {
            return false;
        };
        let loaded = profile.loaded.lock().await.get(&id).cloned();
        loaded.is_some_and(|s| {
            s.busy.load(std::sync::atomic::Ordering::Acquire)
                || !s.queue.lock().expect("queue poisoned").is_empty()
        })
    }

    /// The delivery's turn (and its one retry); how it ended.
    async fn run_delivery(
        &self,
        target: &Arc<Profile>,
        row: &BotMessage,
        session: &str,
    ) -> Outcome {
        let text = format!("Message from {}\n\n{}", row.attribution, row.message);
        let first = Box::pin(self.delivery_turn(target, row, session, &text)).await;
        let Err(failure) = first else {
            return delivered(first);
        };
        let reason = reasons::of_failure(&failure);
        if !reasons::retried(reason) {
            return Self::failure(target, reason, &failure);
        }
        tracing::info!(
            delivery = row.delivery_id,
            reason,
            "bot delivery retried once"
        );
        if let Err(err) = self.prepare_retry(target, session, reason).await {
            return Self::failure(target, "unknown", err.message());
        }
        let _ = target.store.bot_message_retry(&row.delivery_id);
        match Box::pin(self.delivery_turn(target, row, session, &text)).await {
            Ok(reply) => delivered(Ok(reply)),
            Err(failure) => Self::failure(target, reasons::of_failure(&failure), &failure),
        }
    }

    fn failure(target: &Profile, reason: &str, text: &str) -> Outcome {
        let redactor = target.scope().secrets().redactor().unwrap_or_default();
        failed(reason, &reasons::first_line(text, &redactor))
    }

    /// Undoes the failed turn (its message is run again, so the transcript
    /// holds it once); a context overflow compresses the chat first.
    async fn prepare_retry(
        &self,
        target: &Arc<Profile>,
        session: &str,
        reason: &str,
    ) -> Result<(), Error> {
        let id = super::session_id(session)?;
        target.store.undo_last_turn(id, None)?;
        if reason == "context_overflow" {
            Box::pin(self.compress_session(target, session)).await?;
        }
        Ok(())
    }

    /// Compresses `session` in place (`/compress`).
    pub(super) async fn compress_session(
        &self,
        target: &Arc<Profile>,
        session: &str,
    ) -> Result<(), Error> {
        let Some(gateway) = self.gateway.get() else {
            return Ok(());
        };
        let params = pmt_rpc::contracts::DispatchParams {
            session_id: session.to_owned(),
            name: "compress".to_owned(),
            args: String::new(),
        };
        Box::pin(self.dispatch_command(target, params, gateway.link(session)))
            .await
            .map(drop)
    }

    /// One turn of the delivery: the reply, or `<code>: <message>`.
    async fn delivery_turn(
        &self,
        target: &Arc<Profile>,
        row: &BotMessage,
        session: &str,
        text: &str,
    ) -> Result<String, String> {
        let Some(gateway) = self.gateway.get() else {
            return Err("runtime_offline: the gateway is not running".to_owned());
        };
        let params = SubmitParams {
            session_id: session.to_owned(),
            text: text.to_owned(),
            images: Vec::new(),
            files: Vec::new(),
            run_budget_secs: None,
            input_id: None,
            author: None,
        };
        let author = Author {
            client_kind: DELIVERED_KIND.to_owned(),
            client_id: row.delivery_id.clone(),
            principal: target.scope().name().to_string(),
            sent_at: Some(Timestamp::now().to_string()),
            prefix: None,
            origin: None,
        };
        let timeout = Duration::from_secs(target.scope().config().bot_mode.relay_turn_timeout_secs);
        match Box::pin(self.submit_turn(
            target,
            params,
            (author, gateway.link(session)),
            Some(pmt_config::schema::BusyInputMode::Queue),
            None,
        ))
        .await
        {
            Ok(submitted) => super::turn_results::wait(&submitted.turn_id, timeout)
                .await
                .unwrap_or_else(|| Err("delivery_timeout".to_owned())),
            Err(err) => Err(format!("{}: {}", err.code().code, err.message())),
        }
    }

    /// Records how the delivery ended on both sides.
    fn settle(&self, target: &Profile, row: &BotMessage, outcome: &Outcome) -> Result<(), Error> {
        target
            .store
            .bot_message_settle(Direction::In, &row.delivery_id, outcome)?;
        let sender = match self.profile(&row.sender) {
            Ok(sender) => Arc::clone(&sender.store),
            Err(_) => Arc::new(open_store(&target.scope(), &row.sender)?),
        };
        sender.bot_message_settle(Direction::Out, &row.delivery_id, outcome)?;
        Ok(())
    }

    /// Carries a message to a peer gateway (its Bot Chat, as a run) or
    /// waits for a courier to carry it to another connection.
    async fn deliver_out(&self, sender: &Arc<Profile>, row: &BotMessage) {
        let outcome = match row.route.as_str() {
            "peer" => Box::pin(self.to_peer(sender, row)).await,
            _ => Self::relayed(sender, row),
        };
        if let Some(outcome) = outcome
            && let Err(err) =
                sender
                    .store
                    .bot_message_settle(Direction::Out, &row.delivery_id, &outcome)
        {
            tracing::warn!(
                delivery = row.delivery_id,
                "bot delivery not settled: {}",
                err.message()
            );
        }
    }

    async fn to_peer(&self, sender: &Arc<Profile>, row: &BotMessage) -> Option<Outcome> {
        let scope = sender.scope();
        let (peer, agent) = row
            .target
            .split_once('/')
            .unwrap_or((&row.target, "default"));
        let agent = (agent != "default").then_some(agent);
        let client = match crate::daemon_cmd::peers::client::Peer::of(&scope, peer) {
            Ok(client) => client,
            Err(err) => return Some(failed("missing_config", err.message())),
        };
        let text = format!("Message from {}\n\n{}", row.attribution, row.message);
        let outcome = async {
            let session = client.ensure_bot_chat(agent).await?;
            let run = client.run(agent, &session, &text, &row.delivery_id).await?;
            let run_id = run["run_id"]
                .as_str()
                .or_else(|| run["id"].as_str())
                .unwrap_or_default();
            peer_reply(&client, agent, run_id).await
        }
        .await;
        Some(match outcome {
            Ok(reply) => Outcome::Delivered { reply },
            Err(crate::daemon_cmd::peers::client::PeerError::KeyRejected) => failed(
                "provider_auth_or_access",
                "the peer refused this gateway's key",
            ),
            Err(crate::daemon_cmd::peers::client::PeerError::Unreachable(why)) => {
                failed("runtime_offline", &why)
            }
            Err(other) => failed("unknown", &format!("{other:?}")),
        })
    }

    /// The outcome of a relayed envelope, once its courier settled it.
    fn relayed(sender: &Profile, row: &BotMessage) -> Option<Outcome> {
        let config = sender.scope().config().bot_mode.clone();
        let state = crate::a2a_host::relay::outcome(
            &sender.store,
            &row.delivery_id,
            Timestamp::now(),
            &config,
        )
        .ok()
        .flatten()?;
        match state {
            pmt_store::bot_relay::RelayState::Delivered { reply } => {
                Some(Outcome::Delivered { reply })
            }
            pmt_store::bot_relay::RelayState::Failed { reason } => {
                Some(failed(reasons::of_failure(&reason), &reason))
            }
            _ => None,
        }
    }

    /// Tells the sender's Bot Chat how its message ended, as a turn.
    async fn notify_sender(&self, sender: &Arc<Profile>, row: &BotMessage) -> Result<(), Error> {
        let (session, _) = self.ensure_bot_chat(sender).await?;
        let text = outcome_text(row);
        let Some(gateway) = self.gateway.get() else {
            return Err(Error::new(
                &pmt_core::error::codes::RPC_NOT_SERVED,
                "the gateway is not running yet",
            ));
        };
        let params = SubmitParams {
            session_id: session.clone(),
            text,
            images: Vec::new(),
            files: Vec::new(),
            run_budget_secs: None,
            input_id: None,
            author: None,
        };
        let author = Author {
            client_kind: OUTCOME_KIND.to_owned(),
            client_id: row.delivery_id.clone(),
            principal: sender.scope().name().to_string(),
            sent_at: Some(Timestamp::now().to_string()),
            prefix: None,
            origin: None,
        };
        sender.store.bot_message_notified(&row.delivery_id)?;
        Box::pin(self.submit_turn(
            sender,
            params,
            (author, gateway.link(&session)),
            Some(pmt_config::schema::BusyInputMode::Queue),
            None,
        ))
        .await?;
        Ok(())
    }
}

/// A reply, an intentional silence being an empty one.
fn delivered(result: Result<String, String>) -> Outcome {
    let reply = result.unwrap_or_default();
    let silent = pmt_cron::deliver::is_silent(&reply) || reply.trim() == "NO_REPLY";
    Outcome::Delivered {
        reply: if silent { String::new() } else { reply },
    }
}

/// What the sender's Bot Chat is told.
fn outcome_text(row: &BotMessage) -> String {
    let target = &row.target;
    match (row.state.as_str(), &row.reply) {
        ("delivered", Some(reply)) if reply.is_empty() => format!(
            "Reply from {target} to your message ({}): (no reply)",
            row.delivery_id
        ),
        ("delivered", Some(reply)) => format!(
            "Reply from {target} to your message ({}):\n\n{reply}",
            row.delivery_id
        ),
        _ => format!(
            "Your message to {target} ({}) failed [reason: {}]: {}",
            row.delivery_id,
            row.reason.as_deref().unwrap_or("unknown"),
            row.error.as_deref().unwrap_or_default()
        ),
    }
}

/// Waits for a peer run's reply (up to the relay turn bound).
async fn peer_reply(
    client: &crate::daemon_cmd::peers::client::Peer,
    agent: Option<&str>,
    run_id: &str,
) -> Result<String, crate::daemon_cmd::peers::client::PeerError> {
    for _ in 0..600 {
        let status = client.status(agent, run_id).await?;
        match status["status"].as_str() {
            Some("completed") => {
                return Ok(status["output"].as_str().unwrap_or_default().to_owned());
            }
            Some("failed" | "cancelled" | "interrupted") => {
                return Err(crate::daemon_cmd::peers::client::PeerError::Rejected(
                    200,
                    status["error"].to_string(),
                ));
            }
            _ => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
    Err(crate::daemon_cmd::peers::client::PeerError::Unreachable(
        "the peer's run did not finish".to_owned(),
    ))
}
