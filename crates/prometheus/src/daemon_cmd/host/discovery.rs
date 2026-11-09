//! The daemon side of the discovery methods (spec: tui, Discovery RPCs
//! preserve the prompt cache; Composer and path completion; Profile scope
//! on every request): the session's command catalog, path completion
//! through the session's own file backend, its model options and one
//! configuration value. All of them read: none rebuilds the session's
//! system prompt, swaps its toolset or reloads its memory snapshot.

use pmt_config::schema::ModelAlias;
use pmt_core::error::Error;
use pmt_rpc::contracts::{
    BannerInfo, ClientKind, CommandCatalog, CommandEntry, CommandSource, Completion, Completions,
    ConfigValue, Disposition, FrozenPrompt, ModelOption, ModelOptions,
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

fn disposition(runs: bool) -> (Disposition, Option<String>) {
    if runs {
        (Disposition::Exec, None)
    } else {
        (
            Disposition::Unavailable,
            Some("not available on this surface".to_owned()),
        )
    }
}

/// Every command of `registry` with its disposition on `surface`.
pub(super) fn catalog(registry: &Registry, surface: Surface) -> CommandCatalog {
    let mut commands: Vec<CommandEntry> = registry
        .builtins()
        .iter()
        .map(|command| {
            let (disposition, reason) = disposition(command.on(surface));
            let client = CLIENT_COMMANDS.contains(&command.name);
            CommandEntry {
                name: command.name.to_owned(),
                aliases: command.aliases.iter().map(|a| (*a).to_owned()).collect(),
                description: command.description.to_owned(),
                args: (!command.args.is_empty()).then(|| command.usage()),
                source: if client {
                    CommandSource::Client
                } else {
                    CommandSource::Builtin
                },
                disposition,
                reason,
            }
        })
        .collect();
    commands.extend(registry.externals().map(|external| {
        let (disposition, reason) = disposition(external.surfaces.contains(&surface));
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
        Ok(catalog(&registry, surface(client)))
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

    pub(super) async fn models_of(
        &self,
        profile: &Profile,
        session: &str,
    ) -> Result<ModelOptions, Error> {
        let id = session_id(session)?;
        let loaded = profile.loaded.lock().await.get(&id).cloned();
        let current = loaded.map(|s| {
            let target = s.live.model.target();
            ModelOption {
                id: format!("{}/{}", target.provider, target.model.unwrap_or_default()),
                provider: target.provider,
                context_window: None,
            }
        });
        let scope = profile.scope();
        let mut models: Vec<ModelOption> = current.iter().cloned().collect();
        for alias in scope.config().model.aliases.values() {
            let (provider, model) = match alias {
                ModelAlias::Ref(reference) => match reference.split_once('/') {
                    Some((provider, model)) => (provider.to_owned(), model.to_owned()),
                    None => continue,
                },
                ModelAlias::Route(route) => (route.provider.clone(), route.model.clone()),
            };
            let id = format!("{provider}/{model}");
            if !models.iter().any(|m| m.id == id) {
                models.push(ModelOption {
                    id,
                    provider,
                    context_window: None,
                });
            }
        }
        Ok(ModelOptions {
            current: current.map(|c| c.id),
            models,
        })
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
