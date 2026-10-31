//! Command resolution (spec: cli, Command resolution): only the command
//! token is lower-cased, the leading `/` is optional, names and aliases
//! match exactly first, then a prefix of exactly one available name
//! expands, an ambiguous prefix lists its candidates and anything else is
//! `Unknown command: /<token>` with up to three close names.

use pmt_core::registry::closest;

use super::{Command, External, Registry, Surface};

/// What a slash input resolves to.
#[derive(Debug, Clone)]
pub enum Resolution {
    /// A built-in and its arguments (case kept).
    Builtin {
        /// The command.
        command: &'static Command,
        /// Everything after the command token, trimmed.
        args: String,
    },
    /// A registered command and its arguments.
    External {
        /// The command.
        command: External,
        /// Everything after the command token, trimmed.
        args: String,
    },
    /// A prefix of several commands.
    Ambiguous {
        /// What was typed.
        token: String,
        /// The matching names.
        candidates: Vec<String>,
    },
    /// No such command.
    Unknown {
        /// What was typed.
        token: String,
        /// Up to three close names.
        suggestions: Vec<String>,
    },
    /// A command that exists but not on this surface.
    NotHere {
        /// Its name.
        name: String,
    },
}

impl Resolution {
    /// The line to show for a resolution that runs nothing.
    #[must_use]
    pub fn message(&self) -> Option<String> {
        let slashed = |names: &[String]| {
            names
                .iter()
                .map(|n| format!("/{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        match self {
            Self::Builtin { .. } | Self::External { .. } => None,
            Self::Ambiguous { token, candidates } => {
                Some(format!("/{token} is ambiguous: {}", slashed(candidates)))
            }
            Self::Unknown { token, suggestions } if suggestions.is_empty() => {
                Some(format!("Unknown command: /{token}"))
            }
            Self::Unknown { token, suggestions } => Some(format!(
                "Unknown command: /{token} (did you mean {}?)",
                slashed(suggestions)
            )),
            Self::NotHere { name } => Some(format!("/{name} is not available here")),
        }
    }
}

/// The command token (lower-cased, without `/`) and the arguments.
#[must_use]
pub fn split(input: &str) -> (String, String) {
    let text = input.trim();
    let text = text.strip_prefix('/').unwrap_or(text);
    match text.split_once(char::is_whitespace) {
        Some((token, rest)) => (token.to_lowercase(), rest.trim().to_owned()),
        None => (text.to_lowercase(), String::new()),
    }
}

impl Registry {
    /// Resolves `input` (with or without its leading `/`) on `surface`.
    #[must_use]
    pub fn resolve(&self, input: &str, surface: Surface) -> Resolution {
        let (token, args) = split(input);
        if let Some(found) = self.exact(&token, surface, &args) {
            return found;
        }
        let available = self.available(surface);
        let matches: Vec<&String> = available
            .iter()
            .filter(|(name, primary)| *primary && name.starts_with(&token) && !token.is_empty())
            .map(|(name, _)| name)
            .collect();
        match matches.as_slice() {
            [one] => self
                .exact(one, surface, &args)
                .unwrap_or_else(|| unknown(&token, &available)),
            [] => unknown(&token, &available),
            many => Resolution::Ambiguous {
                token,
                candidates: many.iter().map(|n| (*n).clone()).collect(),
            },
        }
    }

    fn exact(&self, token: &str, surface: Surface, args: &str) -> Option<Resolution> {
        if let Some(command) = self
            .builtins
            .iter()
            .find(|c| c.name == token || c.aliases.contains(&token))
        {
            return Some(if command.on(surface) {
                Resolution::Builtin {
                    command,
                    args: args.to_owned(),
                }
            } else {
                Resolution::NotHere {
                    name: command.name.to_owned(),
                }
            });
        }
        let external = self
            .external
            .values()
            .find(|e| e.name == token || e.aliases.iter().any(|a| a == token))?;
        Some(if external.surfaces.contains(&surface) {
            Resolution::External {
                command: external.clone(),
                args: args.to_owned(),
            }
        } else {
            Resolution::NotHere {
                name: external.name.clone(),
            }
        })
    }

    /// Every name and alias available on `surface`, with whether it is a
    /// command's primary name (prefixes expand to primary names only).
    fn available(&self, surface: Surface) -> Vec<(String, bool)> {
        let mut names = Vec::new();
        for command in self.builtins.iter().filter(|c| c.on(surface)) {
            names.push((command.name.to_owned(), true));
            names.extend(command.aliases.iter().map(|a| ((*a).to_owned(), false)));
        }
        for external in self
            .external
            .values()
            .filter(|e| e.surfaces.contains(&surface))
        {
            names.push((external.name.clone(), true));
            names.extend(external.aliases.iter().map(|a| (a.clone(), false)));
        }
        names
    }
}

fn unknown(token: &str, available: &[(String, bool)]) -> Resolution {
    let suggestions = closest(token, available.iter().map(|(n, _)| n.as_str()))
        .into_iter()
        .map(str::to_owned)
        .collect();
    Resolution::Unknown {
        token: token.to_owned(),
        suggestions,
    }
}
