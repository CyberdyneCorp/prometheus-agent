//! `message_agent` (spec: bot-mode, `message_agent` tool; Bot Mode off
//! switch; desktop-app, Agent-to-agent messages from bot chats): the
//! sending side. A message to a profile of this home becomes a durable
//! ingress record in the target's store (its daemon runs it as the target
//! Bot Chat's next turn); one to a peer or another desktop connection is
//! carried by the daemon (peer API) or a courier (relay envelope). The
//! tool returns at once: `queued` with the delivery id. The outcome comes
//! back later as a message in the sender's Bot Chat.

use std::sync::Arc;

use pmt_agent::tools::BotPort;
use pmt_agent::turn::model::BoxFuture;
use pmt_config::ProfileScope;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_store::Store;
use pmt_store::bot_messages::{BotMessage, Direction};
use pmt_tools::ToolResult;
use pmt_tools::agent_level_bots::MAX_MESSAGE_CHARS;
use serde_json::{Value, json};

use super::roster::{self, Resolved, Teammate};

/// The author kind of a delivered bot message's turn (its client id is
/// the delivery id).
pub const DELIVERED_KIND: &str = "bot";
/// The author kind of the turn that brings a sender its outcome.
pub const OUTCOME_KIND: &str = "bot-reply";

/// The port of one Bot Chat.
pub struct Port {
    scope: ProfileScope,
    store: Arc<Store>,
}

/// `message_agent` for the Bot Chat sessions of `scope`.
#[must_use]
pub fn port(scope: &ProfileScope, store: &Arc<Store>) -> Arc<dyn BotPort> {
    Arc::new(Port {
        scope: scope.clone(),
        store: Arc::clone(store),
    })
}

/// A refusal the sending agent reads, with its reason code.
fn refused(text: &str, reason: &str) -> ToolResult {
    ToolResult::error(format!("{text} [reason: {reason}]"))
}

/// The hops of the chain the turn now running in `session` continues:
/// a turn a delivered bot message (or an outcome) started carries its
/// delivery's hops; a turn a person started carries none.
///
/// # Errors
///
/// Store errors.
pub fn current_hops(store: &Store, session: SessionId) -> Result<u32, Error> {
    let messages = store.messages(session, pmt_store::Visibility::Active)?;
    let Some(author) = messages
        .iter()
        .rev()
        .find(|m| m.role == pmt_store::Role::User)
        .and_then(|m| m.metadata.as_ref())
        .and_then(|m| m.get("author"))
    else {
        return Ok(0);
    };
    let id = author["client_id"].as_str().unwrap_or_default();
    let direction = match author["client_kind"].as_str() {
        Some(DELIVERED_KIND) => Direction::In,
        Some(OUTCOME_KIND) => Direction::Out,
        _ => return Ok(0),
    };
    Ok(store.bot_message(direction, id)?.map_or(0, |m| m.hops))
}

impl Port {
    fn send(&self, session: SessionId, arguments: &Value) -> Result<ToolResult, Error> {
        let target = arguments["target"].as_str().unwrap_or_default();
        let message = arguments["message"].as_str().unwrap_or_default();
        if target.trim().is_empty() || message.trim().is_empty() {
            return Ok(ToolResult::error(
                "message_agent needs a target and a message",
            ));
        }
        let length = message.chars().count();
        if length > MAX_MESSAGE_CHARS {
            return Ok(ToolResult::error(format!(
                "the message has {length} characters; at most {MAX_MESSAGE_CHARS} are sent"
            )));
        }
        let config = self.scope.config();
        let max = config.bot_mode.max_hops;
        let hops = current_hops(&self.store, session)?;
        if hops >= max {
            return Ok(refused(
                &format!("agent message hop limit ({max}) reached"),
                "hop_limit",
            ));
        }
        let teammates = roster::local(self.scope.home())?;
        let peers: Vec<String> = config.peers.keys().cloned().collect();
        let resolved = match roster::resolve(target, &teammates, &peers) {
            Ok(resolved) => resolved,
            Err(unresolved) => return Ok(ToolResult::error(unresolved.0)),
        };
        let me = roster::of(&self.scope, &self.store);
        if matches!(&resolved, Resolved::Local(t) if t.profile == me.profile) {
            return Ok(ToolResult::error(
                "message_agent reaches other bots, not yourself",
            ));
        }
        let outgoing = Outgoing {
            me: &me,
            message,
            hops: hops + 1,
            session,
        };
        match resolved {
            Resolved::Local(target) => self.to_local(&outgoing, &target),
            Resolved::Peer { peer, agent } => {
                self.to_remote(&outgoing, "peer", &format!("{peer}/{agent}"))
            }
            Resolved::Connection(handle) => self.to_connection(&outgoing, &handle),
        }
    }

    fn to_local(&self, out: &Outgoing<'_>, target: &Teammate) -> Result<ToolResult, Error> {
        if !target.enabled {
            return Ok(refused(
                &format!("{} has Bot Mode turned off", target.handle()),
                "missing_config",
            ));
        }
        let row = out.row(&new_id(), "local", &target.profile);
        let target_store = open_store(&self.scope, &target.profile)?;
        target_store.bot_message_put(Direction::In, &row)?;
        self.store.bot_message_put(Direction::Out, &row)?;
        Ok(queued(&row))
    }

    fn to_remote(
        &self,
        out: &Outgoing<'_>,
        route: &str,
        target: &str,
    ) -> Result<ToolResult, Error> {
        let row = out.row(&new_id(), route, target);
        self.store.bot_message_put(Direction::Out, &row)?;
        Ok(queued(&row))
    }

    fn to_connection(&self, out: &Outgoing<'_>, handle: &str) -> Result<ToolResult, Error> {
        let text = format!("Message from {}\n\n{}", out.me.attribution(), out.message);
        let envelope =
            crate::a2a_host::relay::enqueue(&self.store, &out.me.handle(), handle, &text)?;
        let row = out.row(&envelope.delivery_id, "connection", handle);
        self.store.bot_message_put(Direction::Out, &row)?;
        Ok(queued(&row))
    }
}

/// One message on its way.
struct Outgoing<'a> {
    me: &'a Teammate,
    message: &'a str,
    hops: u32,
    session: SessionId,
}

impl Outgoing<'_> {
    fn row(&self, id: &str, route: &str, target: &str) -> BotMessage {
        BotMessage {
            delivery_id: id.to_owned(),
            route: route.to_owned(),
            sender: self.me.profile.clone(),
            attribution: self.me.attribution(),
            target: target.to_owned(),
            message: self.message.to_owned(),
            hops: self.hops,
            origin_session: Some(self.session.to_string()),
            session_id: None,
            state: "queued".to_owned(),
            reason: None,
            error: None,
            reply: None,
            attempts: 0,
            notified: false,
            created_at: jiff::Timestamp::now(),
        }
    }
}

fn new_id() -> String {
    format!("dlv_{}", ulid::Ulid::generate().to_string().to_lowercase())
}

fn queued(row: &BotMessage) -> ToolResult {
    ToolResult::ok(
        json!({
            "status": "queued",
            "delivery_id": row.delivery_id,
            "target": row.target,
            "note": "the reply arrives later as a message in this chat"
        })
        .to_string(),
    )
}

/// The store of `profile`, a profile of the same home as `scope`.
///
/// # Errors
///
/// `profile.not_found`; store errors.
pub fn open_store(scope: &ProfileScope, profile: &str) -> Result<Store, Error> {
    if scope.name().as_str() == profile {
        return Store::open(scope);
    }
    let home = pmt_config::Home::open(scope.home())?;
    let dir = home.existing_profile(&pmt_config::ProfileName::parse(profile)?)?;
    let other = ProfileScope::open(
        dir,
        &pmt_config::Overrides::default(),
        pmt_config::Launch::Other,
    )?;
    Store::open(&other)
}

impl BotPort for Port {
    fn message_agent(&self, session: SessionId, arguments: Value) -> BoxFuture<'_, ToolResult> {
        Box::pin(async move {
            self.send(session, &arguments)
                .unwrap_or_else(|err| ToolResult::error(err.message().to_owned()))
        })
    }
}
