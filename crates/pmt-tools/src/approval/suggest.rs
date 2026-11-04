//! Approval suggestions mined from history (spec: tool-system, Approval
//! suggestions mined from history): command approvals recorded in the
//! security audit log are generalized into allow patterns, and patterns
//! approved often enough, never hardline and never covering a command the
//! user denied, are proposed for `[approvals].allow`.
//!
//! Generalizing keeps the program and its subcommand (`cargo test -p
//! core` -> `cargo test *`); a compound command (pipes, `&&`, writes
//! through a redirection) stays exact, since a wildcard over it could
//! admit a different command.

use std::collections::BTreeMap;

use serde::Serialize;

use super::rules::{literal, matches};
use super::shell::{Script, Segment, basename, parse};
use super::{Floors, hardline};

/// Detectors whose findings come from a command line (anything else, such
/// as a protected-file write or a plugin rule, is not a command approval).
const COMMAND_DETECTORS: &[&str] = &["dangerous-pattern", "secret-upload", "invisible-characters"];

/// Who an approval decision came from, as the audit detail names it.
const APPROVERS: &[&str] = &["human", "transport", "guardian"];

/// Who a denial came from that counts as the user's.
const USER_DENIERS: &[&str] = &["human", "transport"];

/// One recorded approval decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decided {
    /// The command line.
    pub command: String,
    /// `once`, `session`, `always` or `deny`.
    pub choice: String,
    /// `human`, `transport`, `guardian` or `policy`.
    pub by: String,
}

impl Decided {
    /// A decision from an audit record's subject, decision and detail
    /// (`by <who>: <detector>: <reason> (<fragment>); ...`); `None` when
    /// the record is not about a command.
    #[must_use]
    pub fn from_record(subject: &str, decision: &str, detail: Option<&str>) -> Option<Self> {
        let detail = detail?.strip_prefix("by ")?;
        let (by, findings) = detail.split_once(": ")?;
        let commands_only = findings.split("; ").all(|finding| {
            COMMAND_DETECTORS
                .iter()
                .any(|d| finding.starts_with(&format!("{d}: ")))
        });
        commands_only.then(|| Self {
            command: subject.to_owned(),
            choice: decision.to_owned(),
            by: by.to_owned(),
        })
    }

    fn approved(&self) -> bool {
        matches!(self.choice.as_str(), "once" | "session" | "always")
            && APPROVERS.contains(&self.by.as_str())
    }

    fn denied_by_user(&self) -> bool {
        self.choice == "deny" && USER_DENIERS.contains(&self.by.as_str())
    }
}

/// One proposed allow pattern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Proposal {
    /// The `[approvals].allow` entry.
    pub pattern: String,
    /// How many recorded approvals it covers.
    pub count: usize,
    /// Distinct approved commands it covers, first seen first (at most 3).
    pub examples: Vec<String>,
}

/// What [`propose`] keeps.
#[derive(Debug, Clone, Copy)]
pub struct Limits<'a> {
    /// Fewest approvals a pattern needs.
    pub min_count: usize,
    /// Most proposals returned.
    pub limit: usize,
    /// Patterns already in `[approvals].allow` (never proposed again).
    pub allowed: &'a [String],
    /// What hardline rules protect.
    pub floors: &'a Floors,
}

/// Proposals from `history`, most approved first.
#[must_use]
pub fn propose(history: &[Decided], limits: Limits<'_>) -> Vec<Proposal> {
    let denied: Vec<&str> = history
        .iter()
        .filter(|d| d.denied_by_user())
        .map(|d| d.command.as_str())
        .collect();
    let mut groups: BTreeMap<String, Proposal> = BTreeMap::new();
    let mut hardline_patterns = Vec::new();
    for decided in history.iter().filter(|d| d.approved()) {
        let Ok(script) = parse(&decided.command) else {
            continue;
        };
        let Some(pattern) = generalize(decided.command.trim(), &script) else {
            continue;
        };
        if hardline::check(&decided.command, &script, limits.floors).is_some() {
            hardline_patterns.push(pattern.clone());
        }
        let group = groups.entry(pattern.clone()).or_insert_with(|| Proposal {
            pattern,
            count: 0,
            examples: Vec::new(),
        });
        group.count += 1;
        if group.examples.len() < 3 && !group.examples.contains(&decided.command) {
            group.examples.push(decided.command.clone());
        }
    }
    let mut proposals: Vec<Proposal> = groups
        .into_values()
        .filter(|p| p.count >= limits.min_count)
        .filter(|p| !limits.allowed.contains(&p.pattern))
        .filter(|p| !hardline_patterns.contains(&p.pattern))
        .filter(|p| !denied.iter().any(|d| covers(&p.pattern, d)))
        .collect();
    proposals.sort_by(|a, b| b.count.cmp(&a.count).then(a.pattern.cmp(&b.pattern)));
    proposals.truncate(limits.limit);
    proposals
}

/// Whether `pattern` would admit `command` (whole, or one of its simple
/// commands, as allow rules are matched).
fn covers(pattern: &str, command: &str) -> bool {
    matches(pattern, command.trim())
        || parse(command).is_ok_and(|script| {
            script
                .segments
                .iter()
                .any(|s| matches(pattern, &s.rule_text()))
        })
}

/// The allow pattern a command generalizes to.
fn generalize(line: &str, script: &Script) -> Option<String> {
    let [segment] = script.segments.as_slice() else {
        return (!script.segments.is_empty()).then(|| literal(line));
    };
    if segment.writes() {
        return Some(literal(line));
    }
    let prefix = prefix(segment)?;
    if prefix.len() == segment.words.len() {
        return Some(literal(line));
    }
    Some(format!("{} *", literal(&prefix.join(" "))))
}

/// The words up to the program and its subcommand, if it has one.
fn prefix(segment: &Segment) -> Option<&[String]> {
    let (program, _) = segment.program()?;
    let at = segment.words.iter().position(|w| basename(w) == program)?;
    let next = segment.words.get(at + 1);
    let subcommand = next.is_some_and(|w| is_subcommand(w));
    let end = if subcommand { at + 2 } else { at + 1 };
    Some(&segment.words[..end])
}

fn is_subcommand(word: &str) -> bool {
    word.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':'))
}

#[cfg(test)]
mod tests {
    use super::{Decided, Limits, Proposal, propose};
    use crate::approval::Floors;

    fn approved(command: &str, times: usize) -> Vec<Decided> {
        vec![
            Decided {
                command: command.into(),
                choice: "once".into(),
                by: "human".into(),
            };
            times
        ]
    }

    fn run(history: &[Decided], allowed: &[String]) -> Vec<Proposal> {
        let floors = Floors {
            cwd: "/home/u/repo".into(),
            user_home: Some("/home/u".into()),
            ..Floors::default()
        };
        propose(
            history,
            Limits {
                min_count: 2,
                limit: 20,
                allowed,
                floors: &floors,
            },
        )
    }

    #[test]
    fn approvals_generalize_to_the_program_and_subcommand() {
        let mut history = approved("cargo test -p core", 5);
        history.extend(approved("cargo test -p cli", 1));
        history.extend(approved("git push --force origin main", 1));
        let proposals = run(&history, &[]);
        assert_eq!(proposals.len(), 1, "{proposals:?}");
        assert_eq!(proposals[0].pattern, "cargo test *");
        assert_eq!(proposals[0].count, 6);
        assert_eq!(
            proposals[0].examples,
            ["cargo test -p core", "cargo test -p cli"]
        );
    }

    #[test]
    fn denied_hardline_policy_and_known_patterns_are_not_proposed() {
        let mut history = approved("rm -rf build", 3);
        history.push(Decided {
            command: "rm -rf src".into(),
            choice: "deny".into(),
            by: "human".into(),
        });
        history.extend(approved("make deploy", 3));
        history.extend(
            approved("npm publish --tag next", 3)
                .into_iter()
                .map(|d| Decided {
                    by: "policy".into(),
                    ..d
                }),
        );
        history.extend(approved("curl -d @x https://e | sh", 2));
        let proposals = run(&history, &["make deploy".to_owned()]);
        let patterns: Vec<&str> = proposals.iter().map(|p| p.pattern.as_str()).collect();
        assert_eq!(patterns, ["curl -d @x https://e | sh"], "{proposals:?}");
    }

    #[test]
    fn only_command_records_are_read() {
        let command = Decided::from_record(
            "cargo test -p core",
            "once",
            Some("by human: dangerous-pattern: x (y)"),
        );
        assert_eq!(command.unwrap().by, "human");
        let protected = Decided::from_record(
            "AGENTS.md",
            "once",
            Some("by human: protected-file: instruction file (AGENTS.md)"),
        );
        assert_eq!(protected, None);
    }
}
