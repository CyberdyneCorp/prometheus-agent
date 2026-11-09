//! Session commands that read or change the running session: `/undo`,
//! `/rewind`, `/compress`, `/model`, `/reasoning`, `/tools`, `/usage` and
//! the input `/retry` sends again (spec: cli, Built-in slash command set;
//! Usage display; Deferred effects with `--now`).

use std::fmt::Write as _;

use pmt_agent::prompt::frozen::Rebuild;
use pmt_agent::turn::TurnInput;
use pmt_core::error::{Error, codes};
use pmt_core::id::{MessageId, SessionId};
use pmt_store::{ListOptions, Role, UsageScope, UsageTotals, Visibility};
use pmt_tools::Selection;

use super::Ctx;
use crate::chat::session::Factory;
use crate::chat::session::model::target_of;

/// The `--now` notice of a deferred change.
pub(crate) fn deferred(what: &str) -> String {
    format!(
        "{what} in your next session. Use /new to start one now, or add --now to apply immediately (invalidates prompt cache)."
    )
}

/// `args` without `--now`, and whether it was there.
pub(crate) fn take_now(args: &str) -> (String, bool) {
    let now = args.split_whitespace().any(|w| w == "--now");
    let kept: Vec<&str> = args.split_whitespace().filter(|w| *w != "--now").collect();
    (kept.join(" "), now)
}

/// `/retry`: the last user message again, with its attachments; what
/// followed it leaves the active transcript. `None`: nothing to retry.
///
/// # Errors
///
/// Store errors.
pub(crate) fn retry(factory: &Factory, id: SessionId) -> Result<Option<TurnInput>, Error> {
    let Some(last) = last_user_input(factory, id)? else {
        return Ok(None);
    };
    factory.store.undo_last_turn(id, None)?;
    Ok(Some(last))
}

/// `/rewind` lists the recent user messages; `/rewind N` goes back to
/// before message N.
pub(super) fn rewind(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let users = user_messages(ctx.factory, ctx.live.id)?;
    if args.is_empty() {
        let mut text = String::from("recent messages (/rewind N goes back to before N):");
        let start = users.len().saturating_sub(10);
        for (n, (_, text_of)) in users.iter().enumerate().skip(start) {
            let first = text_of.lines().next().unwrap_or_default();
            let _ = write!(text, "\n  {}. {first}", n + 1);
        }
        return Ok(text);
    }
    let n: usize = args
        .parse()
        .ok()
        .filter(|n| (1..=users.len()).contains(n))
        .ok_or_else(|| {
            Error::new(
                &codes::CLI_USAGE,
                "usage: /rewind [N] (a number from the list)",
            )
        })?;
    let (target, _) = &users[n - 1];
    let rewound = ctx.factory.store.rewind(ctx.live.id, *target, None, None)?;
    Ok(format!("rewound {} messages", rewound.messages.len()))
}

/// `/compress [--preview] [here [N]] [focus]`.
pub(super) async fn compress(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let manual = pmt_agent::compress::manual::parse(args)?;
    let live = ctx.live;
    let outcome = live.agent.compress(&live.config.get(), &manual).await?;
    let mut said = format!(
        "compress: {:?}, context {} -> {} tokens",
        outcome.status, outcome.tokens_before, outcome.tokens_after
    )
    .to_lowercase();
    if let Some(failure) = &outcome.failure {
        let _ = write!(said, " ({}: {})", failure.kind.as_str(), failure.message);
    }
    Ok(said)
}

/// `/pin [<message-id>|last]`: keeps a message's text in every later
/// compaction summary (spec: context-compression, Pinned messages survive
/// compaction).
pub(super) fn pin(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let max = ctx.live.config.get().config().compression.pinned_max_tokens;
    let pinned = pmt_agent::compress::pins::pin(&ctx.factory.store, ctx.live.id, args, max)?;
    let first = pinned.text.lines().next().unwrap_or_default();
    let preview: String = first.chars().take(60).collect();
    Ok(format!("pinned {}: {preview}", pinned.id))
}

/// `/unpin <message-id>`.
pub(super) fn unpin(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    if args.trim().is_empty() {
        return Err(Error::new(&codes::CLI_USAGE, "usage: /unpin <message-id>"));
    }
    let id = pmt_agent::compress::pins::unpin(&ctx.factory.store, ctx.live.id, args)?;
    Ok(format!("unpinned {id}"))
}

/// `/model [<ref>] [...]`: shows the route or switches it from the next
/// turn (`--global` also saves it).
pub(super) async fn model(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let live = ctx.live;
    let target = live.model.target();
    let models = &ctx.factory.models;
    let resolver = models.resolver();
    let current = resolver
        .resolve_target(&target, &mut pmt_providers::resolve::Trace::default())
        .await?;
    let needs = pmt_providers::switch::Requirements {
        tools: true,
        min_context_window: ctx.scope.config().model.min_context_window,
    };
    let switch = pmt_providers::switch::Context {
        resolver: &resolver,
        catalog: models.catalog(),
        current: &current,
        needs,
    };
    let mut session = live.model.session();
    let text = crate::model_slash::model(ctx.scope, &mut session, &switch, args).await?;
    let once = session.once_pending();
    let (next, reasoning) = session.next_turn();
    if let Some(next) = next {
        let route = resolver
            .resolve_target(&next, &mut pmt_providers::resolve::Trace::default())
            .await?;
        let next = target_of(&route);
        let port = models.port(&next).await?;
        if once {
            // The stored route stays the lasting one, so a resume after
            // the `--once` turn is back on it.
            live.model.switch_once(next, port);
        } else {
            ctx.factory
                .store
                .set_session_route(live.id, &next.provider, next.model.as_deref())?;
            live.model.switch(next, port);
        }
    }
    live.model.set_session(session);
    if reasoning.is_some() {
        live.model.set_reasoning(reasoning);
    }
    Ok(text)
}

/// `/reasoning [...]` on the session's route.
pub(super) async fn reasoning(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let live = ctx.live;
    let models = &ctx.factory.models;
    let route = models
        .resolver()
        .resolve_target(
            &live.model.target(),
            &mut pmt_providers::resolve::Trace::default(),
        )
        .await?;
    let (meta, _) = models.catalog().metadata(&route.provider, &route.model);
    let mut session = live.model.session();
    let text = crate::model_slash::reasoning(ctx.scope, &mut session, &route, &meta, args)?;
    live.model.set_session(session);
    Ok(text)
}

/// `/tools [list] | enable|disable <toolset> [--now]`.
pub(super) async fn tools(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let (args, now) = take_now(args);
    let words: Vec<&str> = args.split_whitespace().collect();
    let registry = &ctx.live.registry();
    let (enable, toolset) = match words.as_slice() {
        [] | ["list"] => {
            return crate::tools_cmd::slash_toolsets(registry, &ctx.live.selection());
        }
        ["enable", name] => (true, *name),
        ["disable", name] => (false, *name),
        _ => {
            return Err(Error::new(
                &codes::CLI_USAGE,
                "usage: /tools [list] | enable|disable <toolset> [--now]",
            ));
        }
    };
    if !registry.toolsets().any(|t| t.name == toolset) {
        return Err(crate::tools_cmd::unknown_toolset(registry, toolset));
    }
    crate::tools_cmd::configure::persist_toolset(ctx.scope, toolset, enable)?;
    let verb = if enable { "enabled" } else { "disabled" };
    if !now {
        return Ok(deferred(&format!("Toolset '{toolset}' is {verb}")));
    }
    apply_tools_now(ctx, toolset, enable).await?;
    Ok(format!(
        "Toolset '{toolset}' {verb} now; the next request uses the new tool set."
    ))
}

/// `/tools enable|disable <toolset> --now`: the same session's next
/// request carries the new tool set; one `prompt.rebuilt` with reason
/// `tools` is recorded.
async fn apply_tools_now(ctx: &Ctx<'_>, toolset: &str, enable: bool) -> Result<(), Error> {
    let live = ctx.live;
    let mut enabled = live.selection().toolsets;
    enabled.retain(|t| t != toolset);
    if enable {
        enabled.push(toolset.to_owned());
    }
    let disabled: Vec<String> = if enable {
        Vec::new()
    } else {
        vec![toolset.to_owned()]
    };
    let opt_ins = live.selection().session_opt_ins;
    let selection = Selection::resolve(&ctx.live.registry(), &enabled, &disabled)?
        .with_session_opt_ins(opt_ins.iter().map(String::as_str));
    ctx.factory.rebuild_tools(live, &selection).await?;
    live.agent.rebuild_prompt(Rebuild::Now("tools".to_owned()));
    Ok(())
}

/// `/usage`: this session and its whole lineage (compression
/// continuations, forks, delegated children), then the budgets.
pub(super) fn usage(ctx: &Ctx<'_>) -> Result<String, Error> {
    let store = &ctx.factory.store;
    let id = ctx.live.id;
    let session = store.usage_totals(UsageScope::Session(id))?;
    let lineage = store.usage_totals(UsageScope::Lineage(id))?;
    let mut text = format!(
        "{}\n{}",
        totals_line("session", &session),
        totals_line("lineage", &lineage)
    );
    let scope = ctx.scope;
    let (budgets, _) =
        crate::slash_handlers::budget(scope, store, id, &scope.config().budgets, "")?;
    if budgets != "no budgets set" {
        let _ = write!(text, "\n{budgets}");
    }
    if let Some(billing) = crate::slash_handlers::billing_lines(store, id)? {
        let _ = write!(text, "\n{billing}");
    }
    Ok(text)
}

fn totals_line(label: &str, totals: &UsageTotals) -> String {
    let t = &totals.tokens;
    format!(
        "{label:<8} calls {}  input {}  output {}  cache read {}  cache write {}  reasoning {}  cost {}",
        totals.calls,
        t.input,
        t.output,
        t.cache_read,
        t.cache_write,
        t.reasoning,
        pmt_obs::usage::format_total_cost(totals)
    )
}

/// The ten most recent sessions (`/resume` without a reference).
///
/// # Errors
///
/// Store errors.
pub(crate) fn list(factory: &Factory) -> Result<String, Error> {
    let sessions = factory.store.list_sessions(&ListOptions {
        limit: Some(10),
        ..ListOptions::default()
    })?;
    let mut text = String::from("recent sessions (/resume <id or title>):");
    for session in sessions {
        let _ = write!(
            text,
            "\n  {}  {}  {}",
            session.id,
            session.last_activity_at.strftime("%Y-%m-%d %H:%M"),
            session.title.as_deref().unwrap_or("(untitled)")
        );
    }
    Ok(text)
}

/// The user messages of `id` (what the user typed), oldest first.
fn user_messages(factory: &Factory, id: SessionId) -> Result<Vec<(MessageId, String)>, Error> {
    Ok(factory
        .store
        .messages(id, Visibility::Active)?
        .into_iter()
        .filter(|m| m.role == Role::User && !authored(m))
        .map(|m| {
            let text = m
                .metadata
                .as_ref()
                .and_then(|meta| meta.get("user_text"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .or(m.content)
                .unwrap_or_default();
            (m.id, text)
        })
        .collect())
}

fn authored(message: &pmt_store::Message) -> bool {
    message
        .metadata
        .as_ref()
        .and_then(|m| m.get(pmt_agent::turn::transcript::AUTHORED))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// The last user message as it was sent: its text and images.
pub(crate) fn last_user_input(
    factory: &Factory,
    id: SessionId,
) -> Result<Option<TurnInput>, Error> {
    let Some((message_id, text)) = user_messages(factory, id)?.pop() else {
        return Ok(None);
    };
    let row = factory
        .store
        .messages(id, Visibility::Active)?
        .into_iter()
        .find(|m| m.id == message_id);
    Ok(Some(TurnInput {
        images: row
            .as_ref()
            .map(pmt_agent::turn::transcript::images_of)
            .unwrap_or_default(),
        ..TurnInput::text(text)
    }))
}
