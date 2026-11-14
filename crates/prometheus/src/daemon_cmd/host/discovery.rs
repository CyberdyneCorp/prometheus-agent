//! The daemon side of the discovery methods (spec: tui, Discovery RPCs
//! preserve the prompt cache; Composer and path completion; Profile scope
//! on every request): the session's command catalog, path completion
//! through the session's own file backend, its model options and one
//! configuration value. All of them read: none rebuilds the session's
//! system prompt, swaps its toolset or reloads its memory snapshot.

use pmt_core::error::Error;
use pmt_rpc::contracts::{
    BannerInfo, ClientKind, CommandCatalog, CommandEntry, CommandSource, Completion, Completions,
    ConfigValue, Disposition, FrozenPrompt, SubcommandEntry,
};
use pmt_tools::environments::Environments;
use pmt_tools::files::backend::Kind;

use super::commands::CLIENT_COMMANDS;
use super::{DaemonHost, Profile, session_id};
use crate::chat::commands;
use crate::slash::{Registry, Source, Surface};

/// At most this many path completions are listed.
const MAX_PATHS: usize = 50;

/// The slash surface a client kind runs commands on.
fn surface(client: ClientKind) -> Surface {
    match client {
        ClientKind::Cli | ClientKind::Eval => Surface::Cli,
        ClientKind::Tui => Surface::Tui,
        ClientKind::Desktop => Surface::Desktop,
        ClientKind::Dashboard => Surface::Dashboard,
        ClientKind::Acp => Surface::Acp,
    }
}

/// Where `/name` (run on `surfaces`) stands on `surface`; the reason of
/// an unavailable command is authored here once and shown verbatim by
/// every client.
fn disposition(
    name: &str,
    surfaces: &[Surface],
    surface: Surface,
) -> (Disposition, Option<String>) {
    if surfaces.contains(&surface) {
        return (Disposition::Exec, None);
    }
    let terminal = |s: &Surface| matches!(s, Surface::Cli | Surface::Tui);
    let reason = if surfaces.iter().any(terminal) && !terminal(&surface) {
        format!("/{name} is only available in the terminal interface.")
    } else {
        "not available on this surface".to_owned()
    };
    (Disposition::Unavailable, Some(reason))
}

fn subcommand_entries(command: &str, surface: Surface) -> Vec<SubcommandEntry> {
    crate::slash::subcommands(command)
        .iter()
        .map(|sub| {
            let (disposition, reason) =
                disposition(&format!("{command} {}", sub.name), sub.surfaces, surface);
            SubcommandEntry {
                name: sub.name.to_owned(),
                disposition,
                reason,
            }
        })
        .collect()
}

/// Every command of `registry` with its disposition on `surface`.
pub(super) fn catalog(
    registry: &Registry,
    surface: Surface,
    strings: &pmt_i18n::Strings,
) -> CommandCatalog {
    let mut commands: Vec<CommandEntry> = registry
        .builtins()
        .iter()
        .map(|command| {
            let (disposition, reason) = disposition(command.name, command.surfaces, surface);
            let client = CLIENT_COMMANDS.contains(&command.name);
            CommandEntry {
                name: command.name.to_owned(),
                aliases: command.aliases.iter().map(|a| (*a).to_owned()).collect(),
                description: command.description(strings).to_owned(),
                args: (!command.args.is_empty()).then(|| command.usage()),
                source: if client {
                    CommandSource::Client
                } else {
                    CommandSource::Builtin
                },
                disposition,
                reason,
                subcommands: subcommand_entries(command.name, surface),
            }
        })
        .collect();
    commands.extend(registry.externals().map(|external| {
        let (disposition, reason) = disposition(&external.name, &external.surfaces, surface);
        CommandEntry {
            name: external.name.clone(),
            aliases: external.aliases.clone(),
            description: external.description.clone(),
            args: None,
            source: match external.source {
                Source::Quick => CommandSource::Quick,
                Source::Skill | Source::Plugin => CommandSource::Skill,
            },
            disposition,
            reason,
            subcommands: Vec::new(),
        }
    }));
    commands.sort_by(|a, b| a.name.cmp(&b.name));
    CommandCatalog { commands }
}

/// Splits a typed path into the directory to list and the partial name.
fn split_path(prefix: &str) -> (&str, &str) {
    match prefix.rfind('/') {
        Some(at) => (&prefix[..=at], &prefix[at + 1..]),
        None => ("", prefix),
    }
}

impl DaemonHost {
    pub(super) async fn commands_of(
        &self,
        profile: &Profile,
        session: &str,
        client: ClientKind,
    ) -> Result<CommandCatalog, Error> {
        let id = session_id(session)?;
        let loaded = profile.loaded.lock().await.get(&id).cloned();
        let scope = profile.scope();
        // A loaded session lists its own registry (with its skills); an
        // unloaded one the profile's, without loading it.
        let registry = match &loaded {
            Some(session) => commands::registry(&scope, &session.live),
            None => commands::profile_registry(&scope),
        };
        let strings = pmt_config::language::strings(scope.dir(), pmt_i18n::Surface::Core);
        Ok(catalog(&registry, surface(client), &strings))
    }

    /// Entries of a typed path, listed by the session's file backend (its
    /// working directory, its capability checks, its filesystem); nothing
    /// when the session has no environment or the backend cannot list.
    pub(super) fn paths_of(session: &str, prefix: &str) -> Completions {
        let Some(files) = Environments::process()
            .session(session)
            .and_then(|env| env.files.clone())
        else {
            return Completions::default();
        };
        let (dir, partial) = split_path(prefix);
        let listed = files.list_dir(if dir.is_empty() { "." } else { dir });
        let Ok(entries) = listed else {
            return Completions::default();
        };
        let items = entries
            .into_iter()
            .filter(|e| e.name.starts_with(partial))
            .filter(|e| partial.starts_with('.') || !e.name.starts_with('.'))
            .take(MAX_PATHS)
            .map(|e| {
                let is_dir = e.kind == Kind::Dir;
                let slash = if is_dir { "/" } else { "" };
                Completion {
                    text: format!("{dir}{}{slash}", e.name),
                    description: None,
                    dir: is_dir,
                }
            })
            .collect();
        Completions { items }
    }

    /// What the TUI's banner lists: the loaded session's tools and
    /// skills, and the prompt it froze (read from the store, never
    /// rebuilt). MCP servers join when the MCP client lands (group 27).
    pub(super) async fn banner_of(profile: &Profile, session: &str) -> Result<BannerInfo, Error> {
        use pmt_agent::turn::tools::ToolPort as _;
        let id = session_id(session)?;
        let system_prompt = profile
            .store
            .session_prompt(id)?
            .map(|prompt| FrozenPrompt {
                sha256: prompt.sha256,
                text: prompt
                    .tiers
                    .into_iter()
                    .map(|tier| tier.text)
                    .collect::<Vec<_>>()
                    .join("\n\n"),
            });
        let loaded = profile.loaded.lock().await.get(&id).cloned();
        let Some(session) = loaded else {
            return Ok(BannerInfo {
                system_prompt,
                ..BannerInfo::default()
            });
        };
        let live = &session.live;
        let tools = live
            .tools
            .definitions()
            .into_iter()
            .map(|t| t.name)
            .collect();
        let (catalog, config) = live.skills.catalog();
        let skills = live
            .skills
            .visible(&catalog, &config)
            .into_iter()
            .map(|skill| skill.name.clone())
            .collect();
        Ok(BannerInfo {
            tools,
            skills,
            system_prompt,
            mcp_servers: Vec::new(),
            degraded: live.downgrades.banner_marker(),
        })
    }

    pub(super) fn config_value(profile: &Profile, key: &str) -> Result<ConfigValue, Error> {
        crate::config_cmd::known_key(key)?;
        let scope = profile.scope();
        let layers = scope.layers();
        Ok(ConfigValue {
            key: key.to_owned(),
            value: layers.value(key).cloned().unwrap_or_default(),
            source: layers.source(key).to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::split_path;

    #[test]
    fn a_typed_path_splits_into_the_directory_and_the_partial_name() {
        assert_eq!(split_path("./src/ma"), ("./src/", "ma"));
        assert_eq!(split_path("./src/"), ("./src/", ""));
        assert_eq!(split_path("READ"), ("", "READ"));
        assert_eq!(split_path("/etc/ho"), ("/etc/", "ho"));
    }
}
