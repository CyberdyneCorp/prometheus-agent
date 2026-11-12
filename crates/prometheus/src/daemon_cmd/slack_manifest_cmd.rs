//! `prometheus gateway slack-manifest` (spec: platform-adapters, Slack app
//! manifest generator): a Slack app manifest for Socket Mode with the bot
//! scopes and events the adapter needs and one native slash command per
//! gateway-surface command (plus the umbrella `/prometheus`), printed or
//! written to a file.

use std::path::PathBuf;

use clap::Args;
use pmt_core::error::{Error, codes};
use serde_json::{Value, json};

use crate::chat::commands::profile_registry;
use crate::context::Context;
use crate::output::{print_error, print_raw};
use crate::slash::Surface;

/// Bounds of Slack's long description.
const LONG_DESCRIPTION: std::ops::RangeInclusive<usize> = 175..=4000;

/// `prometheus gateway slack-manifest` options.
#[derive(Debug, Args)]
pub struct SlackManifestArgs {
    /// The app's display name.
    #[arg(long, default_value = "Prometheus")]
    name: String,
    /// The app's short description.
    #[arg(long)]
    description: Option<String>,
    /// The app's long description (175-4000 characters).
    #[arg(long, conflicts_with = "long_description_file")]
    long_description: Option<String>,
    /// A file holding the long description.
    #[arg(long, value_name = "PATH")]
    long_description_file: Option<PathBuf>,
    /// Subscribe to the agent messaging events (the app's Messages tab).
    #[arg(long)]
    agent_view: bool,
    /// Leave out the assistant view, its scope and events.
    #[arg(long)]
    no_assistant: bool,
    /// Print only the slash-command array.
    #[arg(long)]
    commands_only: bool,
    /// Write the manifest to PATH (default `<profile>/slack-manifest.json`).
    #[arg(long, value_name = "PATH", num_args = 0..=1)]
    #[allow(clippy::option_option)] // absent, `--write`, `--write PATH`
    write: Option<Option<PathBuf>>,
}

/// One slash command per gateway-surface command, and `/prometheus`.
fn slash_commands(ctx: &Context<'_>) -> Result<Vec<Value>, Error> {
    let scope = ctx.scope()?;
    let registry = profile_registry(&scope);
    let strings = pmt_config::language::strings(scope.dir(), pmt_i18n::Surface::Core);
    let builtins = registry
        .builtins()
        .iter()
        .filter(|c| c.on(Surface::Gateway))
        .map(|c| {
            (
                c.name.to_owned(),
                c.description(&strings).to_owned(),
                c.usage(),
            )
        });
    let externals = registry
        .externals()
        .filter(|e| e.surfaces.contains(&Surface::Gateway))
        .map(|e| {
            (
                e.name.clone(),
                e.description.clone(),
                format!("/{}", e.name),
            )
        });
    let umbrella = std::iter::once((
        "prometheus".to_owned(),
        "Run a command or send a message: /prometheus <command> [args]".to_owned(),
        "/prometheus <command> [args]".to_owned(),
    ));
    Ok(umbrella
        .chain(builtins)
        .chain(externals)
        .map(|(name, description, usage)| {
            let name: String = name.to_lowercase().chars().take(31).collect();
            let hint = usage
                .split_once(' ')
                .map_or("", |(_, args)| args)
                .to_owned();
            let mut command = json!({
                "command": format!("/{name}"),
                "description": description.chars().take(2000).collect::<String>(),
                "should_escape": false,
            });
            if !hint.is_empty() {
                command["usage_hint"] = json!(hint);
            }
            command
        })
        .collect())
}

fn scopes(assistant: bool) -> Vec<&'static str> {
    let mut scopes = vec![
        "app_mentions:read",
        "channels:history",
        "channels:read",
        "chat:write",
        "commands",
        "files:read",
        "files:write",
        "groups:history",
        "groups:read",
        "im:history",
        "im:read",
        "im:write",
        "mpim:history",
        "mpim:read",
        "reactions:read",
        "reactions:write",
        // User groups are the roles `allow_roles` grants.
        "usergroups:read",
        "users:read",
    ];
    if assistant {
        scopes.push("assistant:write");
    }
    scopes
}

fn events(assistant: bool) -> Vec<&'static str> {
    // `app_home_opened` is the start ping (opening the Messages tab), so
    // every manifest subscribes to it, not only the agent view.
    let mut events = vec![
        "app_home_opened",
        "app_mention",
        "message.channels",
        "message.groups",
        "message.im",
        "message.mpim",
        "reaction_added",
    ];
    if assistant {
        events.extend([
            "assistant_thread_started",
            "assistant_thread_context_changed",
        ]);
    }
    events
}

/// The long description, from the flag or the file; refused outside
/// 175-4000 characters.
fn long_description(args: &SlackManifestArgs) -> Result<Option<String>, Error> {
    let text = match (&args.long_description, &args.long_description_file) {
        (Some(text), _) => text.clone(),
        (None, Some(path)) => std::fs::read_to_string(path).map_err(|e| {
            Error::new(
                &codes::CLI_USAGE,
                format!("--long-description-file {}: {e}", path.display()),
            )
        })?,
        (None, None) => return Ok(None),
    };
    let text = text.trim().to_owned();
    let len = text.chars().count();
    if !LONG_DESCRIPTION.contains(&len) {
        return Err(Error::new(
            &codes::CLI_USAGE,
            format!(
                "the long description must be 175 to 4000 characters (Slack's minimum is 175); it has {len}"
            ),
        ));
    }
    Ok(Some(text))
}

fn manifest(args: &SlackManifestArgs, commands: &[Value]) -> Result<Value, Error> {
    let assistant = !args.no_assistant;
    let mut display = json!({"name": args.name});
    if let Some(description) = &args.description {
        display["description"] = json!(description);
    }
    if let Some(long) = long_description(args)? {
        display["long_description"] = json!(long);
    }
    let mut features = json!({
        "bot_user": {"display_name": args.name, "always_online": true},
        "slash_commands": commands,
    });
    if assistant {
        features["assistant_view"] =
            json!({"assistant_description": format!("{} answers in threads.", args.name)});
    }
    if args.agent_view {
        features["app_home"] = json!({"messages_tab_enabled": true,
                                      "messages_tab_read_only_enabled": false});
    }
    Ok(json!({
        "display_information": display,
        "features": features,
        "oauth_config": {"scopes": {"bot": scopes(assistant)}},
        "settings": {
            "event_subscriptions": {"bot_events": events(assistant)},
            "interactivity": {"is_enabled": true},
            "org_deploy_enabled": false,
            "socket_mode_enabled": true,
            "token_rotation_enabled": false,
        },
    }))
}

/// Runs the command; its exit code.
pub(super) fn run(args: &SlackManifestArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let commands = slash_commands(ctx)?;
    let document = if args.commands_only {
        // The array alone (validated description flags are irrelevant).
        Value::Array(commands)
    } else {
        manifest(args, &commands)?
    };
    let text = serde_json::to_string_pretty(&document).expect("a manifest serializes");
    let Some(target) = &args.write else {
        return print_raw(&format!("{text}\n"));
    };
    let path = match target {
        Some(path) => path.clone(),
        None => ctx.scope()?.dir().join("slack-manifest.json"),
    };
    if let Err(e) = std::fs::write(&path, format!("{text}\n")) {
        let err = Error::new(
            &codes::OUTPUT_IO,
            format!("cannot write {}: {e}", path.display()),
        );
        print_error(&err, ctx.json);
        return Ok(pmt_core::exit::CANNOT_CREATE_OUTPUT);
    }
    print_raw(&format!("wrote {}\n", path.display()))
}
