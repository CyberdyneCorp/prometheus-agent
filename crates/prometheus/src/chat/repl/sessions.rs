//! Session commands of the line interface that switch the session it
//! holds: `/new`, `/clear`, `/fork`, `/resume` and `/snapshot` (spec: cli,
//! Built-in slash command set). The commands every surface shares are in
//! [`crate::chat::commands`].

use std::sync::Arc;

use pmt_core::error::Error;
use pmt_core::id::{MessageId, SessionId};
use pmt_store::{EndReason, NewMessage, Visibility};

use super::Repl;

impl Repl {
    /// Ends the current session and opens `next` (a new row when `None`).
    pub(super) async fn switch_to(
        &mut self,
        next: Option<SessionId>,
        reason: EndReason,
    ) -> Result<SessionId, Error> {
        let target = self.live().model.target();
        let id = match next {
            Some(id) => id,
            None => self.factory.create_session(None, &target)?,
        };
        self.finish(reason).await;
        let live = self.factory.open(id, &target).await?;
        self.attach(live);
        self.load_registry();
        Ok(id)
    }

    /// `/new`, `/clear`: a fresh session.
    pub(super) async fn new_session(
        &mut self,
        _parent: Option<SessionId>,
        _title: Option<&str>,
    ) -> Result<String, Error> {
        let id = self.switch_to(None, EndReason::NewSession).await?;
        Ok(format!("new session {id}"))
    }

    /// `/fork [title]`: continues in a copy of this session.
    pub(super) async fn fork(&mut self, title: &str) -> Result<String, Error> {
        let parent = self.live().id;
        let store = &self.factory.store;
        let child = self
            .factory
            .create_session(Some(parent), &self.live().model.target())?;
        let copied: Vec<NewMessage> = store
            .messages(parent, Visibility::Active)?
            .into_iter()
            .map(|m| NewMessage {
                id: MessageId::new(),
                role: m.role,
                content: m.content,
                tool_calls: m.tool_calls,
                reasoning: m.reasoning,
                provider_payload: m.provider_payload,
                tool_call_id: m.tool_call_id,
                tool_name: m.tool_name,
                metadata: m.metadata,
            })
            .collect();
        store.append_messages(child, &copied)?;
        if !title.is_empty() {
            store.set_title(child, Some(title), pmt_store::TitleSource::User)?;
        }
        self.switch_to(Some(child), EndReason::NewSession).await?;
        Ok(format!("forked {parent} into {child}"))
    }

    /// `/resume <ref>`: switches to another session and shows its recap.
    pub(super) async fn resume(&mut self, reference: &str) -> Result<String, Error> {
        if reference.is_empty() {
            return crate::chat::commands::session::list(&self.factory);
        }
        let store = &self.factory.store;
        let id = store.resolve_session(reference)?;
        store.check_resumable(id, self.factory.scope.config().sessions.max_resume_messages)?;
        store.reopen_session(id)?;
        self.switch_to(Some(id), EndReason::UserExit).await?;
        self.recap();
        Ok(String::new())
    }

    /// `/snapshot ...`; a restore replaces `state.db`, so the session's
    /// store handles are opened again.
    pub(super) async fn snapshot(&mut self, args: &str) -> Result<String, Error> {
        let text = crate::slash_handlers::snapshot(&self.factory.scope, args)?;
        if args.split_whitespace().next() == Some("restore") {
            self.factory.store = Arc::new(pmt_store::Store::open(&self.factory.scope)?);
            let id = self.live().id;
            let next = self.factory.store.session(id).ok().map(|_| id);
            self.switch_to(next, EndReason::UserExit).await?;
        }
        Ok(text)
    }
}
