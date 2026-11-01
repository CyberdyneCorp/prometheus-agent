//! Closed, name-keyed registries (spec: workspace-architecture, Typed
//! registries reject unknown names).
//!
//! Providers, terminal backends, toolsets, context engines, hooks and every
//! other name-keyed set use [`Registry`]. A lookup is an exact match or an
//! error: there is no default entry, no case folding and no fallback, so a
//! misspelled provider can never silently select another one.
//!
//! The error kind is `config` (exit 78): registry names arrive from
//! configuration (`config.toml`, plugin manifests), and every spec scenario
//! for an unknown name exits 78.

use std::collections::BTreeMap;

use crate::error::{Error, codes};

/// How many close names an unknown-name error suggests at most.
const MAX_SUGGESTIONS: usize = 3;
/// The edit distance at which a name still counts as a likely typo (the
/// same bound as config-key suggestions), so `lcm` does not suggest `builtin`.
const MAX_DISTANCE: usize = 2;

/// A closed set of named entries of one category.
#[derive(Debug, Clone)]
pub struct Registry<T> {
    category: &'static str,
    entries: BTreeMap<String, T>,
}

impl<T> Registry<T> {
    /// An empty registry. `category` is the singular noun used in errors,
    /// for example `"provider"` or `"context engine"`.
    #[must_use]
    pub fn new(category: &'static str) -> Self {
        Self {
            category,
            entries: BTreeMap::new(),
        }
    }

    /// A registry holding `entries`.
    ///
    /// # Errors
    ///
    /// `registry.duplicate_name` when two entries share a name.
    pub fn from_entries(
        category: &'static str,
        entries: impl IntoIterator<Item = (impl Into<String>, T)>,
    ) -> Result<Self, Error> {
        let mut registry = Self::new(category);
        for (name, value) in entries {
            registry.register(name, value)?;
        }
        Ok(registry)
    }

    /// Adds an entry.
    ///
    /// # Errors
    ///
    /// `registry.duplicate_name` when the name is already registered; the
    /// existing entry is kept.
    pub fn register(&mut self, name: impl Into<String>, value: T) -> Result<(), Error> {
        let name = name.into();
        if self.entries.contains_key(&name) {
            return Err(Error::new(
                &codes::REGISTRY_DUPLICATE_NAME,
                format!("{} '{name}' is registered twice", self.category),
            ));
        }
        self.entries.insert(name, value);
        Ok(())
    }

    /// The entry registered under exactly `name`.
    ///
    /// # Errors
    ///
    /// `registry.unknown_name` (kind `config`, exit 78) naming the unknown
    /// entry, up to three close registered names, and every registered name
    /// in the hint.
    pub fn get(&self, name: &str) -> Result<&T, Error> {
        self.entries.get(name).ok_or_else(|| self.unknown(name))
    }

    /// Whether `name` is registered.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.entries.contains_key(name)
    }

    /// Registered names, sorted.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }

    /// Entries in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &T)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    fn unknown(&self, name: &str) -> Error {
        let category = self.category;
        let suggestions = closest(name, self.names());
        let message = match suggestions.as_slice() {
            [] => format!("unknown {category} '{name}'"),
            names => format!(
                "unknown {category} '{name}' (did you mean {}?)",
                or_list(names)
            ),
        };
        let hint = if self.entries.is_empty() {
            format!("no {category} is registered")
        } else {
            let all: Vec<_> = self.names().collect();
            format!("registered {category} names: {}", all.join(", "))
        };
        Error::new(&codes::REGISTRY_UNKNOWN_NAME, message).with_hint(hint)
    }
}

/// `'a'`, `'a' or 'b'`, `'a', 'b' or 'c'`.
#[must_use]
pub fn or_list(names: &[&str]) -> String {
    let quoted: Vec<String> = names.iter().map(|n| format!("'{n}'")).collect();
    match quoted.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
        _ => quoted.concat(),
    }
}

/// Up to [`MAX_SUGGESTIONS`] registered names within [`MAX_DISTANCE`]
/// edits of `name`, nearest first. Shared by every "did you mean"
/// message (registries, config keys).
#[must_use]
pub fn closest<'a>(name: &str, candidates: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut scored: Vec<(usize, &str)> = candidates
        .map(|c| (edit_distance(name, c), c))
        .filter(|(d, _)| *d <= MAX_DISTANCE)
        .collect();
    scored.sort_unstable();
    scored
        .into_iter()
        .take(MAX_SUGGESTIONS)
        .map(|(_, c)| c)
        .collect()
}

/// Levenshtein distance over chars, single-row dynamic programming.
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let substitute = diagonal + usize::from(ca != *cb);
            diagonal = row[j + 1];
            row[j + 1] = substitute.min(row[j] + 1).min(diagonal + 1);
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::{Registry, edit_distance};
    use crate::error::ErrorKind;

    fn hooks() -> Registry<u8> {
        Registry::from_entries(
            "hook",
            [
                ("pre_tool_call", 1),
                ("post_tool_call", 2),
                ("on_session_start", 3),
            ],
        )
        .unwrap()
    }

    #[test]
    fn registered_name_resolves() {
        assert_eq!(*hooks().get("post_tool_call").unwrap(), 2);
    }

    #[test]
    fn misspelled_hook_suggests_the_closest_name() {
        let err = hooks().get("pre_tool_cal").unwrap_err();
        assert_eq!(
            err.message(),
            "unknown hook 'pre_tool_cal' (did you mean 'pre_tool_call'?)"
        );
        assert_eq!(err.kind(), ErrorKind::Config);
        assert_eq!(err.code().code, "registry.unknown_name");
        assert_eq!(err.exit_code(), 78);
        assert_eq!(
            err.hint(),
            Some("registered hook names: on_session_start, post_tool_call, pre_tool_call")
        );
    }

    #[test]
    fn unknown_name_never_falls_back_to_a_default() {
        let engines = Registry::from_entries("context engine", [("builtin", ())]).unwrap();
        let err = engines.get("lcm").unwrap_err();
        assert_eq!(err.message(), "unknown context engine 'lcm'");
        assert_eq!(err.hint(), Some("registered context engine names: builtin"));
        assert!(engines.get("").is_err());
    }

    #[test]
    fn lookups_are_exact_and_case_sensitive() {
        let providers = Registry::from_entries("provider", [("openai", ())]).unwrap();
        for near in ["OpenAI", "openai ", " openai", "open-ai"] {
            assert!(providers.get(near).is_err(), "{near:?} resolved");
        }
    }

    #[test]
    fn at_most_three_suggestions_nearest_first() {
        let r = Registry::from_entries(
            "toolset",
            [
                ("webx", ()),
                ("web", ()),
                ("weba", ()),
                ("webby", ()),
                ("wex", ()),
            ],
        )
        .unwrap();
        let err = r.get("wb").unwrap_err();
        assert_eq!(
            err.message(),
            "unknown toolset 'wb' (did you mean 'web', 'weba' or 'webx'?)"
        );
    }

    #[test]
    fn empty_registry_says_nothing_is_registered() {
        let err = Registry::<()>::new("terminal backend")
            .get("ssh")
            .unwrap_err();
        assert_eq!(err.hint(), Some("no terminal backend is registered"));
    }

    #[test]
    fn duplicate_registration_is_rejected_and_keeps_the_first() {
        let mut r = Registry::new("tool");
        r.register("read_file", 1).unwrap();
        let err = r.register("read_file", 2).unwrap_err();
        assert_eq!(err.code().code, "registry.duplicate_name");
        assert_eq!(*r.get("read_file").unwrap(), 1);
    }

    #[test]
    fn edit_distance_basics() {
        assert_eq!(edit_distance("", "abc"), 3);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("same", "same"), 0);
    }
}
