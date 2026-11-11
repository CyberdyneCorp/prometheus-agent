//! `/fast [normal|fast|auto|cold|ultrafast|status] [--global]` (spec:
//! model-providers, Fast mode service tier): the session's fast mode,
//! persisted to `agent.fast_mode` with `--global`.

use pmt_agent::turn::model::ModelPort;
use pmt_config::schema::FastMode;
use pmt_core::error::{Error, codes};
use pmt_i18n::t;

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
    let strings = ctx.strings();
    let key = if global {
        "slash.fast.set_global"
    } else {
        "slash.fast.set_session"
    };
    let mut text = t!(strings, key, mode = mode.as_str());
    if mode != FastMode::Normal && !ctx.live.model.route().await.fast_tier {
        text.push('\n');
        text.push_str(strings.get("slash.fast.no_tier"));
    }
    Ok(text)
}

async fn status(ctx: &Ctx<'_>) -> String {
    let mode = ctx.live.agent.fast_mode(ctx.scope.config());
    let route = ctx.live.model.route().await;
    let key = if route.fast_tier {
        "slash.fast.status_tier"
    } else {
        "slash.fast.status_no_tier"
    };
    t!(
        ctx.strings(),
        key,
        mode = mode.as_str(),
        secs = ctx.scope.config().agent.fast_auto_secs,
        route = format!("{}/{}", route.provider, route.model)
    )
}
