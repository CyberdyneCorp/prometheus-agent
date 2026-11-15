//! The eval suite format (spec: replay-and-evaluation, Eval suite format):
//! a directory with `suite.toml` and `tasks/<task_id>/`, each task with
//! `task.toml`, its `files/` and an optional `grader/`.
//!
//! Offline suites (the bundled `smoke` suite) may also give a task a
//! `mock.toml` (the mock-provider script its model plays when the run's
//! provider is `mock`) and a `run.toml` ([`RunSpec`]: configuration
//! overrides, follow-up prompts, an interrupt, a required capability).
//!
//! [`Suite::load`] validates everything and stops at the first invalid
//! file, naming it relative to the suite (`tasks/<id>/task.toml`).

use std::path::{Component, Path, PathBuf};

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

use crate::grader::GraderSpec;

/// The default `default_timeout_secs`.
pub const DEFAULT_TIMEOUT_SECS: u64 = 600;

/// `suite.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuiteMeta {
    /// The suite's name.
    pub name: String,
    /// Its version (a resumed run must match it).
    pub version: String,
    /// The experience-library domain of its tasks.
    pub domain: String,
    /// A task's time limit unless it sets its own.
    #[serde(default = "default_timeout")]
    pub default_timeout_secs: u64,
    /// A task's toolsets unless it sets its own (empty: the profile's).
    #[serde(default)]
    pub default_toolsets: Vec<String>,
    /// The run's spend cap unless `--budget-usd` gives one.
    #[serde(default)]
    pub budget_usd: Option<f64>,
    /// Globs of answer material: a task touching a matching path is
    /// contaminated (spec: Answer-free boundary).
    #[serde(default)]
    pub leak_globs: Vec<String>,
}

const fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_SECS
}

/// `tasks/<id>/task.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskMeta {
    /// What the agent is asked.
    pub prompt: String,
    /// Overrides the suite's domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    /// Overrides the suite's time limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Overrides the suite's toolsets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toolsets: Option<Vec<String>>,
    /// Extra capabilities granted to the task session.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// Paths below `files/` copied into the task workspace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
}

/// `tasks/<id>/run.toml` (offline suites).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSpec {
    /// Configuration overrides, `key=value` as for `--set`.
    #[serde(default)]
    pub set: Vec<String>,
    /// Follow-up prompts, each a further turn of the same session.
    #[serde(default)]
    pub then: Vec<String>,
    /// A TOML fragment merged into the task's configuration.
    #[serde(default)]
    pub config: String,
    /// The task is interrupted once its first turn has started.
    #[serde(default)]
    pub interrupt_after_turn_start: bool,
    /// A harness capability the platform may lack (the task is skipped).
    #[serde(default)]
    pub requires: Option<String>,
}

/// One task of a suite.
#[derive(Debug, Clone, PartialEq)]
pub struct Task {
    /// Its id (the directory name).
    pub id: String,
    /// Its directory.
    pub dir: PathBuf,
    /// `task.toml`.
    pub meta: TaskMeta,
    /// `run.toml`, when present.
    pub run: RunSpec,
    /// `mock.toml`, when present.
    pub mock_script: Option<PathBuf>,
    /// `grader/grader.toml`, when the task has a grader.
    pub grader: Option<GraderSpec>,
}

impl Task {
    /// The task's grader directory (never inside any workspace).
    #[must_use]
    pub fn grader_dir(&self) -> PathBuf {
        self.dir.join("grader")
    }

    /// The file `name` of `files/`.
    #[must_use]
    pub fn file(&self, name: &str) -> PathBuf {
        self.dir.join("files").join(name)
    }
}

/// A loaded, valid suite.
#[derive(Debug, Clone, PartialEq)]
pub struct Suite {
    /// The suite directory.
    pub dir: PathBuf,
    /// `suite.toml`.
    pub meta: SuiteMeta,
    /// Its tasks, sorted by id.
    pub tasks: Vec<Task>,
}

/// The error naming the first invalid file.
fn invalid(file: &str, problem: impl std::fmt::Display) -> Error {
    Error::new(&codes::EVAL_INVALID_SUITE, format!("{file}: {problem}"))
}

fn read(dir: &Path, file: &str) -> Result<String, Error> {
    std::fs::read_to_string(dir.join(file)).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => invalid(file, "missing"),
        _ => invalid(file, e),
    })
}

/// Parses `text` as `T`, first naming every required key that is absent.
fn parse<T: serde::de::DeserializeOwned>(
    file: &str,
    text: &str,
    required: &[&str],
) -> Result<T, Error> {
    let table: toml::Table = text.parse().map_err(|e| invalid(file, e))?;
    if let Some(key) = required.iter().find(|k| !table.contains_key(**k)) {
        return Err(invalid(file, format!("missing key '{key}'")));
    }
    T::deserialize(toml::Value::Table(table)).map_err(|e| invalid(file, e.to_string().trim()))
}

/// Whether `path` stays below the directory it is relative to.
#[must_use]
pub fn is_relative_inside(path: &str) -> bool {
    let path = Path::new(path);
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

impl Suite {
    /// Loads and validates the suite at `dir`.
    ///
    /// # Errors
    ///
    /// `eval.suite_not_found` (66) without a `suite.toml`;
    /// `eval.invalid_suite` (65) naming the first invalid file.
    pub fn load(dir: &Path) -> Result<Self, Error> {
        if !dir.join("suite.toml").is_file() {
            return Err(Error::new(
                &codes::EVAL_SUITE_NOT_FOUND,
                format!("{} is not an eval suite (no suite.toml)", dir.display()),
            ));
        }
        let meta: SuiteMeta = parse(
            "suite.toml",
            &read(dir, "suite.toml")?,
            &["name", "version", "domain"],
        )?;
        if let Some(glob) = meta
            .leak_globs
            .iter()
            .find(|g| glob::Pattern::new(g).is_err())
        {
            return Err(invalid("suite.toml", format!("invalid leak glob '{glob}'")));
        }
        let mut ids = task_ids(dir)?;
        ids.sort();
        let tasks = ids
            .iter()
            .map(|id| load_task(dir, id))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            dir: dir.to_path_buf(),
            meta,
            tasks,
        })
    }

    /// The tasks named in `selection` (all when empty), in suite order.
    ///
    /// # Errors
    ///
    /// `cli.usage` (2) naming a task the suite does not have.
    pub fn select(&self, selection: &[String]) -> Result<Vec<&Task>, Error> {
        if let Some(missing) = selection
            .iter()
            .find(|id| !self.tasks.iter().any(|t| &t.id == *id))
        {
            return Err(Error::new(
                &codes::CLI_USAGE,
                format!("suite {} has no task '{missing}'", self.meta.name),
            ));
        }
        Ok(self
            .tasks
            .iter()
            .filter(|t| selection.is_empty() || selection.contains(&t.id))
            .collect())
    }

    /// The task `id`.
    #[must_use]
    pub fn task(&self, id: &str) -> Option<&Task> {
        self.tasks.iter().find(|t| t.id == id)
    }
}

fn task_ids(dir: &Path) -> Result<Vec<String>, Error> {
    let tasks = dir.join("tasks");
    let Ok(entries) = std::fs::read_dir(&tasks) else {
        return Err(invalid("tasks/", "missing"));
    };
    let mut ids = Vec::new();
    for entry in entries.flatten() {
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            ids.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    Ok(ids)
}

fn load_task(suite: &Path, id: &str) -> Result<Task, Error> {
    let dir = suite.join("tasks").join(id);
    let task_file = format!("tasks/{id}/task.toml");
    let meta: TaskMeta = parse(&task_file, &read(suite, &task_file)?, &["prompt"])?;
    if meta.prompt.trim().is_empty() {
        return Err(invalid(&task_file, "'prompt' is empty"));
    }
    for name in &meta.files {
        if !is_relative_inside(name) || !dir.join("files").join(name).is_file() {
            return Err(invalid(
                &task_file,
                format!("file '{name}' is not in files/"),
            ));
        }
    }
    let run_file = format!("tasks/{id}/run.toml");
    let run = if suite.join(&run_file).is_file() {
        parse(&run_file, &read(suite, &run_file)?, &[])?
    } else {
        RunSpec::default()
    };
    let mock = dir.join("mock.toml");
    let grader_file = format!("tasks/{id}/grader/grader.toml");
    let grader = if dir.join("grader").is_dir() {
        let spec: GraderSpec = parse(&grader_file, &read(suite, &grader_file)?, &["type"])?;
        spec.check().map_err(|p| invalid(&grader_file, p))?;
        Some(spec)
    } else {
        None
    };
    Ok(Task {
        id: id.to_owned(),
        dir: dir.clone(),
        meta,
        run,
        mock_script: mock.is_file().then_some(mock),
        grader,
    })
}

/// The suites under `dir` (`<profile>/evals/`): each subdirectory with a
/// `suite.toml`, by name.
#[must_use]
pub fn discover(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("suite.toml").is_file())
        .collect();
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_files_never_leave_the_task() {
        assert!(is_relative_inside("a/b.md"));
        assert!(!is_relative_inside("../grader/answer.txt"));
        assert!(!is_relative_inside("/etc/passwd"));
        assert!(!is_relative_inside(""));
    }
}
