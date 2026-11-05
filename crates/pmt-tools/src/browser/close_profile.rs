//! `browser_close_profile` (spec: execution-environments, Real-profile
//! browsing).
//!
//! When the user's browser holds its profile locked, real-profile
//! browsing falls back to a throwaway profile. With
//! `browser.real_profile_autoclose` the agent may end that browser, the
//! same action as `prometheus browser close-profile`, but only through
//! this tool: it is offered only with the setting (its probe), it needs a
//! person (an interactive claim, so cron, API and one-shot runs never get
//! it), and every call asks the user, under every approval mode and YOLO,
//! with nothing remembered; an unattended gate refuses it. It runs in this
//! process, on the host: the terminal sandbox cannot signal processes
//! outside it, and a command line from the agent's terminal is blocked
//! ([`crate::approval::hardline`]).

use std::sync::Arc;

use serde_json::{Value, json};

use super::TOOLSET;
use crate::descriptor::{
    CallContext, HandlerFuture, Owner, RegisteredTool, ResourceClaims, ToolDescriptor, ToolHandler,
};
use crate::dispatch::Escalation;
use crate::environments::{Environments, part};
use crate::probe::FnProbe;
use crate::result::ToolResult;

/// The tool's name.
pub const NAME: &str = "browser_close_profile";
/// The approval rule every call asks under (never remembered).
pub const RULE: &str = "browser:close_profile";

/// `browser_close_profile`, offered with `browser.use_real_profile` and
/// `browser.real_profile_autoclose`.
#[must_use]
pub fn tool(environments: &Arc<Environments>) -> RegisteredTool {
    let probe = Arc::new(FnProbe::new("browser-real-profile-autoclose", |context| {
        let browser = &context.config.browser;
        if browser.use_real_profile && browser.real_profile_autoclose {
            Ok(())
        } else {
            Err("browser.real_profile_autoclose is off".to_owned())
        }
    }));
    RegisteredTool::new(
        descriptor(),
        Owner::Builtin,
        Arc::new(Handler(Arc::clone(environments))),
    )
    .with_probe(probe)
}

struct Handler(Arc<Environments>);

impl ToolHandler for Handler {
    fn call<'a>(&'a self, context: &'a CallContext, _: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let exec = part(&self.0, context, "browser", |env| env.browser.clone())?;
            Ok::<ToolResult, crate::descriptor::ToolFailure>(
                match exec.manager().close_real_profile(&context.session_id).await {
                    Ok(text) => ToolResult::ok(text),
                    Err(why) => ToolResult::error(why),
                },
            )
        })
    }
}

fn descriptor() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        NAME,
        TOOLSET,
        "End the user's own browser (every window it has open) so real-profile browsing can \
         copy its profile. Use it only after a browser result reported real_profile_unavailable \
         because the profile is locked, and say why first: the user is asked to approve every \
         call. This session's browser then restarts on a fresh copy of the profile.",
        json!({"type": "object", "properties": {}, "additionalProperties": false}),
    );
    d.claims = Some(ResourceClaims {
        interactive: true,
        ..ResourceClaims::exclusive(TOOLSET)
    });
    d.approval = Some(Escalation {
        rule_key: RULE.to_owned(),
        reason: "end the user's browser and every window it has open, so its profile can be copied"
            .to_owned(),
        once_only: true,
    });
    d
}
