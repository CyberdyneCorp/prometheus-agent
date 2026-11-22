//! `prometheus plugins run <plugin> [<command> [args...]]` for
//! `platform-adapter` plugins (spec: plugin-system, Plugin CLI
//! subcommands; platform-adapters, Teams meeting summary pipeline): an
//! adapter world has no CLI export, so its descriptor lists the commands
//! (`cli_commands: [{name, description}]`) and the host runs one by
//! starting the plugin with its platform table's options and handing it
//! `{"op": "cli", "name", "args"}` through `send`; the text it answers is
//! printed (exit 0) and its error goes to stderr (exit 1). Each command
//! must also be granted (`plugin.cli_command:<name>`, from the manifest's
//! `cli_commands`). The plugin runs in this process only for the command:
//! what it must hand to the running gateway goes through its profile
//! state.

use std::fmt::Write as _;

use pmt_config::ProfileScope;
use pmt_core::capability::{Capability, PluginRight};
use pmt_core::error::{Error, codes};
use pmt_plugins::host::CallInfo;
use pmt_plugins::load::LoadedPlugin;
use serde_json::{Value, json};

use crate::output::print_raw;
use crate::plugins_host;

/// The commands `plugin`'s descriptor lists: (name, description).
fn commands(plugin: &LoadedPlugin, call: &CallInfo) -> Result<Vec<(String, String)>, Error> {
    let text = plugin.instance.adapter_descriptor(call)?;
    let descriptor: Value = serde_json::from_str(&text).unwrap_or_default();
    Ok(descriptor["cli_commands"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            Some((
                c["name"].as_str()?.to_owned(),
                c["description"].as_str().unwrap_or_default().to_owned(),
            ))
        })
        .collect())
}

/// Runs (or lists) the commands of adapter plugin `plugin`.
///
/// # Errors
///
/// `plugins.not_found` for a command it does not list or is not granted;
/// `plugins.call` when it crashes.
pub(super) fn run(
    scope: &ProfileScope,
    plugin: &LoadedPlugin,
    command: Option<&str>,
    args: &[String],
) -> Result<u8, Error> {
    let id = &plugin.manifest.id;
    let call = CallInfo {
        session_id: None,
        surface: "cli".to_owned(),
        cwd: None,
    };
    let listed = commands(plugin, &call)?;
    let Some(name) = command else {
        let mut text = String::new();
        for (name, description) in &listed {
            let _ = writeln!(text, "{name}  {description}");
        }
        if text.is_empty() {
            let _ = writeln!(text, "plugin '{id}' registers no commands");
        }
        print_raw(&text)?;
        return Ok(0);
    };
    let granted = plugin.grants.current().allows(&Capability::Plugin {
        right: PluginRight::CliCommand,
        target: Some(name.to_owned()),
    });
    if !granted || !listed.iter().any(|(n, _)| n == name) {
        return Err(Error::new(
            &codes::PLUGINS_NOT_FOUND,
            format!("plugin '{id}' has no command '{name}'"),
        )
        .with_hint(format!("list them with `prometheus plugins run {id}`")));
    }
    let platform = plugin.manifest.adapter_platform().unwrap_or_default();
    let config = scope.config().gateway.platform(&platform);
    let options = plugins_host::adapter::start_options(scope, &config).to_string();
    if let Err(why) = plugin.instance.adapter_start(&call, &options)? {
        eprintln!("{id}: {why}");
        return Ok(pmt_core::exit::FAILURE);
    }
    let request = json!({"op": "cli", "name": name, "args": args}).to_string();
    let answer = plugin.instance.adapter_send(&call, &request);
    let _ = plugin.instance.adapter_stop(&call);
    match answer? {
        Ok(text) => {
            print_raw(&if text.ends_with('\n') || text.is_empty() {
                text
            } else {
                format!("{text}\n")
            })?;
            Ok(0)
        }
        Err(why) => {
            eprintln!("{id} {name}: {why}");
            Ok(pmt_core::exit::FAILURE)
        }
    }
}
