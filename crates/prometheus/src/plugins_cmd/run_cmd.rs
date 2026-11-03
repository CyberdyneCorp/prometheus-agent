//! `prometheus plugins run <plugin_id> [<command> [args...]]` (spec:
//! plugin-system, Plugin CLI subcommands): a command a plugin registered
//! under its `cli_commands` grant, run with argv, the invoking profile and
//! the terminal's size and TTY flag. What the plugin writes to its
//! `console` (hook world 1.3) is streamed to stdout and stderr as it is
//! written, the output it returns follows, and its exit code is the
//! command's (70 when the plugin crashes). Without a command, the plugin's commands are
//! listed; `prometheus help` lists every enabled plugin's commands under
//! `plugin commands`.

use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_plugins::host::surfaces_world;
use pmt_plugins::host::{CallInfo, Console};
use pmt_plugins::manifest::Manifest;

use crate::context::Context;
use crate::output::print_raw;
use crate::plugins_host;

/// Runs (or lists) a plugin's commands.
///
/// # Errors
///
/// `plugins.not_found` for a plugin that is not loaded or a command it
/// does not register; profile errors.
pub(super) fn run(
    ctx: &Context<'_>,
    id: &str,
    command: Option<&str>,
    args: &[String],
) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| scope.dir().to_path_buf());
    let plugins = plugins_host::for_session(&scope, &cwd);
    let loaded = plugins.current();
    if !loaded.plugins.iter().any(|p| p.manifest.id == id) {
        let why = loaded
            .statuses
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| s.problem.clone())
            .unwrap_or_else(|| "it is not installed and enabled".to_owned());
        return Err(Error::new(
            &codes::PLUGINS_NOT_FOUND,
            format!("plugin '{id}' is not loaded: {why}"),
        ));
    }
    let entries: Vec<_> = loaded
        .surfaces
        .cli
        .iter()
        .filter(|c| c.plugin == id)
        .collect();
    let Some(name) = command else {
        let mut text = String::new();
        for entry in &entries {
            let _ = writeln!(text, "{}  {}", entry.name, entry.description);
        }
        if text.is_empty() {
            let _ = writeln!(text, "plugin '{id}' registers no commands");
        }
        print_raw(&text)?;
        return Ok(0);
    };
    let entry = entries.iter().find(|c| c.name == name).ok_or_else(|| {
        Error::new(
            &codes::PLUGINS_NOT_FOUND,
            format!("plugin '{id}' has no command '{name}'"),
        )
        .with_hint(format!("list them with `prometheus plugins run {id}`"))
    })?;
    let call = CallInfo {
        session_id: None,
        surface: "cli".to_owned(),
        cwd: Some(cwd.display().to_string()),
    };
    let (columns, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let terminal = surfaces_world::Terminal {
        columns,
        rows,
        tty: crate::prompt::stdin_is_tty(),
    };
    match entry
        .instance
        .run_cli(&call, name, args, terminal, Arc::new(Terminal))
    {
        Ok(Ok(output)) => {
            let _ = std::io::stdout().write_all(&output.stdout);
            let _ = std::io::stderr().write_all(&output.stderr);
            Ok(output.code)
        }
        Ok(Err(message)) => {
            eprintln!("{id} {name}: {message}");
            Ok(pmt_core::exit::FAILURE)
        }
        Err(err) => {
            eprintln!("plugin '{id}' crashed: {}", err.message());
            Ok(pmt_core::exit::INTERNAL)
        }
    }
}

/// The process's own stdout and stderr, flushed after every write so the
/// output streams.
struct Terminal;

impl Console for Terminal {
    fn stdout(&self, bytes: &[u8]) {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(bytes);
        let _ = out.flush();
    }

    fn stderr(&self, bytes: &[u8]) {
        let _ = std::io::stderr().lock().write_all(bytes);
    }
}

/// The `plugin commands` section of `prometheus help`: the
/// `cli_commands` every enabled plugin of the default profile declares
/// (read from manifests; nothing is instantiated). `None` when there are
/// none or the home cannot be read.
#[must_use]
pub fn help_section(scope: &ProfileScope) -> Option<String> {
    let mut text = String::new();
    for id in &scope.config().plugins.enabled {
        let dir = scope.dir().join("plugins").join(id);
        let Ok(manifest) = Manifest::read(&dir, &plugins_host::running_version()) else {
            continue;
        };
        for name in &manifest.capabilities.cli_commands {
            let _ = writeln!(text, "  prometheus plugins run {id} {name}");
        }
    }
    (!text.is_empty()).then(|| format!("\nplugin commands:\n{text}"))
}
