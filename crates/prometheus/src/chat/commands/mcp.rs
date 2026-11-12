//! `/mcp [status|reload [--now]]` (spec: plugin-system, MCP client
//! servers; tool-system, Tool surface is frozen for the session).
//!
//! `status` lists the profile's servers and their state; `reload` rebuilds
//! the server set from the configuration and starts it: its tools reach
//! sessions started afterwards, or this one at once with `--now` (which
//! rebuilds the session's tool array and so invalidates its prompt cache).

use std::fmt::Write as _;

use pmt_core::error::{Error, codes};

use super::Ctx;

/// Runs `/mcp`.
pub(super) async fn mcp(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let words: Vec<&str> = args.split_whitespace().collect();
    let now = words.contains(&"--now");
    let rest: Vec<&str> = words.into_iter().filter(|w| *w != "--now").collect();
    match rest.as_slice() {
        [] | ["status"] if !now => Ok(status(
            &ctx.strings(),
            &ctx.factory.extensions_scope(ctx.scope),
        )),
        ["reload"] => reload(ctx, now).await,
        _ => Err(Error::new(
            &codes::CLI_USAGE,
            "usage: /mcp [status|reload [--now]]",
        )),
    }
}

fn status(strings: &pmt_i18n::Strings, scope: &pmt_config::ProfileScope) -> String {
    let rows = crate::mcp_host::rows(scope);
    if rows.is_empty() {
        return pmt_i18n::t!(strings, "slash.mcp.none");
    }
    let mut text = String::new();
    for row in rows {
        let _ = write!(text, "{} ({}) {}", row.name, row.transport, row.state);
        if let Some(reason) = &row.reason {
            let _ = write!(text, ": {reason}");
        }
        let tools = pmt_i18n::t!(strings, "slash.mcp.tools", count = row.tools);
        let _ = writeln!(text, ", {tools}");
    }
    text.trim_end().to_owned()
}

async fn reload(ctx: &Ctx<'_>, now: bool) -> Result<String, Error> {
    let scope = ctx.factory.plugins_reload_scope(ctx.scope)?;
    let manager = crate::mcp_host::reload(&scope);
    manager.discover().await;
    if now {
        ctx.factory.apply_tools_now(ctx.live, "mcp").await?;
    }
    let strings = ctx.strings();
    let when = strings.get(if now {
        "slash.mcp.now"
    } else {
        "slash.mcp.next"
    });
    Ok(format!("{}\n{when}", status(&strings, &scope)))
}
