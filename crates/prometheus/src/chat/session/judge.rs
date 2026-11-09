//! The session's goal judge (spec: session-automation, Goal judge loop;
//! Automation configuration): `/goal` verdicts, contract drafts and
//! `/loop --until` checks run on the `goal_judge` auxiliary route, which
//! follows the session's current main route unless `[auxiliary.goal_judge]`
//! pins one. Every failure is an `Err` the caller treats as its fail-open
//! default.

use std::sync::Arc;

use pmt_core::id::SessionId;
use pmt_providers::auxiliary::{self, AuxArgs, AuxCaller, AuxContext, AuxTask};
use pmt_providers::cost::CallContext;
use pmt_providers::fallback::Healthy;
use pmt_providers::request::{Message, Request, Role};
use pmt_providers::resolve::Trace;

use super::model::{ProviderSetupParts, SwitchableModel};

/// Output cap of one verdict or draft.
const REPLY_TOKENS: u64 = 600;

/// Calls on the session's `goal_judge` route.
pub struct RouteJudge {
    pub(super) parts: Arc<ProviderSetupParts>,
    pub(super) aux: Arc<AuxCaller>,
    pub(super) model: Arc<SwitchableModel>,
    pub(super) session: SessionId,
}

impl RouteJudge {
    /// Asks `prompt` as one user message; the reply text.
    ///
    /// # Errors
    ///
    /// Why no reply came (no route, a failed call, the task's timeout).
    pub async fn ask(&self, prompt: &str) -> Result<String, String> {
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
        let route = auxiliary::resolve(AuxTask::GoalJudge, &AuxArgs::default(), &ctx)
            .await
            .map_err(|e| e.message().to_owned())?
            .ok_or_else(|| "no goal_judge route answers".to_owned())?;
        let ask = Request {
            messages: vec![Message::text(Role::User, prompt)],
            max_output_tokens: Some(REPLY_TOKENS),
            ..Request::default()
        };
        let context = CallContext::new(self.session, &AuxTask::GoalJudge.purpose());
        let reply = tokio::time::timeout(route.timeout, self.aux.call(&route, &ask, &context))
            .await
            .map_err(|_| format!("no reply within {} s", route.timeout.as_secs()))?
            .map_err(|e| e.message().to_owned())?;
        Ok(reply.text)
    }
}
