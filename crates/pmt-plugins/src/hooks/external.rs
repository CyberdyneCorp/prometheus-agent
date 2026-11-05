//! Hook callbacks that are not plugin instances: shell hooks, gateway hook
//! directories and outbound webhooks (spec: plugin-system, Shell hooks
//! with first-use consent; Gateway hook directories; Outbound webhooks).
//! The embedder implements them; the registry runs them after every
//! plugin callback of the same hook, in the order they were added.

use std::future::Future;
use std::pin::Pin;

use super::Payload;
use crate::host::CallInfo;
use crate::host::hook_world::Reply;

/// The future an external callback returns.
pub type HookFuture<'a> = Pin<Box<dyn Future<Output = Result<Option<Reply>, String>> + Send + 'a>>;

/// One external callback.
pub trait ExternalHook: Send + Sync {
    /// What logs and answers name it by (`shell:<command>`...).
    fn label(&self) -> &str;

    /// Whether it runs for this firing (a tool matcher, a surface): `false`
    /// skips it without a result.
    fn applies(&self, hook: &str, payload: &Payload, call: &CallInfo) -> bool {
        let _ = (hook, payload, call);
        true
    }

    /// Runs it (bounded by its own timeout): `Ok(None)` is no answer, an
    /// error is logged like a failing plugin callback.
    fn call<'a>(
        &'a self,
        hook: &'a str,
        payload: &'a Payload,
        call: &'a CallInfo,
    ) -> HookFuture<'a>;
}
