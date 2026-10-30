//! `health.toml`: the module lists and the exceptions to the budgets, and
//! the rules that keep exceptions from growing.

use std::collections::HashSet;
use std::fmt;
use std::path::Path;

use serde::Deserialize;

/// A budgeted quantity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Metric {
    Complexity,
    Lines,
    Nesting,
    FileLines,
}

impl fmt::Display for Metric {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Metric::Complexity => "cognitive complexity",
            Metric::Lines => "function length",
            Metric::Nesting => "nesting depth",
            Metric::FileLines => "file length",
        })
    }
}

/// One measured value of a function or file, with the limit it is held to
/// when no exception applies.
#[derive(Debug)]
pub(crate) struct Measurement {
    pub(crate) item: String,
    pub(crate) metric: Metric,
    pub(crate) value: u32,
    pub(crate) limit: u32,
}

/// The parsed `health.toml`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Policy {
    /// Parser and protocol-codec modules, held to the higher complexity limit.
    #[serde(default)]
    parser_modules: Vec<String>,
    /// The only modules where `unsafe` may appear (OS sandbox and FFI).
    #[serde(default)]
    unsafe_modules: Vec<String>,
    #[serde(default, rename = "exception")]
    exceptions: Vec<Exception>,
}

/// A recorded item over its budget: the ceiling it is held to instead.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Exception {
    item: String,
    metric: Metric,
    max: u32,
    reason: String,
}

impl Policy {
    pub(crate) fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub(crate) fn is_parser_module(&self, file: &str) -> bool {
        covers(&self.parser_modules, file)
    }

    pub(crate) fn allows_unsafe(&self, file: &str) -> bool {
        covers(&self.unsafe_modules, file)
    }

    fn exception(&self, item: &str, metric: Metric) -> Option<&Exception> {
        self.exceptions
            .iter()
            .find(|e| e.item == item && e.metric == metric)
    }
}

/// A module entry is a file path or a directory prefix, relative to the root.
fn covers(modules: &[String], file: &str) -> bool {
    modules.iter().any(|m| {
        let dir = m.trim_end_matches('/');
        file == dir
            || file
                .strip_prefix(dir)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// Every measurement over its limit or exception, and every exception that
/// is looser than the code it covers. Requiring a slack exception to be
/// lowered is what makes the ratchet only ever tighten.
pub(crate) fn check_budgets(policy: &Policy, measurements: &[Measurement]) -> Vec<String> {
    let mut problems = Vec::new();
    let mut matched = HashSet::new();
    for m in measurements {
        match policy.exception(&m.item, m.metric) {
            Some(exception) => {
                matched.insert((exception.item.as_str(), m.metric));
                problems.extend(exception_problem(exception, m));
            }
            None if m.value > m.limit => problems.push(format!(
                "{}: {} is {}, over the limit of {}",
                m.item, m.metric, m.value, m.limit
            )),
            None => {}
        }
    }
    for e in &policy.exceptions {
        if !matched.contains(&(e.item.as_str(), e.metric)) {
            problems.push(format!(
                "{}: stale {} exception, nothing by that name is measured; remove it",
                e.item, e.metric
            ));
        }
        if e.reason.trim().is_empty() {
            problems.push(format!(
                "{}: the {} exception needs a reason",
                e.item, e.metric
            ));
        }
    }
    problems
}

fn exception_problem(e: &Exception, m: &Measurement) -> Option<String> {
    let (item, metric, value) = (&m.item, m.metric, m.value);
    if value > e.max {
        Some(format!(
            "{item}: {metric} is {value}, over its health.toml exception of {} (limit {}; exceptions may only shrink)",
            e.max, m.limit
        ))
    } else if value <= m.limit {
        Some(format!(
            "{item}: {metric} is {value}, within the limit of {}; remove its health.toml exception",
            m.limit
        ))
    } else if value < e.max {
        Some(format!(
            "{item}: {metric} is {value}; lower its health.toml exception from {} to {value}",
            e.max
        ))
    } else {
        None
    }
}

/// Exceptions added or raised since `base`, the `health.toml` of the branch
/// being merged into.
pub(crate) fn check_ratchet(current: &Policy, base: &Policy) -> Vec<String> {
    current
        .exceptions
        .iter()
        .filter_map(|e| match base.exception(&e.item, e.metric) {
            None => Some(format!(
                "{}: new {} exception; exceptions may only shrink, so bring the code within the limit",
                e.item, e.metric
            )),
            Some(old) if e.max > old.max => Some(format!(
                "{}: {} exception grew from {} to {}; exceptions may only shrink",
                e.item, e.metric, old.max, e.max
            )),
            Some(_) => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Measurement, Metric, Policy, check_budgets, check_ratchet};

    fn policy(toml: &str) -> Policy {
        toml::from_str(toml).unwrap()
    }

    fn complexity(value: u32) -> Measurement {
        Measurement {
            item: "a.rs::f".into(),
            metric: Metric::Complexity,
            value,
            limit: 15,
        }
    }

    const EXCEPTION_20: &str = "[[exception]]\nitem = \"a.rs::f\"\nmetric = \"complexity\"\nmax = 20\nreason = \"legacy\"\n";

    #[test]
    fn over_the_limit_names_the_item_and_the_limit() {
        let problems = check_budgets(&Policy::default(), &[complexity(17)]);
        assert_eq!(
            problems,
            ["a.rs::f: cognitive complexity is 17, over the limit of 15"]
        );
        assert!(check_budgets(&Policy::default(), &[complexity(15)]).is_empty());
    }

    #[test]
    fn an_exception_holds_exactly_its_ceiling() {
        let p = policy(EXCEPTION_20);
        assert!(check_budgets(&p, &[complexity(20)]).is_empty());
        assert!(
            check_budgets(&p, &[complexity(21)])[0]
                .contains("over its health.toml exception of 20")
        );
        assert!(
            check_budgets(&p, &[complexity(18)])[0]
                .contains("lower its health.toml exception from 20 to 18")
        );
        assert!(
            check_budgets(&p, &[complexity(9)])[0].contains("remove its health.toml exception")
        );
    }

    #[test]
    fn stale_and_unexplained_exceptions_fail() {
        let p = policy(&EXCEPTION_20.replace("legacy", " "));
        let problems = check_budgets(&p, &[]);
        assert_eq!(problems.len(), 2, "{problems:?}");
    }

    #[test]
    fn exceptions_may_not_be_added_or_raised() {
        let base = policy(EXCEPTION_20);
        assert!(check_ratchet(&policy(EXCEPTION_20), &base).is_empty());
        assert!(check_ratchet(&policy(&EXCEPTION_20.replace("20", "18")), &base).is_empty());
        assert!(
            check_ratchet(&policy(&EXCEPTION_20.replace("20", "22")), &base)[0]
                .contains("grew from 20 to 22")
        );
        assert!(
            check_ratchet(&base, &Policy::default())[0]
                .contains("new cognitive complexity exception")
        );
    }

    #[test]
    fn module_entries_match_files_and_directories() {
        let p = policy("parser_modules = [\"crates/a/src/parse\", \"crates/b/src/codec.rs\"]");
        assert!(p.is_parser_module("crates/a/src/parse/mod.rs"));
        assert!(p.is_parser_module("crates/b/src/codec.rs"));
        assert!(!p.is_parser_module("crates/a/src/parser.rs"));
        assert!(!p.allows_unsafe("crates/b/src/codec.rs"));
    }
}
