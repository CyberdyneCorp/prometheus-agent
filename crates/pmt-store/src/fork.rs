//! Forking a session (spec: session-storage, Session forking API): a new
//! session with `lineage_kind = "fork"` whose active transcript is a copy
//! of the parent's active messages up to a chosen one, each copy with a
//! new id and an `origin_message_id` back-reference in its metadata; the
//! parent is left unchanged.

use pmt_core::error::{Error, codes};
use pmt_core::id::{MessageId, SessionId};
use serde_json::{Map, Value};

use crate::messages::{Message, NewMessage, Visibility};
use crate::model::{LineageKind, TitleSource};
use crate::sessions::{NewSession, Session};
use crate::store::Store;

/// The metadata key of a copied message's original id.
pub const ORIGIN_MESSAGE_ID: &str = "origin_message_id";

impl Store {
    /// Forks `parent` after the active message `at` (default its last
    /// active message). The fork keeps the parent's working directory,
    /// model and provider and is titled `title`, else `<parent title> #<n>`
    /// when the parent has a title.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.fork_target_invalid` when `at` is not an
    /// active message of `parent`, `store.title_in_use`, store errors.
    pub fn fork_session(
        &self,
        parent: SessionId,
        at: Option<MessageId>,
        title: Option<&str>,
    ) -> Result<Session, Error> {
        let row = self.session(parent)?;
        let messages = self.messages(parent, Visibility::Active)?;
        let keep = match at {
            None => messages.len(),
            Some(at) => {
                messages.iter().position(|m| m.id == at).ok_or_else(|| {
                    Error::new(
                        &codes::STORE_FORK_TARGET_INVALID,
                        format!("{at} is not an active message of session {parent}"),
                    )
                })? + 1
            }
        };
        let mut new = NewSession::child(row.source, parent, LineageKind::Fork);
        new.source_tag = row.source_tag;
        new.model = row.model;
        new.provider = row.provider;
        new.cwd = row.cwd;
        new.git_repo_root = row.git_repo_root;
        new.git_branch = row.git_branch;
        if let Some(title) = title {
            new.title = crate::titles::sanitize(title)?;
            new.title_source = TitleSource::User;
        }
        let fork = self.create_session(&new)?;
        let copies: Vec<NewMessage> = messages[..keep].iter().map(copy).collect();
        if !copies.is_empty() {
            self.append_messages(fork.id, &copies)?;
        }
        Ok(fork)
    }
}

fn copy(message: &Message) -> NewMessage {
    let mut metadata = match &message.metadata {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    };
    metadata.insert(
        ORIGIN_MESSAGE_ID.to_owned(),
        Value::String(message.id.to_string()),
    );
    NewMessage {
        id: MessageId::new(),
        role: message.role,
        content: message.content.clone(),
        tool_calls: message.tool_calls.clone(),
        reasoning: message.reasoning.clone(),
        provider_payload: message.provider_payload.clone(),
        tool_call_id: message.tool_call_id.clone(),
        tool_name: message.tool_name.clone(),
        metadata: Some(Value::Object(metadata)),
    }
}
