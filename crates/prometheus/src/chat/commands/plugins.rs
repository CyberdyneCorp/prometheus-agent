//! `/plugins [list|reload [<id>] [--now]]` and the commands plugins
//! register (spec: plugin-system, plugins CLI; Hot reload respects the
//! prompt-cache invariant; Plugin slash commands).

use std::fmt::Write as _;

use pmt_core::error::{Error, codes};
use pmt_plugins::load::Loaded;

use super::Ctx;
use crate::chat::session::Live;
use crate::plugins_host::reload::{check_target, report};
use crate::slash::{self, Registry};

/// Runs `/plugins`.
pub(super) async fn plugins(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let words: Vec<&str> = args.split_whitespace().collect();
    let now = words.contains(&"--now");
    let rest: Vec<&str> = words.into_iter().filter(|w| *w != "--now").collect();
    match rest.as_slice() {
        [] | ["list"] if !now => Ok(listing(&ctx.live.plugins.current())),
        ["reload"] => reload(ctx, None, now).await,
        ["reload", id] => reload(ctx, Some(id), now).await,
        _ => Err(Error::new(
            &codes::CLI_USAGE,
            "usage: /plugins [list|reload [<id>] [--now]]",
        )),
    }
}

/// One line per plugin: id, version, kind, state and load error.
fn listing(loaded: &Loaded) -> String {
    if loaded.statuses.is_empty() {
        return "no plugins".to_owned();
    }
    let mut text = String::new();
    for status in &loaded.statuses {
        let _ = write!(
            text,
            "{} {} {} {}",
            status.id,
            status.version.as_deref().unwrap_or("-"),
            status.kind.map_or("-", |k| k.as_str()),
            status.state.as_str()
        );
        if let Some(problem) = &status.problem {
            let _ = write!(text, " ({problem})");
        }
        text.push('\n');
    }
    text.trim_end().to_owned()
}

async fn reload(ctx: &Ctx<'_>, id: Option<&str>, now: bool) -> Result<String, Error> {
    let scope = ctx.factory.plugins_reload_scope(ctx.scope)?;
    let plugins = crate::plugins_host::reload(&scope, &ctx.factory.options.cwd);
    let loaded = plugins.current();
    check_target(&loaded, id)?;
    if now {
        ctx.factory.apply_plugins_now(ctx.live).await?;
    }
    Ok(report(&loaded, id, now))
}

/// Registers the slash commands of `live`'s current plugin set; a name a
/// built-in already has is refused (logged with its owner) and the
/// built-in keeps it.
pub(crate) fn register(registry: &mut Registry, live: &Live) {
    for command in &live.plugins.current().contributions.commands {
        let declaration = slash::Declaration {
            name: command.name.clone(),
            description: format!("{} (plugin {})", command.description, command.plugin),
            surfaces: command.surfaces.clone(),
            ..slash::Declaration::default()
        };
        if let Err(err) = registry.register(&declaration, slash::Source::Plugin) {
            tracing::warn!(plugin = %command.plugin, "{}", err.message());
        }
    }
}

/// Runs the plugin command of `line`, typed on `surface` (`cli`, `tui`,
/// `gateway`...), on `live`'s session: the text to show, or why it failed.
pub(crate) async fn run_line(live: &Live, typed: &str, surface: &str) -> Result<String, String> {
    let (name, args) = slash::split_line(typed);
    let Some(command) = live.plugins.current().contributions.command(&name).cloned() else {
        return Err(format!("Unknown command: /{name}"));
    };
    let call = live.plugin_call(surface);
    let outcome = pmt_core::spawn::blocking(move || command.run(&call, &args))
        .await
        .map_err(|e| e.to_string())?;
    match outcome {
        Ok(answer) => answer,
        Err(err) => Err(err.message().to_owned()),
    }
}
