//! Toolset distributions (spec: replay-and-evaluation, Toolset
//! distributions): a distribution maps toolset names, or `+`-joined
//! compound keys such as `browser+web`, to inclusion probabilities. Each
//! task rolls every key independently with a generator seeded from the
//! run seed and the task id, so the same seed gives every task the same
//! toolsets in every run; a compound key enables all of its members on
//! one roll; a task whose rolls all fail gets the most probable key.

use std::collections::BTreeMap;
use std::path::Path;

use pmt_core::error::{Error, codes};
use serde::Deserialize;

use crate::rng::Rng;

/// A distribution: key → probability, in key order.
#[derive(Debug, Clone, PartialEq)]
pub struct Distribution {
    /// Its name (a built-in name or the file it came from).
    pub name: String,
    /// Keys and their probabilities.
    pub entries: BTreeMap<String, f64>,
}

/// The built-in distributions, by name.
pub const BUILTIN: &[(&str, &[(&str, f64)])] = &[
    (
        "default",
        &[
            ("web", 1.0),
            ("vision", 1.0),
            ("image_gen", 1.0),
            ("terminal", 1.0),
            ("file", 1.0),
            ("browser", 1.0),
            ("skills", 1.0),
            ("todo", 1.0),
        ],
    ),
    (
        "image_gen",
        &[
            ("image_gen", 0.9),
            ("vision", 0.9),
            ("web", 0.55),
            ("terminal", 0.45),
            ("file", 0.45),
        ],
    ),
    (
        "research",
        &[
            ("web", 0.9),
            ("browser", 0.7),
            ("vision", 0.5),
            ("file", 0.5),
            ("terminal", 0.1),
        ],
    ),
    (
        "science",
        &[
            ("web", 0.94),
            ("terminal", 0.94),
            ("file", 0.9),
            ("vision", 0.65),
            ("browser", 0.5),
            ("image_gen", 0.15),
        ],
    ),
    (
        "development",
        &[
            ("terminal", 0.8),
            ("file", 0.8),
            ("code_execution", 0.5),
            ("web", 0.6),
            ("browser", 0.3),
        ],
    ),
    (
        "safe",
        &[
            ("web", 0.8),
            ("browser", 0.7),
            ("vision", 0.6),
            ("image_gen", 0.6),
            ("file", 0.5),
        ],
    ),
    (
        "balanced",
        &[
            ("web", 0.5),
            ("vision", 0.5),
            ("image_gen", 0.5),
            ("terminal", 0.5),
            ("file", 0.5),
            ("browser", 0.5),
        ],
    ),
    ("minimal", &[("web", 1.0)]),
    ("terminal_only", &[("terminal", 1.0), ("file", 1.0)]),
    (
        "terminal_web",
        &[("terminal", 1.0), ("file", 1.0), ("web", 1.0)],
    ),
    (
        "creative",
        &[("image_gen", 0.9), ("vision", 0.9), ("web", 0.3)],
    ),
    (
        "reasoning",
        &[("code_execution", 0.6), ("web", 0.3), ("terminal", 0.1)],
    ),
    (
        "browser_use",
        &[("browser", 1.0), ("web", 0.8), ("vision", 0.7)],
    ),
    ("browser_only", &[("browser", 1.0)]),
    (
        "browser_tasks",
        &[("browser+web", 0.97), ("vision", 0.3), ("terminal", 0.1)],
    ),
    (
        "terminal_tasks",
        &[
            ("terminal", 0.97),
            ("file", 0.97),
            ("web", 0.6),
            ("browser", 0.3),
        ],
    ),
    (
        "mixed_tasks",
        &[
            ("browser+web", 0.66),
            ("terminal", 0.66),
            ("file", 0.66),
            ("vision", 0.3),
        ],
    ),
];

fn invalid(message: String) -> Error {
    Error::new(&codes::EVAL_INVALID_DATASET, message)
}

/// `[toolsets]` of a distribution file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    toolsets: BTreeMap<String, f64>,
}

impl Distribution {
    /// The built-in distribution `name`.
    #[must_use]
    pub fn builtin(name: &str) -> Option<Self> {
        BUILTIN
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(n, entries)| Self {
                name: (*n).to_owned(),
                entries: entries.iter().map(|(k, p)| ((*k).to_owned(), *p)).collect(),
            })
    }

    /// A built-in name, else a TOML file `[toolsets] web = 0.9`; checked
    /// against `known` toolsets.
    ///
    /// # Errors
    ///
    /// `eval.invalid_dataset` (65) for an unreadable file, an unknown
    /// toolset or a probability outside 0–1.
    pub fn resolve(spec: &str, known: &[String]) -> Result<Self, Error> {
        let dist = if let Some(dist) = Self::builtin(spec) {
            dist
        } else {
            let text = std::fs::read_to_string(Path::new(spec)).map_err(|e| {
                invalid(format!(
                    "toolset distribution '{spec}' is neither built in nor a readable file: {e}"
                ))
            })?;
            let file: File = toml::from_str(&text)
                .map_err(|e| invalid(format!("{spec}: {}", e.to_string().trim())))?;
            Self {
                name: spec.to_owned(),
                entries: file.toolsets,
            }
        };
        dist.check(known)?;
        Ok(dist)
    }

    /// Every member is a known toolset and every probability is in 0–1.
    ///
    /// # Errors
    ///
    /// `eval.invalid_dataset` (65).
    pub fn check(&self, known: &[String]) -> Result<(), Error> {
        if self.entries.is_empty() {
            return Err(invalid(format!(
                "toolset distribution '{}' is empty",
                self.name
            )));
        }
        for (key, p) in &self.entries {
            if !(0.0..=1.0).contains(p) {
                return Err(invalid(format!(
                    "toolset distribution '{}': probability of '{key}' must be within 0-1",
                    self.name
                )));
            }
            if let Some(unknown) = key.split('+').find(|t| !known.iter().any(|k| k == t)) {
                return Err(invalid(format!(
                    "toolset distribution '{}': unknown toolset '{unknown}'",
                    self.name
                )));
            }
        }
        Ok(())
    }

    /// The toolsets of task `task_id` under run seed `seed`, sorted.
    #[must_use]
    pub fn sample(&self, seed: u64, task_id: &str) -> Vec<String> {
        let mut rng = Rng::derived(seed, task_id);
        let mut chosen: Vec<&str> = Vec::new();
        for (key, p) in &self.entries {
            if rng.unit() < *p {
                chosen.extend(key.split('+'));
            }
        }
        if chosen.is_empty() {
            let best =
                self.entries
                    .iter()
                    .fold(None::<(&String, f64)>, |best, (k, p)| match best {
                        Some((_, bp)) if bp >= *p => best,
                        _ => Some((k, *p)),
                    });
            if let Some((key, _)) = best {
                chosen.extend(key.split('+'));
            }
        }
        let mut out: Vec<String> = chosen.into_iter().map(str::to_owned).collect();
        out.sort();
        out.dedup();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known() -> Vec<String> {
        [
            "web",
            "browser",
            "terminal",
            "file",
            "vision",
            "image_gen",
            "skills",
            "todo",
            "code_execution",
        ]
        .map(str::to_owned)
        .to_vec()
    }

    #[test]
    fn every_builtin_names_known_toolsets() {
        for (name, _) in BUILTIN {
            Distribution::resolve(name, &known()).unwrap();
        }
        assert_eq!(BUILTIN.len(), 17);
    }

    /// Scenario: Reproducible sampling.
    #[test]
    fn the_same_seed_samples_the_same_toolsets() {
        let research = Distribution::builtin("research").unwrap();
        for task in ["t1", "t2", "t3", "t4"] {
            assert_eq!(research.sample(7, task), research.sample(7, task));
        }
        let differs = (0..50)
            .any(|i| research.sample(7, &format!("t{i}")) != research.sample(8, &format!("t{i}")));
        assert!(differs, "the seed matters");
    }

    /// Scenario: Compound key rolls once.
    #[test]
    fn a_compound_key_enables_all_or_none_of_its_members() {
        let dist = Distribution {
            name: "x".to_owned(),
            entries: [("browser+web".to_owned(), 0.97)].into(),
        };
        for i in 0..200 {
            let got = dist.sample(0, &format!("task-{i}"));
            assert_eq!(got, ["browser", "web"], "the fallback is the key too");
        }
        let mixed = Distribution::builtin("mixed_tasks").unwrap();
        for i in 0..200 {
            let got = mixed.sample(3, &format!("task-{i}"));
            assert_eq!(
                got.contains(&"browser".to_owned()),
                got.contains(&"web".to_owned())
            );
        }
    }

    #[test]
    fn unknown_toolsets_and_bad_probabilities_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("d.toml");
        std::fs::write(&file, "[toolsets]\nweb = 0.9\nteleport = 0.5\n").unwrap();
        let err = Distribution::resolve(file.to_str().unwrap(), &known()).unwrap_err();
        assert_eq!(err.exit_code(), 65);
        assert!(
            err.message().contains("unknown toolset 'teleport'"),
            "{err}"
        );
        std::fs::write(&file, "[toolsets]\nweb = 0.9\n").unwrap();
        let ok = Distribution::resolve(file.to_str().unwrap(), &known()).unwrap();
        assert_eq!(ok.entries.len(), 1);
    }
}
