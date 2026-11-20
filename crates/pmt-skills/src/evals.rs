//! Skill evals (spec: skills, Skill evals; Eval-gated skill edits).
//!
//! A skill may carry `evals/evals.toml`, a list of `[[case]]` entries:
//!
//! ```toml
//! [[case]]
//! name = "sums"                       # optional; default case-<n>
//! prompt = "sum column B of data.csv"
//! setup_files = ["data.csv"]          # from evals/, copied into the workspace
//! expect_skill_loaded = true          # skill_view(name = <skill>) was called
//! expect_tools = ["terminal"]         # each was called
//! expect_output_contains = "42"
//! expect_output_regex = "total: \\d+"
//! judge = "The answer states the sum."  # rubric for the auxiliary judge
//! mock_script = "mock/sums.toml"      # what the mock provider plays
//! ```
//!
//! This module reads the file and judges one run's observations; running a
//! case (an isolated temporary profile) belongs to the binary, which also
//! implements [`Gate`] for eval-gated edits.

use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The file, relative to the skill directory.
pub const EVALS_FILE: &str = "evals/evals.toml";

/// One case.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    /// Its name in reports.
    #[serde(default)]
    pub name: String,
    /// The user prompt.
    pub prompt: String,
    /// Files copied from `evals/` into the case's workspace.
    #[serde(default)]
    pub setup_files: Vec<String>,
    /// The skill must be loaded with `skill_view`.
    #[serde(default)]
    pub expect_skill_loaded: Option<bool>,
    /// Tools that must be called.
    #[serde(default)]
    pub expect_tools: Vec<String>,
    /// Text the final answer must contain.
    #[serde(default)]
    pub expect_output_contains: Option<String>,
    /// A regex the final answer must match.
    #[serde(default)]
    pub expect_output_regex: Option<String>,
    /// A rubric the auxiliary judge model checks the answer against.
    #[serde(default)]
    pub judge: Option<String>,
    /// The mock script (relative to `evals/`) the mock provider plays.
    #[serde(default)]
    pub mock_script: Option<String>,
}

/// A skill's evals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evals {
    /// The `evals/` directory.
    pub dir: PathBuf,
    /// The cases, named.
    pub cases: Vec<Case>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    case: Vec<Case>,
}

/// Whether `skill_dir` has evals.
#[must_use]
pub fn has_evals(skill_dir: &Path) -> bool {
    skill_dir.join(EVALS_FILE).is_file()
}

/// A relative path below `evals/` (no `..`, not absolute).
fn inside(rel: &str) -> bool {
    let path = Path::new(rel);
    !rel.is_empty()
        && path
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

impl Evals {
    /// Reads `<skill_dir>/evals/evals.toml`; `Ok(None)` without one.
    ///
    /// # Errors
    ///
    /// Why the file does not read: TOML, an empty prompt, a bad regex, a
    /// path leaving `evals/`, a duplicate name.
    pub fn load(skill_dir: &Path) -> Result<Option<Self>, String> {
        let path = skill_dir.join(EVALS_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let file: File = toml::from_str(&text).map_err(|e| format!("{EVALS_FILE}: {e}"))?;
        let mut cases = file.case;
        for (n, case) in cases.iter_mut().enumerate() {
            if case.name.is_empty() {
                case.name = format!("case-{}", n + 1);
            }
            validate(case)?;
        }
        let mut names: Vec<&str> = cases.iter().map(|c| c.name.as_str()).collect();
        names.sort_unstable();
        if let Some([name, _]) = names.windows(2).find(|w| w[0] == w[1]) {
            return Err(format!("{EVALS_FILE}: two cases are named {name}"));
        }
        Ok(Some(Self {
            dir: skill_dir.join("evals"),
            cases,
        }))
    }
}

fn validate(case: &Case) -> Result<(), String> {
    let at = format!("{EVALS_FILE}: case {}", case.name);
    if case.prompt.trim().is_empty() {
        return Err(format!("{at}: prompt is empty"));
    }
    if let Some(pattern) = &case.expect_output_regex {
        Regex::new(pattern).map_err(|e| format!("{at}: expect_output_regex: {e}"))?;
    }
    let paths = case.setup_files.iter().chain(case.mock_script.iter());
    if let Some(bad) = paths.into_iter().find(|p| !inside(p)) {
        return Err(format!("{at}: {bad} is not a path inside evals/"));
    }
    Ok(())
}

/// What one run of a case showed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Observed {
    /// The final answer.
    pub output: String,
    /// Every tool call: name and parsed arguments.
    pub calls: Vec<(String, Value)>,
    /// The judge's verdict on the rubric (`None`: no judge ran) and why.
    pub judged: Option<(bool, String)>,
}

/// Why a run of `case` (for skill `skill`) failed; empty when it passed.
#[must_use]
pub fn failures(skill: &str, case: &Case, seen: &Observed) -> Vec<String> {
    let mut out = Vec::new();
    let loaded = seen
        .calls
        .iter()
        .any(|(name, args)| name == "skill_view" && args["name"].as_str() == Some(skill));
    match case.expect_skill_loaded {
        Some(true) if !loaded => out.push("skill not loaded".to_owned()),
        Some(false) if loaded => out.push("skill loaded but should not be".to_owned()),
        _ => {}
    }
    for tool in &case.expect_tools {
        if !seen.calls.iter().any(|(name, _)| name == tool) {
            out.push(format!("tool {tool} not called"));
        }
    }
    if let Some(text) = &case.expect_output_contains
        && !seen.output.contains(text.as_str())
    {
        out.push(format!("output does not contain {text:?}"));
    }
    if let Some(pattern) = &case.expect_output_regex
        && !Regex::new(pattern).is_ok_and(|re| re.is_match(&seen.output))
    {
        out.push(format!("output does not match /{pattern}/"));
    }
    match (&case.judge, &seen.judged) {
        (Some(_), Some((false, why))) => out.push(format!("judge: {why}")),
        (Some(_), None) => out.push("judge: no verdict".to_owned()),
        _ => {}
    }
    out
}

/// The system prompt of a rubric check.
pub const JUDGE_SYSTEM: &str = "You grade an assistant's answer against a rubric. Reply with \
PASS or FAIL on the first line, then one short sentence saying why.";

/// The judge's verdict in `reply`: PASS or FAIL on its first line.
#[must_use]
pub fn verdict(reply: &str) -> (bool, String) {
    let mut lines = reply.trim().lines();
    let first = lines.next().unwrap_or_default().trim().to_uppercase();
    let why = lines.collect::<Vec<_>>().join(" ").trim().to_owned();
    let why = if why.is_empty() { first.clone() } else { why };
    (first.starts_with("PASS"), why)
}

/// A pass rate: cases × repeats that passed, of all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Rate {
    /// Passed runs.
    pub passed: u32,
    /// All runs.
    pub total: u32,
}

impl Rate {
    /// The rate as a fraction (1 when nothing ran).
    #[must_use]
    pub fn fraction(self) -> f64 {
        if self.total == 0 {
            1.0
        } else {
            f64::from(self.passed) / f64::from(self.total)
        }
    }

    /// Whether `self` is a lower rate than `before`.
    #[must_use]
    pub fn fell_from(self, before: Self) -> bool {
        // Cross-multiplied: exact for any totals.
        u64::from(self.passed) * u64::from(before.total)
            < u64::from(before.passed) * u64::from(self.total)
    }
}

impl std::fmt::Display for Rate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.passed, self.total)
    }
}

/// Runs a skill's evals for an eval-gated edit (the binary implements it).
pub trait Gate: Send + Sync {
    /// The pass rate of the skill in `skill_dir` named `name` over
    /// `repeat` runs per case; `Ok(None)` when it has no evals.
    ///
    /// # Errors
    ///
    /// Why the evals could not run.
    fn rate(&self, skill_dir: &Path, name: &str, repeat: u32) -> Result<Option<Rate>, String>;
}

/// The refusal of an edit that lowered the pass rate.
#[must_use]
pub fn rejected(before: Rate, after: Rate) -> String {
    format!("edit rejected: eval pass rate fell from {before} to {after}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case() -> Case {
        Case {
            name: "c".into(),
            prompt: "sum column B of data.csv".into(),
            setup_files: Vec::new(),
            expect_skill_loaded: Some(true),
            expect_tools: vec!["terminal".into()],
            expect_output_contains: Some("42".into()),
            expect_output_regex: Some(r"total: \d+".into()),
            judge: None,
            mock_script: None,
        }
    }

    #[test]
    fn a_run_without_skill_view_fails_with_skill_not_loaded() {
        let seen = Observed {
            output: "total: 42".into(),
            calls: vec![("terminal".into(), Value::Null)],
            judged: None,
        };
        assert_eq!(failures("csv-tools", &case(), &seen), ["skill not loaded"]);
        let loaded = Observed {
            calls: vec![
                (
                    "skill_view".into(),
                    serde_json::json!({"name": "csv-tools"}),
                ),
                ("terminal".into(), Value::Null),
            ],
            ..seen
        };
        assert!(failures("csv-tools", &case(), &loaded).is_empty());
    }

    #[test]
    fn rates_compare_exactly_and_render_as_counts() {
        let all = Rate {
            passed: 3,
            total: 3,
        };
        let one = Rate {
            passed: 1,
            total: 3,
        };
        assert!(one.fell_from(all));
        assert!(!all.fell_from(one));
        assert!(
            !Rate {
                passed: 2,
                total: 2
            }
            .fell_from(all)
        );
        assert_eq!(
            rejected(all, one),
            "edit rejected: eval pass rate fell from 3/3 to 1/3"
        );
    }

    #[test]
    fn paths_must_stay_inside_evals() {
        assert!(inside("data.csv") && inside("mock/a.toml"));
        assert!(!inside("../x") && !inside("/etc/passwd") && !inside(""));
    }

    #[test]
    fn the_judge_reply_is_read_from_its_first_line() {
        assert_eq!(
            verdict("PASS\nstates the sum"),
            (true, "states the sum".into())
        );
        assert!(!verdict("fail").0);
    }
}
