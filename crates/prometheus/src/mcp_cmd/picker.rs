//! `prometheus mcp` with no subcommand (spec: plugin-system, mcp CLI): on
//! a terminal, pick a server and an action (test, tools, sign in, remove);
//! elsewhere the server list and the subcommands.

use pmt_core::error::Error;

use crate::context::Context;
use crate::setup_cmd::Wizard;

pub(super) fn run(ctx: &Context<'_>) -> Result<u8, Error> {
    let mut wizard = Wizard::stdio();
    if !wizard.is_tty() || ctx.json {
        let code = super::list(&ctx.scope()?, ctx.json);
        if !ctx.json {
            println!(
                "manage them with: prometheus mcp add|remove|test|tools|login|reauth|catalog|install|status"
            );
        }
        return Ok(code);
    }
    loop {
        let scope = ctx.scope()?;
        let names: Vec<String> = scope.config().mcp.servers.keys().cloned().collect();
        if names.is_empty() {
            wizard.say("no MCP servers are configured (add one with: prometheus mcp add, or browse: prometheus mcp catalog)")?;
            return Ok(0);
        }
        show(&scope, &names, &mut wizard)?;
        let Some(answer) = wizard.ask("server (number or name; Enter to quit):")? else {
            return Ok(0);
        };
        match pick(&names, &answer) {
            Some(name) => act(ctx, &name, &mut wizard)?,
            None => wizard.say(&format!("no server '{answer}'"))?,
        }
    }
}

fn show(
    scope: &pmt_config::ProfileScope,
    names: &[String],
    wizard: &mut Wizard<'_>,
) -> Result<(), Error> {
    for (n, name) in names.iter().enumerate() {
        let server = &scope.config().mcp.servers[name];
        let state = if server.enabled { "" } else { " (disabled)" };
        wizard.say(&format!(
            "{:>3}) {name}  {}{state}",
            n + 1,
            crate::mcp_host::target(server)
        ))?;
    }
    Ok(())
}

/// The server an answer names: its number or its name.
fn pick(names: &[String], answer: &str) -> Option<String> {
    answer
        .parse::<usize>()
        .ok()
        .and_then(|n| n.checked_sub(1))
        .and_then(|i| names.get(i))
        .or_else(|| names.iter().find(|n| *n == answer))
        .cloned()
}

fn act(ctx: &Context<'_>, name: &str, wizard: &mut Wizard<'_>) -> Result<(), Error> {
    let Some(action) = wizard.ask(&format!(
        "{name}: test, tools, login, remove (Enter: back):"
    ))?
    else {
        return Ok(());
    };
    let outcome = match action.as_str() {
        "test" => super::test(&ctx.scope()?, name).map(drop),
        "tools" => super::tools::run(ctx, name).map(drop),
        "login" => super::login::login(ctx, name, None, false).map(drop),
        "remove" if wizard.confirm(&format!("remove MCP server '{name}'?"), false)? => {
            super::remove(ctx, name).map(drop)
        }
        "remove" => Ok(()),
        other => {
            wizard.say(&format!("no action '{other}'"))?;
            Ok(())
        }
    };
    if let Err(err) = outcome {
        wizard.say(err.message())?;
    }
    Ok(())
}
