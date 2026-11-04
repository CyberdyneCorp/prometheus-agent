//! The tool checklist of an MCP server (spec: plugin-system, mcp CLI):
//! `prometheus mcp tools <name>`, and after `mcp add` on a terminal.
//!
//! The server is probed for its tools; the checklist starts from what its
//! filter admits now. Selecting every tool removes `tools.include` and
//! `tools.exclude` (every tool, including ones the server adds later);
//! otherwise `tools.include` lists the selected names.

use pmt_config::ProfileScope;
use pmt_core::error::Error;
use pmt_mcp::naming::Filter;

use super::{json, not_found, runtime};
use crate::context::Context;
use crate::setup_cmd::Wizard;

pub(super) fn run(ctx: &Context<'_>, name: &str) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    if !scope.config().mcp.servers.contains_key(name) {
        return Err(not_found(name));
    }
    let Some(tools) = probe(&scope, name)? else {
        return Ok(pmt_core::exit::UNAVAILABLE);
    };
    let mut wizard = Wizard::stdio();
    choose(&scope, name, &tools, &mut wizard)?;
    Ok(0)
}

/// After `mcp add`: probes the server and, on a terminal, offers its
/// checklist.
pub(super) fn probe_and_choose(
    ctx: &Context<'_>,
    name: &str,
    wizard: &mut Wizard<'_>,
) -> Result<(), Error> {
    let scope = ctx.scope()?;
    let Some(tools) = probe(&scope, name)? else {
        return Ok(());
    };
    if wizard.is_tty() && !tools.is_empty() {
        choose(&scope, name, &tools, wizard)?;
    } else {
        println!("change its tools with: prometheus mcp tools {name}");
    }
    Ok(())
}

/// The server's tool names, or `None` (reported) when it does not answer.
fn probe(scope: &ProfileScope, name: &str) -> Result<Option<Vec<String>>, Error> {
    let manager = crate::mcp_host::reload(scope);
    let Some(server) = manager.server(name).cloned() else {
        println!("MCP server '{name}' is disabled; it was not probed");
        return Ok(None);
    };
    match runtime()?.block_on(server.connect()) {
        Ok(()) => {
            let tools: Vec<String> = server.tools().iter().map(|t| t.name.to_string()).collect();
            println!("MCP server '{name}' offers {} tools", tools.len());
            Ok(Some(tools))
        }
        Err(why) => {
            println!("MCP server '{name}' did not answer ({why}); it stays configured");
            Ok(None)
        }
    }
}

fn choose(
    scope: &ProfileScope,
    name: &str,
    tools: &[String],
    wizard: &mut Wizard<'_>,
) -> Result<(), Error> {
    let filter = Filter::of(&scope.config().mcp.servers[name].tools);
    let mut selected: Vec<bool> = tools.iter().map(|t| filter.admits(t)).collect();
    loop {
        for (n, (tool, on)) in tools.iter().zip(&selected).enumerate() {
            let mark = if *on { "x" } else { " " };
            wizard.say(&format!("{:>3}) [{mark}] {tool}", n + 1))?;
        }
        let Some(answer) =
            wizard.ask("toggle a tool (number or name; all; none; Enter when done):")?
        else {
            break;
        };
        match answer.as_str() {
            "all" => selected.iter_mut().for_each(|s| *s = true),
            "none" => selected.iter_mut().for_each(|s| *s = false),
            _ => {
                let index = answer
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| n.checked_sub(1))
                    .or_else(|| tools.iter().position(|t| *t == answer));
                match index.filter(|i| *i < tools.len()) {
                    Some(i) => selected[i] = !selected[i],
                    None => wizard.say(&format!("no tool '{answer}'"))?,
                }
            }
        }
    }
    write(scope, name, tools, &selected)?;
    let count = selected.iter().filter(|s| **s).count();
    wizard.say(&format!(
        "{count} of {} tools of '{name}' enabled; this applies from the next session",
        tools.len()
    ))
}

/// Writes the selection: every tool clears the filter, else
/// `tools.include` names the selected ones.
pub(super) fn write(
    scope: &ProfileScope,
    name: &str,
    tools: &[String],
    selected: &[bool],
) -> Result<(), Error> {
    let file = scope.profile().config_file();
    let key = format!("mcp.servers.{name}.tools");
    if selected.iter().all(|s| *s) {
        let configured = &scope.config().mcp.servers[name].tools;
        if configured.include.is_some() {
            file.unset(&format!("{key}.include"))?;
        }
        if !configured.exclude.is_empty() {
            file.unset(&format!("{key}.exclude"))?;
        }
        return Ok(());
    }
    let chosen: Vec<&String> = tools
        .iter()
        .zip(selected)
        .filter(|(_, s)| **s)
        .map(|(t, _)| t)
        .collect();
    file.set_all(&[(&format!("{key}.include"), &json(&chosen))])
}
