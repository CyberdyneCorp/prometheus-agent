//! Dynamic values: profile names, session ids and titles, config keys,
//! skill names and slash commands. Everything is read from local files
//! only, without creating, migrating or locking anything, and without
//! starting the daemon or opening a network connection (spec: cli, Shell
//! completions).

use std::path::{Path, PathBuf};

/// A kind of dynamic value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Profiles,
    Sessions,
    ConfigKeys,
    Skills,
    SlashCommands,
}

/// The dynamic kind of argument `arg` of the command at `path`.
pub(super) fn kind(path: &[String], arg: &str) -> Option<Kind> {
    let path: Vec<&str> = path.iter().map(String::as_str).collect();
    match (path.as_slice(), arg) {
        (_, "profile" | "clone_from")
        | (
            [
                "profile",
                "use" | "show" | "rename" | "delete" | "export" | "alias",
            ],
            "name" | "old",
        ) => Some(Kind::Profiles),
        (_, "resume" | "continue_" | "session") => Some(Kind::Sessions),
        (["config", "get" | "set" | "unset" | "explain"], "key") => Some(Kind::ConfigKeys),
        (["skills", ..], "name") => Some(Kind::Skills),
        (_, "oneshot") => Some(Kind::SlashCommands),
        _ => None,
    }
}

/// Where the values live: the home and profile named on the command line
/// (else the launch environment's home and the home's active profile).
pub(super) struct Sources {
    home: Option<PathBuf>,
    profile: Option<String>,
}

/// Session candidates offered at most (newest activity first).
const SESSION_LIMIT: u32 = 200;

impl Sources {
    pub(super) fn new(home: Option<PathBuf>, profile: Option<String>) -> Self {
        Self { home, profile }
    }

    /// The candidates of `kind` starting with `prefix`, with their help.
    pub(super) fn values(&self, kind: Kind, prefix: &str) -> Vec<(String, String)> {
        match kind {
            Kind::Profiles => self.profiles(),
            Kind::Sessions => self.sessions(prefix),
            Kind::ConfigKeys => config_keys(),
            Kind::Skills => self.skills(),
            Kind::SlashCommands if prefix.starts_with('/') => slash_commands(),
            Kind::SlashCommands => Vec::new(),
        }
    }

    fn profiles(&self) -> Vec<(String, String)> {
        let Some(home) = &self.home else {
            return Vec::new();
        };
        let Ok(entries) = std::fs::read_dir(home.join("profiles")) else {
            return Vec::new();
        };
        let mut names: Vec<(String, String)> = entries
            .filter_map(Result::ok)
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|name| pmt_config::ProfileName::parse(name).is_ok())
            .map(|name| (name, String::from("profile")))
            .collect();
        names.sort();
        names
    }

    /// The selected profile's directory, when it exists.
    fn profile_dir(&self) -> Option<PathBuf> {
        let home = self.home.as_ref()?;
        let name = self
            .profile
            .clone()
            .or_else(|| active_profile(home))
            .unwrap_or_else(|| "default".to_owned());
        let dir = home.join("profiles").join(name.to_lowercase());
        dir.is_dir().then_some(dir)
    }

    fn sessions(&self, prefix: &str) -> Vec<(String, String)> {
        let Some(dir) = self.profile_dir() else {
            return Vec::new();
        };
        let refs =
            pmt_store::inspect::session_refs(&dir, prefix, SESSION_LIMIT).unwrap_or_default();
        let mut out = Vec::new();
        for (id, title) in refs {
            let title = title.unwrap_or_default();
            if title.starts_with(prefix) && !title.is_empty() {
                out.push((title.clone(), id.clone()));
            }
            if id.starts_with(prefix) {
                out.push((id, title));
            }
        }
        out
    }

    fn skills(&self) -> Vec<(String, String)> {
        let mut paths: Vec<String> = pmt_skills::bundled::Bundled::builtin()
            .paths()
            .map(str::to_owned)
            .collect();
        if let Some(dir) = self.profile_dir() {
            paths.extend(pmt_skills::layout::walk(&dir.join("skills")));
        }
        let mut names: Vec<String> =
            pmt_skills::layout::skill_dirs(paths.iter().map(String::as_str))
                .iter()
                .filter_map(|d| d.rsplit('/').next().map(str::to_owned))
                .collect();
        names.sort();
        names.dedup();
        names
            .into_iter()
            .map(|n| (n, String::from("skill")))
            .collect()
    }
}

fn active_profile(home: &Path) -> Option<String> {
    let text = std::fs::read_to_string(home.join("active_profile")).ok()?;
    let name = text.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

fn config_keys() -> Vec<(String, String)> {
    pmt_config::catalog::keys()
        .into_iter()
        .filter(|k| !k.section && !k.key.contains('<'))
        .map(|k| (k.key.clone(), first_sentence(&k.description)))
        .collect()
}

fn slash_commands() -> Vec<(String, String)> {
    crate::slash::builtins::BUILTINS
        .iter()
        .flat_map(|c| {
            std::iter::once(c.name)
                .chain(c.aliases.iter().copied())
                .map(|name| (format!("/{name}"), c.description.to_owned()))
        })
        .collect()
}

fn first_sentence(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    line.split(". ")
        .next()
        .unwrap_or(line)
        .trim_end_matches('.')
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::{Kind, kind};

    fn path(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    #[test]
    fn arguments_map_to_their_value_kinds() {
        assert_eq!(kind(&path(&[]), "profile"), Some(Kind::Profiles));
        assert_eq!(kind(&path(&["chat"]), "resume"), Some(Kind::Sessions));
        assert_eq!(
            kind(&path(&["sessions", "show"]), "session"),
            Some(Kind::Sessions)
        );
        assert_eq!(
            kind(&path(&["config", "set"]), "key"),
            Some(Kind::ConfigKeys)
        );
        assert_eq!(
            kind(&path(&["profile", "use"]), "name"),
            Some(Kind::Profiles)
        );
        assert_eq!(kind(&path(&["profile", "create"]), "name"), None);
        assert_eq!(kind(&path(&["skills", "show"]), "name"), Some(Kind::Skills));
        assert_eq!(kind(&path(&[]), "oneshot"), Some(Kind::SlashCommands));
    }

    #[test]
    fn config_keys_are_offered_with_a_short_help() {
        let keys = super::config_keys();
        assert!(keys.iter().any(|(k, _)| k == "agent.max_iterations"));
        assert!(keys.iter().all(|(_, help)| !help.contains('\n')));
    }
}
