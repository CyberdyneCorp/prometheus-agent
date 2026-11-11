//! `/agents` (alias `/tasks`; spec: subagent-delegation, Agents monitor
//! and live transcripts): what runs for this session, with status,
//! elapsed time, model, tokens and cost: its subagents as a tree grouped
//! by parent (running and recently finished), its `/background` tasks and
//! its background processes, and its goal, loop and heartbeat. Per item:
//!
//! - `/agents stop <id>`: a subagent (`sa-…`; its siblings keep running),
//!   a background task (`bg_…` or `#<n>`) or a process (`proc_…`);
//! - `/agents steer <sa-id> <message>`;
//! - `/agents inspect <id>`: a returned subagent's or a task's turn-by-turn
//!   history;
//! - `/agents tail <sa-id>`: the end of a subagent's live transcript.

use std::fmt::Write as _;

use pmt_core::error::{Error, codes};
use pmt_core::tasks::Registry;
use pmt_delegation::monitor;
use pmt_i18n::t;
use pmt_store::Visibility;
use pmt_tools::environments::Environments;
use pmt_tools::terminal::process::ProcessArgs;
use serde_json::Value;

use super::Ctx;

const USAGE: &str =
    "usage: /agents [stop <id> | steer <sa-id> <message> | inspect <id> | tail <sa-id>]";

pub(super) async fn agents(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let mut words = args.split_whitespace();
    let action = words.next();
    let id = words.next();
    let rest = words.collect::<Vec<_>>().join(" ");
    let conversation = pmt_delegation::host::conversation_of(&ctx.factory.store, ctx.live.id);
    let refused = |r: pmt_delegation::Refusal| r.error();
    match (action, id) {
        (None, _) => Ok(listing(ctx, &conversation).await),
        (Some("stop"), Some(id)) => stop(ctx, &conversation, id).await,
        (Some("steer"), Some(id)) if !rest.is_empty() => {
            monitor::steer(&conversation, id, &rest).map_err(refused)
        }
        (Some("tail"), Some(id)) => monitor::tail(&conversation, id).map_err(refused),
        (Some("inspect"), Some(id)) => inspect(ctx, &conversation, id),
        _ => Err(Error::new(&codes::CLI_USAGE, USAGE)),
    }
}

/// Everything that runs for the session.
async fn listing(ctx: &Ctx<'_>, conversation: &str) -> String {
    let strings = ctx.strings();
    let heading = |key: &str| format!("{}\n", strings.get(key));
    let mut out = String::new();
    let tree = monitor::tree(conversation);
    if !tree.is_empty() {
        out.push_str(&heading("slash.agents.subagents"));
        for line in tree {
            let _ = writeln!(out, "{line}");
        }
    }
    let tasks = Registry::process().of(&ctx.live.id.to_string());
    if !tasks.is_empty() {
        out.push_str(&heading("slash.agents.tasks"));
        for task in tasks {
            let _ = writeln!(
                out,
                "  #{} {} {} {}s {}{} — {}",
                task.number,
                task.id,
                task.state().name(),
                task.elapsed().as_secs(),
                task.model,
                usage(ctx, task.session().as_deref()),
                pmt_core::subagent_tree::one_line(&task.prompt)
            );
        }
    }
    let processes = processes(ctx).await;
    if !processes.is_empty() {
        out.push_str(&heading("slash.agents.processes"));
        out.push_str(&processes);
    }
    // The session's goal, loop and heartbeat (spec: session-automation,
    // Automation status on every surface).
    let scope = ctx.live.config.get();
    let limits = crate::automation::limits(scope.config());
    let rows = crate::automation::rows(&ctx.factory.store, ctx.live.id, &limits);
    if !rows.is_empty() {
        out.push_str(&heading("slash.agents.automation"));
        for row in rows {
            let _ = writeln!(out, "  {row}");
        }
    }
    if out.is_empty() {
        return t!(strings, "slash.agents.nothing");
    }
    out.trim_end().to_owned()
}

/// ` <tokens> tokens $<cost>` of `session`'s model calls.
fn usage(ctx: &Ctx<'_>, session: Option<&str>) -> String {
    let Some(id) = session.and_then(|s| s.parse().ok()) else {
        return String::new();
    };
    let filter = pmt_store::UsageFilter {
        sessions: Some(vec![id]),
        ..pmt_store::UsageFilter::default()
    };
    let Ok(sum) = ctx.factory.store.usage_sum(&filter) else {
        return String::new();
    };
    let t = &sum.tokens;
    let tokens = t.input + t.output + t.cache_read + t.cache_write;
    let tokens = t!(ctx.strings(), "slash.agents.tokens", tokens = tokens);
    format!(" {tokens} ${:.4}", sum.cost_usd)
}

/// The session's background processes, one per line.
async fn processes(ctx: &Ctx<'_>) -> String {
    let Some(terminal) = Environments::process()
        .session(&ctx.live.id.to_string())
        .and_then(|env| env.terminal.clone())
    else {
        return String::new();
    };
    let listed = terminal
        .processes()
        .handle(&process_args("list", None))
        .await;
    let mut out = String::new();
    for p in listed["processes"].as_array().into_iter().flatten() {
        let text = |key: &str| {
            p.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let _ = writeln!(
            out,
            "  {} {} {}s — {}",
            text("process_id"),
            text("state"),
            p.get("started_secs_ago")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            text("command")
        );
    }
    out
}

fn process_args(action: &str, id: Option<&str>) -> ProcessArgs {
    ProcessArgs {
        action: action.to_owned(),
        process_id: id.map(str::to_owned),
        data: None,
        timeout: None,
    }
}

async fn stop(ctx: &Ctx<'_>, conversation: &str, id: &str) -> Result<String, Error> {
    if let Some(task) = Registry::process().find(&ctx.live.id.to_string(), id) {
        task.stop();
        return Ok(t!(
            ctx.strings(),
            "slash.agents.stopping",
            number = task.number,
            id = task.id
        ));
    }
    if id.starts_with("proc_") {
        let Some(terminal) = Environments::process()
            .session(&ctx.live.id.to_string())
            .and_then(|env| env.terminal.clone())
        else {
            return Err(Error::new(&codes::CLI_USAGE, format!("no process '{id}'")));
        };
        let killed = terminal
            .processes()
            .handle(&process_args("kill", Some(id)))
            .await;
        return Ok(killed.to_string());
    }
    monitor::stop(conversation, id).map_err(|r| r.error())
}

/// A returned subagent's (or a task's) history, turn by turn.
fn inspect(ctx: &Ctx<'_>, conversation: &str, id: &str) -> Result<String, Error> {
    let session = match Registry::process().find(&ctx.live.id.to_string(), id) {
        Some(task) => task.session(),
        None => Some(monitor::returned_session(conversation, id).map_err(|r| r.error())?),
    };
    let strings = ctx.strings();
    let Some(session) = session.and_then(|s| s.parse().ok()) else {
        return Ok(t!(strings, "slash.agents.no_session", id = id));
    };
    let messages = ctx.factory.store.messages(session, Visibility::Active)?;
    let mut out = t!(
        strings,
        "slash.agents.inspect_heading",
        id = id,
        session = session
    );
    out.push('\n');
    for m in messages {
        let text = m.content.unwrap_or_default();
        let calls = m
            .tool_calls
            .as_ref()
            .and_then(Value::as_array)
            .map(|calls| {
                calls
                    .iter()
                    .filter_map(|c| c.get("name").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|names| !names.is_empty())
            .map(|names| format!(" {}", t!(strings, "slash.agents.calls", names = names)))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "{}: {}{calls}",
            m.role.as_str(),
            pmt_core::subagent_tree::one_line(&text)
        );
    }
    Ok(out.trim_end().to_owned())
}
