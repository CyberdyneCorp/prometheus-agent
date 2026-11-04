//! The session's approval guardian (spec: tool-system, Smart approvals
//! with a denial breaker): in `smart` mode the gate asks the auxiliary
//! `approval` route about a flagged call before a human. The route follows
//! the session's current main route unless `[auxiliary.approval]` pins one;
//! a missing route, a failure or the task's timeout escalates.

use std::fmt;
use std::sync::Arc;

use pmt_core::id::SessionId;
use pmt_providers::auxiliary::{self, AuxArgs, AuxCaller, AuxContext, AuxTask};
use pmt_providers::cost::CallContext;
use pmt_providers::fallback::Healthy;
use pmt_providers::request::{Message, Request, Role};
use pmt_providers::resolve::Trace;
use pmt_tools::approval::ApprovalRequest;
use pmt_tools::approval::guardian::{self, Guardian, GuardianVerdict};
use pmt_tools::dispatch::SeamFuture;

use super::model::{ProviderSetupParts, SwitchableModel};

/// Output cap of one verdict.
const VERDICT_TOKENS: u64 = 120;

/// The guardian on the session's `approval` auxiliary route.
pub(crate) struct RouteGuardian {
    pub(super) parts: Arc<ProviderSetupParts>,
    pub(super) aux: Arc<AuxCaller>,
    pub(super) model: Arc<SwitchableModel>,
    pub(super) session: SessionId,
}

impl fmt::Debug for RouteGuardian {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RouteGuardian")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl RouteGuardian {
    async fn verdict(&self, request: &ApprovalRequest) -> Result<GuardianVerdict, String> {
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
        let route = auxiliary::resolve(AuxTask::Approval, &AuxArgs::default(), &ctx)
            .await
            .map_err(|e| e.message().to_owned())?
            .ok_or_else(|| "no approval route answers".to_owned())?;
        let ask = Request {
            messages: vec![Message::text(Role::User, guardian::prompt(request))],
            max_output_tokens: Some(VERDICT_TOKENS),
            ..Request::default()
        };
        let context = CallContext::new(self.session, &AuxTask::Approval.purpose());
        let reply = tokio::time::timeout(route.timeout, self.aux.call(&route, &ask, &context))
            .await
            .map_err(|_| format!("no verdict within {} s", route.timeout.as_secs()))?
            .map_err(|e| e.message().to_owned())?;
        Ok(guardian::parse(&reply.text))
    }
}

impl Guardian for RouteGuardian {
    fn judge<'a>(
        &'a self,
        request: &'a ApprovalRequest,
    ) -> SeamFuture<'a, Result<GuardianVerdict, String>> {
        Box::pin(self.verdict(request))
    }
}
