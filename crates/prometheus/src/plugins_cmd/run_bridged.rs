//! `prometheus plugins run <plugin> <name> [args...]` for python plugins
//! (spec: python-bridge, Plugin CLI commands through the bridge): a
//! one-shot sidecar for the invoking profile builds the plugin's argparse
//! tree, gets argv, streams stdout and stderr here as the command writes
//! them, reads stdin lines from an interactive terminal, and exits with
//! the handler's code (a `SystemExit` code, 0 for `None`, 2 for an
//! argparse error, 70 for an uncaught exception, whose traceback goes to
//! the plugin log).

use std::fmt::Write as _;
use std::io::{BufRead as _, Write as _};
use std::sync::Arc;
use std::time::Duration;

use pmt_config::ProfileScope;
use pmt_core::capability::{Capability, PluginRight};
use pmt_core::error::{Error, codes};
use pmt_plugins::host::Console;
use pmt_plugins::load::BridgedPlugin;
use pmt_pybridge::host::{LineSource, Terminal};
use pmt_pybridge::sidecar::CallError;
use serde_json::json;

use crate::output::print_raw;
use crate::plugins_host::bridge;

/// No bound beyond the user's patience: the command runs until it ends.
const NO_BOUND: Duration = Duration::from_hours(24);

/// The enabled, approved python plugin `id`, if there is one.
pub(super) fn find(scope: &ProfileScope, id: &str) -> Option<BridgedPlugin> {
    let version = crate::plugins_host::running_version();
    let blocked = crate::plugins_host::catalog::kill_list(scope);
    let cwd = std::env::current_dir().unwrap_or_else(|_| scope.dir().to_path_buf());
    let inputs = crate::plugins_host::inputs(scope, &cwd, &version, &blocked);
    let surveyed = pmt_plugins::load::survey(&inputs);
    pmt_plugins::load::bridged(&surveyed, &inputs)
        .into_iter()
        .find(|p| p.manifest.id == id)
}

/// Lines from this process's stdin.
struct Stdin;

impl LineSource for Stdin {
    fn read_line(&self) -> Option<String> {
        let mut line = String::new();
        match std::io::stdin().lock().read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line),
        }
    }
}

/// This process's stdout and stderr, flushed per write.
struct Streams;

impl Console for Streams {
    fn stdout(&self, bytes: &[u8]) {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(bytes);
        let _ = out.flush();
    }

    fn stderr(&self, bytes: &[u8]) {
        let _ = std::io::stderr().lock().write_all(bytes);
    }
}

/// The profile's active external memory provider. Until the external
/// memory-provider contract adds `memory.provider` (memory spec, task 44.9)
/// every profile runs the built-in store, so no memory provider is active.
fn active_memory_provider(_scope: &ProfileScope) -> &'static str {
    "builtin"
}

/// Runs (or lists) a python plugin's CLI commands.
///
/// # Errors
///
/// The plugin's preparation failure; `plugins.not_found` for an unknown
/// command; `plugins.capability_denied` for an ungranted one.
pub(super) fn run(
    scope: &ProfileScope,
    plugin: &BridgedPlugin,
    name: Option<&str>,
    args: &[String],
) -> Result<u8, Error> {
    let id = plugin.manifest.id.clone();
    let prepared = bridge::prepare(scope, plugin)?;
    let interactive = crate::prompt::stdin_is_tty();
    prepared.host.attach(Terminal {
        output: Arc::new(Streams),
        input: interactive.then(|| Arc::new(Stdin) as Arc<dyn LineSource>),
    });
    let supervisor = prepared.supervisor();
    let registrations = supervisor.start_now()?;
    let active = active_memory_provider(scope);
    let available =
        |c: &&pmt_pybridge::registrations::CliCommand| !c.memory_provider || id == active;
    let Some(name) = name else {
        let mut text = String::new();
        for command in registrations.cli_commands.iter().filter(available) {
            let _ = writeln!(text, "{}  {}", command.name, command.help);
        }
        if text.is_empty() {
            let _ = writeln!(text, "plugin '{id}' registers no commands");
        }
        supervisor.stop();
        return print_raw(&text);
    };
    if let Some(command) = registrations
        .cli_commands
        .iter()
        .find(|c| c.name == name && !available(c))
    {
        supervisor.stop();
        return Err(Error::new(
            &codes::PLUGINS_NOT_FOUND,
            format!(
                "command '{}' of memory provider '{id}' is available only while '{id}' is the active memory.provider (now '{active}')",
                command.name
            ),
        ));
    }
    if !registrations.cli_commands.iter().any(|c| c.name == name) {
        supervisor.stop();
        return Err(Error::new(
            &codes::PLUGINS_NOT_FOUND,
            format!("plugin '{id}' has no command '{name}'"),
        )
        .with_hint(format!("list them with `prometheus plugins run {id}`")));
    }
    let right = Capability::Plugin {
        right: PluginRight::CliCommand,
        target: Some(name.to_owned()),
    };
    if !plugin.grants.current().allows(&right) {
        supervisor.stop();
        return Err(Error::new(
            &codes::PLUGINS_CAPABILITY_DENIED,
            format!("capability_denied: cli_commands {name}; plugin '{id}' may not run it"),
        ));
    }
    let params = json!({"kind": "cli", "name": name, "argv": args, "context": {"surface": "cli"}});
    let outcome = supervisor.call("command.invoke", &params, NO_BOUND);
    supervisor.stop();
    match outcome {
        Ok(answer) => Ok(u8::try_from(answer["code"].as_u64().unwrap_or(1)).unwrap_or(1)),
        Err(CallError::Remote { message, .. }) => {
            eprintln!("{id} {name}: {message}");
            Ok(pmt_core::exit::INTERNAL)
        }
        Err(err) => {
            eprintln!("plugin '{id}' crashed: {}", err.message());
            Ok(pmt_core::exit::INTERNAL)
        }
    }
}
