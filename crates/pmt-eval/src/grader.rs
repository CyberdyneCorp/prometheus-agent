//! Grader specs and scoring (spec: replay-and-evaluation, Post-hoc
//! judging): `grader/grader.toml` names a type — `exact` (normalised
//! string equality), `regex`, `script` (a sandboxed command given the
//! deliverables that prints `{"n_passed", "n_criteria"}`) or `llm` (a
//! judge model with the rubric) — and what it reads (`of`): the final
//! `answer` (default), the turn's `exit` status or a deliverable
//! `file:<path>`.
//!
//! Only the judge phase reads this module's inputs; the script and model
//! sides are [`Ports`] the binary provides.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use serde::{Deserialize, Serialize};

/// A boxed future.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The grader types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GraderKind {
    /// Normalised string equality.
    Exact,
    /// A regular expression that must match.
    Regex,
    /// A sandboxed command printing the criteria counts.
    Script,
    /// A judge model with a rubric.
    Llm,
}

/// `grader/grader.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraderSpec {
    /// The type.
    #[serde(rename = "type")]
    pub kind: GraderKind,
    /// What is graded: `answer` (default), `exit` or `file:<path>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub of: Option<String>,
    /// `exact`: the expected text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<String>,
    /// `regex`: the pattern.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// `script`: the command (argv; relative paths are below `grader/`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// `llm`: the rubric, or `rubric_file` below `grader/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rubric: Option<String>,
    /// `llm`: a file holding the rubric.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rubric_file: Option<String>,
    /// `llm`: the judge model (`provider/model`); the profile's judge
    /// route otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// What a grader reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The final answer.
    Answer,
    /// The exit status of the task's turn.
    Exit,
    /// A deliverable file.
    File(String),
}

impl GraderSpec {
    /// Checks the fields the type needs.
    ///
    /// # Errors
    ///
    /// The problem, for the invalid-suite message.
    pub fn check(&self) -> Result<(), String> {
        self.target()?;
        match self.kind {
            GraderKind::Exact if self.expect.is_none() => Err("exact needs 'expect'".to_owned()),
            GraderKind::Regex => match &self.pattern {
                None => Err("regex needs 'pattern'".to_owned()),
                Some(p) => regex::Regex::new(p)
                    .map(|_| ())
                    .map_err(|e| format!("invalid pattern: {e}")),
            },
            GraderKind::Script if self.command.is_empty() => {
                Err("script needs 'command'".to_owned())
            }
            GraderKind::Llm if self.rubric.is_none() && self.rubric_file.is_none() => {
                Err("llm needs 'rubric' or 'rubric_file'".to_owned())
            }
            _ => Ok(()),
        }
    }

    /// What the grader reads.
    ///
    /// # Errors
    ///
    /// An unknown `of`.
    pub fn target(&self) -> Result<Target, String> {
        match self.of.as_deref() {
            None | Some("answer") => Ok(Target::Answer),
            Some("exit") => Ok(Target::Exit),
            Some(other) => match other.strip_prefix("file:") {
                Some(path) if crate::suite::is_relative_inside(path) => {
                    Ok(Target::File(path.to_owned()))
                }
                _ => Err(format!("unknown 'of' target '{other}'")),
            },
        }
    }
}

/// A task's graded outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Score {
    /// Criteria met.
    pub n_passed: u32,
    /// Criteria in all.
    pub n_criteria: u32,
}

/// What an executed task produced, as the judge sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deliverables {
    /// `deliverables/` of the task's run directory.
    pub dir: PathBuf,
    /// The final answer.
    pub answer: String,
    /// The turn's exit status.
    pub exit_status: Option<u8>,
}

impl Deliverables {
    /// The text `target` names.
    #[must_use]
    pub fn text(&self, target: &Target) -> String {
        match target {
            Target::Answer => self.answer.clone(),
            Target::Exit => self.exit_status.map(|c| c.to_string()).unwrap_or_default(),
            Target::File(path) => {
                std::fs::read_to_string(self.dir.join("files").join(path)).unwrap_or_default()
            }
        }
    }
}

/// The judge's script and model sides.
pub trait Ports: Sync {
    /// Runs `command` sandboxed in `grader_dir` with read access to
    /// `deliverables`; returns its standard output.
    fn script<'a>(
        &'a self,
        command: &'a [String],
        grader_dir: &'a Path,
        deliverables: &'a Path,
    ) -> BoxFuture<'a, Result<String, String>>;

    /// Asks the judge model (`model`, else the default judge) to grade
    /// `deliverable` against `rubric`; returns its reply.
    fn llm<'a>(
        &'a self,
        model: Option<&'a str>,
        rubric: &'a str,
        deliverable: &'a str,
    ) -> BoxFuture<'a, Result<String, String>>;
}

/// Surrounding whitespace, line endings and trailing spaces aside.
#[must_use]
pub fn normalise(text: &str) -> String {
    text.replace("\r\n", "\n")
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

/// `{"n_passed", "n_criteria"}` from the last line of `output` that holds
/// such an object.
///
/// # Errors
///
/// Why none was found.
pub fn parse_counts(output: &str) -> Result<Score, String> {
    output
        .lines()
        .rev()
        .find_map(|line| {
            let start = line.find('{')?;
            let end = line.rfind('}')?;
            serde_json::from_str::<Score>(line.get(start..=end)?).ok()
        })
        .ok_or_else(|| "no {\"n_passed\", \"n_criteria\"} in the grader's output".to_owned())
}

/// The instruction given to an `llm` judge.
pub const JUDGE_SYSTEM: &str = "You grade one deliverable of an evaluation task against a rubric. Count the rubric's criteria and how many the deliverable meets. Reply with one JSON object on its own line: {\"n_passed\": <int>, \"n_criteria\": <int>}.";

/// Scores `deliverables` with `spec` (in `grader_dir`).
///
/// # Errors
///
/// Why the task is ungraded: the grader failed or had no criteria.
pub async fn grade(
    spec: &GraderSpec,
    grader_dir: &Path,
    deliverables: &Deliverables,
    ports: &dyn Ports,
) -> Result<Score, String> {
    let target = spec.target()?;
    let score = match spec.kind {
        GraderKind::Exact => {
            let expect = spec.expect.as_deref().unwrap_or_default();
            pass(normalise(&deliverables.text(&target)) == normalise(expect))
        }
        GraderKind::Regex => {
            let re = regex::Regex::new(spec.pattern.as_deref().unwrap_or_default())
                .map_err(|e| e.to_string())?;
            pass(re.is_match(&deliverables.text(&target)))
        }
        GraderKind::Script => {
            let out = ports
                .script(&spec.command, grader_dir, &deliverables.dir)
                .await?;
            parse_counts(&out)?
        }
        GraderKind::Llm => {
            let rubric = rubric(spec, grader_dir)?;
            let reply = ports
                .llm(spec.model.as_deref(), &rubric, &deliverables.text(&target))
                .await?;
            parse_counts(&reply)?
        }
    };
    if score.n_criteria == 0 {
        return Err("the grader found zero criteria".to_owned());
    }
    if score.n_passed > score.n_criteria {
        return Err(format!(
            "the grader reported {} of {} criteria",
            score.n_passed, score.n_criteria
        ));
    }
    Ok(score)
}

fn pass(ok: bool) -> Score {
    Score {
        n_passed: u32::from(ok),
        n_criteria: 1,
    }
}

fn rubric(spec: &GraderSpec, grader_dir: &Path) -> Result<String, String> {
    if let Some(text) = &spec.rubric {
        return Ok(text.clone());
    }
    let file = spec.rubric_file.as_deref().unwrap_or_default();
    if !crate::suite::is_relative_inside(file) {
        return Err(format!("rubric_file '{file}' is outside grader/"));
    }
    std::fs::read_to_string(grader_dir.join(file)).map_err(|e| format!("rubric_file: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_come_from_the_last_json_line() {
        assert_eq!(
            parse_counts("checking\n{\"n_passed\": 2, \"n_criteria\": 3}\n"),
            Ok(Score {
                n_passed: 2,
                n_criteria: 3
            })
        );
        assert!(parse_counts("no json").is_err());
        assert_eq!(normalise("  a  \r\nb \n\n"), "a\nb");
    }
}
