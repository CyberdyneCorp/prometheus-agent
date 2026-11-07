//! Side questions (spec: agent-turn-loop, Side questions with /btw): one
//! call on the `side_question` auxiliary route (default: the session's
//! main route) over a read-only snapshot of the transcript. Nothing is
//! written to the session, so its history and prompt cache are untouched.

use std::sync::Arc;

use pmt_core::id::SessionId;
use pmt_providers::auxiliary::{self, AuxArgs, AuxCaller, AuxContext, AuxTask};
use pmt_providers::cost::CallContext;
use pmt_providers::fallback::Healthy;
use pmt_providers::request::{Message, Request, Role, SystemBlock};
use pmt_providers::resolve::Trace;

use super::model::{ProviderSetupParts, SwitchableModel};

/// Output cap of one side answer.
const REPLY_TOKENS: u64 = 1_000;

/// The instruction of a side question.
const SYSTEM: &str = "You answer a side question about the conversation below. \
Answer briefly from what the conversation shows; you cannot run tools, and \
your answer is not added to the conversation.";

/// Asks side questions on the session's `side_question` route.
pub struct SideAsker {
    pub(super) parts: Arc<ProviderSetupParts>,
    pub(super) aux: Arc<AuxCaller>,
    pub(super) model: Arc<SwitchableModel>,
    pub(super) session: SessionId,
}

impl SideAsker {
    /// Asks `question` after `transcript`; the answer text.
    ///
    /// # Errors
    ///
    /// Why no answer came (no route, a failed call, the task's timeout).
    pub async fn ask(&self, transcript: Vec<Message>, question: &str) -> Result<String, String> {
        let resolver = self.parts.resolver();
        let main = resolver
            .resolve_target(&self.model.target(), &mut Trace::default())
            .await
            .ok();
        let ctx = AuxContext {
            resolver: &resolver,
            main: main.as_ref(),
            health: &Healthy,
        };
        let route = auxiliary::resolve(AuxTask::SideQuestion, &AuxArgs::default(), &ctx)
            .await
            .map_err(|e| e.message().to_owned())?
            .ok_or_else(|| "no side_question route answers".to_owned())?;
        let mut messages = transcript;
        messages.push(Message::text(Role::User, question));
        let ask = Request {
            system: vec![SystemBlock {
                text: SYSTEM.to_owned(),
                cache_breakpoint: false,
            }],
            messages,
            max_output_tokens: Some(REPLY_TOKENS),
            ..Request::default()
        };
        let context = CallContext::new(self.session, &AuxTask::SideQuestion.purpose());
        let reply = tokio::time::timeout(route.timeout, self.aux.call(&route, &ask, &context))
            .await
            .map_err(|_| format!("no answer within {} s", route.timeout.as_secs()))?
            .map_err(|e| e.message().to_owned())?;
        Ok(reply.text)
    }
}
