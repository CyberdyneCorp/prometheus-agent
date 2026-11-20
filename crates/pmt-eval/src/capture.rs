//! Session-to-task capture (spec: replay-and-evaluation, Session-to-task
//! capture): a real session becomes a regression task — its first user
//! message the prompt, its recorded workspace the task files, and a
//! grader the user supplies. A file whose contents the recording lacks
//! refuses the capture: a task never declares files it cannot provide.

use std::path::{Path, PathBuf};

use pmt_core::error::{Error, codes};

use crate::grader::{GraderKind, GraderSpec};
use crate::suite::{SuiteMeta, TaskMeta};

/// The grader the user supplies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    /// `--expect TEXT`: an `exact` grader on the answer.
    Exact(String),
    /// `--expect-regex RE`.
    Regex(String),
    /// `--grader-script PATH`: copied into `grader/`.
    Script(PathBuf),
}

/// A task to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capture {
    /// The suite (created when it has no `suite.toml`).
    pub suite_dir: PathBuf,
    /// The new task's id.
    pub task_id: String,
    /// The session's first user message.
    pub prompt: String,
    /// Workspace files: path and contents (`None` when not recorded).
    pub files: Vec<(String, Option<Vec<u8>>)>,
    /// The grader.
    pub expect: Expect,
}

fn refused(message: String) -> Error {
    Error::new(&codes::EVAL_CAPTURE_REFUSED, message)
}

fn io(path: &Path, e: &std::io::Error) -> Error {
    Error::io(&codes::EVAL_IO, path, e)
}

/// The interpreter of a grader script, by extension.
fn script_command(name: &str) -> Vec<String> {
    match Path::new(name).extension().and_then(|e| e.to_str()) {
        Some("py") => vec!["python3".to_owned(), name.to_owned()],
        Some("sh") => vec!["sh".to_owned(), name.to_owned()],
        _ => vec![format!("./{name}")],
    }
}

fn grader(expect: &Expect, dir: &Path) -> Result<GraderSpec, Error> {
    let mut spec = GraderSpec {
        kind: GraderKind::Exact,
        of: None,
        expect: None,
        pattern: None,
        command: Vec::new(),
        rubric: None,
        rubric_file: None,
        model: None,
    };
    match expect {
        Expect::Exact(text) => spec.expect = Some(text.clone()),
        Expect::Regex(re) => {
            regex::Regex::new(re).map_err(|e| refused(format!("--expect-regex: {e}")))?;
            spec.kind = GraderKind::Regex;
            spec.pattern = Some(re.clone());
        }
        Expect::Script(path) => {
            let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
                refused(format!(
                    "--grader-script {} has no file name",
                    path.display()
                ))
            })?;
            std::fs::copy(path, dir.join(name)).map_err(|e| io(path, &e))?;
            spec.kind = GraderKind::Script;
            spec.command = script_command(name);
        }
    }
    Ok(spec)
}

/// Writes the task; returns its directory.
///
/// # Errors
///
/// `eval.capture_refused` (65) for a file without recorded contents, an
/// unsafe file path or an existing task id; `eval.io`.
pub fn write(capture: &Capture) -> Result<PathBuf, Error> {
    if let Some((path, _)) = capture.files.iter().find(|(_, bytes)| bytes.is_none()) {
        return Err(refused(format!(
            "the session's recording has no workspace contents (no {path}); export it with --include-workspace or capture a session whose workspace is still on disk"
        )));
    }
    if let Some((path, _)) = capture
        .files
        .iter()
        .find(|(p, _)| !crate::suite::is_relative_inside(p))
    {
        return Err(refused(format!("workspace path {path} leaves the task")));
    }
    let task = capture.suite_dir.join("tasks").join(&capture.task_id);
    if task.exists() {
        return Err(refused(format!("task {} exists already", task.display())));
    }
    ensure_suite(&capture.suite_dir)?;
    let grader_dir = task.join("grader");
    std::fs::create_dir_all(&grader_dir).map_err(|e| io(&grader_dir, &e))?;
    let spec = grader(&capture.expect, &grader_dir)?;
    for (path, bytes) in &capture.files {
        let target = task.join("files").join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io(parent, &e))?;
        }
        std::fs::write(&target, bytes.as_deref().unwrap_or_default())
            .map_err(|e| io(&target, &e))?;
    }
    let meta = TaskMeta {
        prompt: capture.prompt.clone(),
        domain: None,
        timeout_secs: None,
        toolsets: None,
        capabilities: Vec::new(),
        files: capture.files.iter().map(|(p, _)| p.clone()).collect(),
    };
    write_toml(&task.join("task.toml"), &meta)?;
    write_toml(&grader_dir.join("grader.toml"), &spec)?;
    Ok(task)
}

fn ensure_suite(dir: &Path) -> Result<(), Error> {
    let file = dir.join("suite.toml");
    if file.is_file() {
        return Ok(());
    }
    let name = dir.file_name().map_or_else(
        || "captured".to_owned(),
        |n| n.to_string_lossy().into_owned(),
    );
    let meta = SuiteMeta {
        name,
        version: "1".to_owned(),
        domain: "general".to_owned(),
        default_timeout_secs: crate::suite::DEFAULT_TIMEOUT_SECS,
        default_toolsets: Vec::new(),
        budget_usd: None,
        leak_globs: Vec::new(),
    };
    std::fs::create_dir_all(dir).map_err(|e| io(dir, &e))?;
    write_toml(&file, &meta)
}

fn write_toml(path: &Path, value: &impl serde::Serialize) -> Result<(), Error> {
    let text = toml::to_string(value).map_err(|e| Error::new(&codes::EVAL_IO, e.to_string()))?;
    std::fs::write(path, text).map_err(|e| io(path, &e))
}
