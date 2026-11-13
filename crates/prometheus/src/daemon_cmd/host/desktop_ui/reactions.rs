//! `react_to_message` (spec: desktop-app, Agent reactions): the agent's
//! single reaction on a message of its own conversation, kept in the
//! message's host metadata (never in what the model sees) and announced
//! to attached clients with `message.reaction`.

use pmt_core::id::MessageId;
use pmt_rpc::contracts::{MessageReacted, MessageReaction, Reaction};
use pmt_store::{Message, Role, Visibility};
use pmt_tools::ToolResult;
use serde_json::{Value, json};

use super::{UiPort, answer, fail};

/// Who the tool reacts as.
const AGENT: &str = "agent";

/// The reactions kept in a message's metadata.
#[must_use]
pub(in super::super) fn reactions_of(metadata: Option<&Value>) -> Vec<Reaction> {
    metadata
        .and_then(|m| m.get("reactions"))
        .and_then(|r| serde_json::from_value(r.clone()).ok())
        .unwrap_or_default()
}

/// Runs one `react_to_message` call.
pub(super) fn react(port: &UiPort, args: &Value) -> ToolResult {
    let emoji = args
        .get("emoji")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let messages = match port.store.messages(port.session, Visibility::Active) {
        Ok(messages) => messages,
        Err(err) => return fail(err.message()),
    };
    let target = match pick(&messages, args) {
        Ok(target) => target,
        Err(refused) => return fail(refused),
    };
    let mut reactions: Vec<Reaction> = reactions_of(target.metadata.as_ref())
        .into_iter()
        .filter(|r| r.by != AGENT)
        .collect();
    if !emoji.is_empty() {
        reactions.push(Reaction {
            by: AGENT.to_owned(),
            emoji: emoji.to_owned(),
        });
    }
    let stored = serde_json::to_value(&reactions).unwrap_or_default();
    match port
        .store
        .set_message_reactions(port.session, target.id, &stored)
    {
        Ok(true) => {}
        Ok(false) => return fail(foreign(&target.id.to_string())),
        Err(err) => return fail(err.message()),
    }
    if let Some(link) = port.link() {
        link.emit::<MessageReacted>(&MessageReaction {
            session_id: port.session.to_string(),
            message_id: target.id.to_string(),
            reactions: reactions.clone(),
        });
    }
    answer(&json!({
        "success": true,
        "message_id": target.id.to_string(),
        "reactions": reactions,
    }))
}

fn foreign(id: &str) -> String {
    format!("message {id} is not part of this conversation")
}

/// The message `args` names: `message_id` of this conversation, else the
/// latest user message or the `messages_back`-th one before it.
fn pick<'a>(messages: &'a [Message], args: &Value) -> Result<&'a Message, String> {
    if let Some(id) = args.get("message_id").and_then(Value::as_str) {
        let parsed = MessageId::parse(id).ok();
        return messages
            .iter()
            .find(|m| Some(m.id) == parsed)
            .ok_or_else(|| foreign(id));
    }
    let back = args
        .get("messages_back")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let users: Vec<&Message> = messages.iter().filter(|m| m.role == Role::User).collect();
    usize::try_from(back)
        .ok()
        .and_then(|back| users.len().checked_sub(back + 1))
        .map(|at| users[at])
        .ok_or_else(|| {
            format!(
                "there is no user message {back} back (this conversation has {})",
                users.len()
            )
        })
}
