//! The takeover fence of the browser tools (spec: execution-environments,
//! Bot screen takeover lease): while a person holds the profile's bot
//! screen every browser tool is refused with `human_has_control`, and a
//! call that started before a takeover has its result voided. Every call
//! also counts as use of the bot screen (its idle stop): an agent driving
//! a headed browser on the screen is using it even with no viewer.

use std::sync::Arc;

use serde_json::Value;

use crate::descriptor::{
    CallContext, Executor, HandlerFuture, RegisteredTool, ToolFailure, ToolHandler,
};
use crate::environments::{Environments, part};
use pmt_config::screen_lease::{self, ScreenLease};

/// `tool` behind the fence of its session's profile.
#[must_use]
pub fn fenced(mut tool: RegisteredTool, environments: &Arc<Environments>) -> RegisteredTool {
    if let Executor::Handler(inner) = &tool.executor {
        tool.executor = Executor::Handler(Arc::new(Fenced {
            inner: Arc::clone(inner),
            environments: Arc::clone(environments),
        }));
    }
    tool
}

struct Fenced {
    inner: Arc<dyn ToolHandler>,
    environments: Arc<Environments>,
}

fn refused(error: &pmt_core::error::Error) -> ToolFailure {
    ToolFailure::new(screen_lease::FAILURE_KIND, error.message())
}

impl ToolHandler for Fenced {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let exec = part(&self.environments, context, "browser", |env| {
                env.browser.clone()
            })?;
            let profile_dir = exec.manager().profile_dir();
            let lease = ScreenLease::of(profile_dir);
            let epoch = lease.guard(screen_lease::now()).map_err(|e| refused(&e))?;
            screen_lease::mark_screen_used(profile_dir);
            let result = self.inner.call(context, arguments).await;
            // Again after: a long call counts from its end.
            screen_lease::mark_screen_used(profile_dir);
            lease
                .still(epoch, screen_lease::now())
                .map_err(|e| refused(&e))?;
            result
        })
    }
}
