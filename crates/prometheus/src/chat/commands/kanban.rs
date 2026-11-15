//! `/kanban <verb> ...` on every chat surface (spec: kanban, /kanban slash
//! command): the arguments of `prometheus kanban`, parsed by the same
//! definitions and run by the same verbs with their output captured. It
//! runs at once, alongside a running turn (`busy_policy = immediate`), on
//! a blocking thread so the turn is not held. `/kanban create` from a
//! messaging chat (without `--json`) subscribes that chat to the new task
//! in `notify+wake` mode; output above 3800 characters on messaging
//! platforms is cut with a pointer to the terminal.

use std::cell::RefCell;

use clap::Parser;
use pmt_core::error::{Error, codes};
use pmt_kanban::notify::{CHAT_TYPES, DeliveryMode, NewSubscription};
use pmt_store::Origin;

use super::Ctx;
use crate::kanban_cmd::{KanbanArgs, KanbanCommand};

/// Characters a messaging reply keeps.
const MESSAGING_LIMIT: usize = 3800;

/// `/kanban` as clap sees it.
#[derive(Debug, Parser)]
#[command(name = "/kanban", no_binary_name = true, disable_help_flag = true)]
struct Slash {
    #[command(flatten)]
    args: KanbanArgs,
}

/// The chat a `/kanban create` from a messaging conversation reports
/// back to, woken in `notify+wake` mode.
fn origin(ctx: &Ctx<'_>) -> Option<NewSubscription> {
    let store = &ctx.factory.store;
    let Origin::Gateway {
        platform,
        chat,
        thread,
        ..
    } = store.session(ctx.live.id).ok()?.origin
    else {
        return None;
    };
    // A conversation key is `<profile>:<platform>[@scope]:<chat type>:...`.
    let chat_type = store
        .routed_session(ctx.live.id)
        .ok()
        .flatten()
        .and_then(|r| r.route_key.split(':').nth(2).map(str::to_owned))
        .filter(|kind| CHAT_TYPES.contains(&kind.as_str()));
    let mut sub = NewSubscription::notify(
        &platform,
        &chat,
        thread.as_deref(),
        ctx.scope.name().as_str(),
    );
    sub.mode = DeliveryMode::NotifyWake;
    sub.chat_type = chat_type;
    sub.session_id = Some(ctx.live.id.to_string());
    Some(sub)
}

fn usage(message: impl Into<String>) -> Error {
    Error::new(&codes::CLI_USAGE, message)
}

/// Cuts `text` to the messaging limit with the pointer to the terminal.
#[must_use]
fn fit(text: String, limit: Option<usize>, verb: &str) -> String {
    let Some(limit) = limit.map(|l| l.min(MESSAGING_LIMIT)) else {
        return text;
    };
    if text.chars().count() <= limit {
        return text;
    }
    let more = format!("… (truncated; run prometheus kanban {verb} in a terminal for full output)");
    let keep = limit.saturating_sub(more.chars().count() + 1);
    let mut cut: String = text.chars().take(keep).collect();
    cut.push('\n');
    cut.push_str(&more);
    cut
}

/// `/kanban ...`.
///
/// # Errors
///
/// Usage errors and the verb's own errors.
pub(super) async fn kanban(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let mut words = super::cron::words(args)?;
    let json = words.iter().any(|w| w == "--json");
    words.retain(|w| w != "--json");
    let verb = words.first().cloned().unwrap_or_else(|| "list".to_owned());
    if words.is_empty() {
        words.push("list".to_owned());
    }
    let parsed = Slash::try_parse_from(&words).map_err(|e| usage(e.to_string()))?;
    let args = parsed.args;
    if args.ticket.is_some() || matches!(args.command, KanbanCommand::Work { .. }) {
        return Err(usage(
            "/kanban does not take --ticket or work: they are for workers",
        ));
    }
    let origin = (!json).then(|| origin(ctx)).flatten();
    let scope = ctx.scope.clone();
    let launch = ctx.factory.launch().clone();
    let cwd = ctx.factory.options.cwd.clone();
    let run = move || -> Result<String, Error> {
        let home = scope.home().to_path_buf();
        let profile = scope.name().to_string();
        let context = crate::context::Context {
            home_flag: Some(&home),
            launch: &launch,
            profile: Some(&profile),
            set: &[],
            json,
        };
        let capture = Some(RefCell::new(String::new()));
        let mut env = crate::kanban_cmd::open(&context, args.board.as_deref(), cwd, capture)?;
        if matches!(args.command, KanbanCommand::Create(_)) {
            env.origin = origin;
        }
        let result = crate::kanban_cmd::dispatch_verb(&env, &args);
        env.hooks.flush(&env.scope);
        let out = env
            .capture
            .take()
            .map(RefCell::into_inner)
            .unwrap_or_default();
        result.map(|_| out.trim_end().to_owned())
    };
    let text = pmt_core::spawn::blocking(run)
        .await
        .map_err(|e| Error::new(&codes::OUTPUT_IO, format!("/kanban stopped: {e}")))??;
    Ok(fit(text, ctx.message_limit, &verb))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_messaging_output_is_cut_with_a_pointer_to_the_terminal() {
        let long = "x".repeat(5000);
        let cut = fit(long.clone(), Some(4096), "list");
        assert_eq!(cut.chars().count(), MESSAGING_LIMIT);
        assert!(
            cut.ends_with(
                "… (truncated; run prometheus kanban list in a terminal for full output)"
            )
        );
        assert_eq!(
            fit(long.clone(), None, "list"),
            long,
            "terminals get everything"
        );
    }
}
