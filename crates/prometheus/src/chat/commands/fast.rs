//! `/fast [normal|fast|auto|cold|ultrafast|status] [--global]` (spec:
//! model-providers, Fast mode service tier): the session's fast mode,
//! persisted to `agent.fast_mode` with `--global`.

use pmt_agent::turn::model::ModelPort;
use pmt_config::schema::FastMode;
use pmt_core::error::{Error, codes};

use super::Ctx;

const USAGE: &str = "usage: /fast [normal|fast|auto|cold|ultrafast|status] [--global]";

/// Runs `/fast`.
pub(super) async fn fast(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let mut words: Vec<&str> = args.split_whitespace().collect();
    let global = words.contains(&"--global");
    words.retain(|w| *w != "--global");
    let agent = &ctx.live.agent;
    let mode = match words.as_slice() {
        [] | ["status"] => return Ok(status(ctx).await),
        [word] => {
            FastMode::parse(word).ok_or_else(|| Error::new(&codes::CLI_USAGE, USAGE.to_owned()))?
        }
        _ => return Err(Error::new(&codes::CLI_USAGE, USAGE.to_owned())),
    };
    if global {
        ctx.scope
            .profile()
            .config_file()
            .set("agent.fast_mode", mode.as_str())?;
    }
    agent.set_fast_mode(Some(mode));
    let scope = if global {
        "this session and new sessions (agent.fast_mode)"
    } else {
        "this session"
    };
    let mut text = format!("fast mode {} for {scope}", mode.as_str());
    if mode != FastMode::Normal && !ctx.live.model.route().await.fast_tier {
        text.push('\n');
        text.push_str(pmt_agent::turn::fast::NO_TIER_NOTICE);
    }
    Ok(text)
}

async fn status(ctx: &Ctx<'_>) -> String {
    let mode = ctx.live.agent.fast_mode(ctx.scope.config());
    let route = ctx.live.model.route().await;
    let tier = if route.fast_tier {
        "the route has a priority tier"
    } else {
        "the route has no priority tier"
    };
    format!(
        "fast mode {} (window {} s); {}/{}: {tier}",
        mode.as_str(),
        ctx.scope.config().agent.fast_auto_secs,
        route.provider,
        route.model
    )
}
