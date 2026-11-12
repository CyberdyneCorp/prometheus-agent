//! `/delegation` (spec: subagent-delegation, Spawn preconditions and
//! refusals; Delegation settings changes respect prompt caching):
//! `pause` and `resume` stop and restart new spawns for the profile
//! (control actions keep working); `router <name>` sets
//! `delegation.router` for new sessions, or for this one too with
//! `--now`, which rebuilds its tools before the next turn and records
//! `session.tools_rebuilt`; `status` (the default) shows the state and
//! this conversation's children.

use pmt_agent::prompt::frozen::Rebuild;
use pmt_core::error::{Error, codes};
use pmt_i18n::t;
use pmt_obs::trajectory::events::Event;
use pmt_obs::trajectory::payload::ToolsRebuilt;

use super::Ctx;
use super::session::{deferred, take_now};
use crate::chat::session::delegation::PAUSE_FILE;

const USAGE: &str = "usage: /delegation [status] | pause | resume | router <name> [--now]";

/// Runs `/delegation <args>`.
pub(super) async fn delegation(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let (args, now) = take_now(args);
    let words: Vec<&str> = args.split_whitespace().collect();
    let pause = ctx.scope.dir().join(PAUSE_FILE);
    let strings = ctx.strings();
    match words.as_slice() {
        [] | ["status"] => Ok(status(ctx, pause.exists())),
        ["pause"] => {
            std::fs::write(&pause, b"").map_err(|e| Error::io(&codes::HOME_IO, &pause, &e))?;
            Ok(t!(strings, "slash.delegation.paused"))
        }
        ["resume"] => {
            match std::fs::remove_file(&pause) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::io(&codes::HOME_IO, &pause, &e)),
            }
            Ok(t!(strings, "slash.delegation.resumed"))
        }
        ["router", name] => router(ctx, name, now).await,
        _ => Err(Error::new(&codes::CLI_USAGE, USAGE)),
    }
}

fn status(ctx: &Ctx<'_>, paused: bool) -> String {
    let strings = ctx.strings();
    let key = if paused {
        "slash.delegation.status_paused"
    } else {
        "slash.delegation.status_spawning"
    };
    let router = ctx.scope.config().delegation.router.clone();
    let mut lines = vec![t!(strings, key, router = router)];
    if let Some(delegator) = &ctx.live.delegation {
        let tree = pmt_delegation::tree::Registry::process().tree(&delegator.conversation());
        for entry in tree {
            let snapshot = entry.snapshot();
            let line = t!(
                strings,
                "slash.delegation.entry",
                id = entry.subagent_id,
                state = entry.state().name(),
                depth = entry.depth,
                goal = pmt_core::subagent_tree::one_line(&entry.goal),
                calls = snapshot.tool_calls
            );
            lines.push(format!("  {line}"));
        }
    }
    lines.join("\n")
}

async fn router(ctx: &Ctx<'_>, name: &str, now: bool) -> Result<String, Error> {
    ctx.scope
        .profile()
        .config_file()
        .set("delegation.router", name)?;
    let strings = ctx.strings();
    if !now {
        let what = t!(strings, "slash.delegation.router_later", name = name);
        return Ok(deferred(&strings, &what));
    }
    let live = ctx.live;
    if let Some(delegator) = &live.delegation {
        delegator.set_router(crate::chat::session::delegation::routers::select(
            name,
            &live.plugins,
            &live.plugin_call(live.surface()),
        ));
    }
    ctx.factory.rebuild_tools(live, &live.selection()).await?;
    live.agent
        .rebuild_prompt(Rebuild::Now("delegation".to_owned()));
    let tools = {
        use pmt_agent::turn::ToolPort as _;
        live.tools
            .definitions()
            .into_iter()
            .map(|t| t.name)
            .collect()
    };
    live.emit(Event::SessionToolsRebuilt(ToolsRebuilt {
        reason: "delegation --now".to_owned(),
        tools,
    }))
    .await;
    Ok(t!(strings, "slash.delegation.router_now", name = name))
}
