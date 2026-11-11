//! Planning-only turns (spec: cli, Planning-only turn): while `/plan`
//! runs, the session's dispatcher refuses every call of a tool whose
//! effect is `workspace` or `external`, except a `write_file` of the
//! turn's plan file `<workspace>/.prometheus/plans/<slug>.md`. The tool
//! definitions sent to the provider do not change (the prompt cache stays
//! warm); the model only learns of the restriction from the refusals and
//! the plan request itself.

use std::path::{Component, Path, PathBuf};

use pmt_agent::turn::Declared;
use pmt_agent::turn::model::RequestedCall;
use pmt_agent::turn::tools::ToolOutcome;
use pmt_tools::ToolResult;

/// What a refused call answers.
pub const REFUSAL: &str = "not allowed in /plan";

/// Agent-loop tools that change nothing outside the conversation.
const CONVERSATION_ONLY: &[&str] = &["todo", "clarify", "session_search"];

/// The directory plans go to, under the workspace.
pub const PLANS_DIR: &str = ".prometheus/plans";

/// One planning turn's restriction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    /// The only file the turn may write (absolute).
    pub file: PathBuf,
    /// The session's working directory, which relative paths are taken
    /// from.
    pub cwd: PathBuf,
}

impl Plan {
    /// The plan of `task` in `cwd`: `<cwd>/.prometheus/plans/<slug>.md`,
    /// the slug deduplicated with `-2`, `-3`... against existing plans.
    #[must_use]
    pub fn new(cwd: &Path, task: &str) -> Self {
        let dir = cwd.join(PLANS_DIR);
        let base = slug(task);
        let mut file = dir.join(format!("{base}.md"));
        let mut n = 2;
        while file.exists() {
            file = dir.join(format!("{base}-{n}.md"));
            n += 1;
        }
        Self {
            file,
            cwd: cwd.to_path_buf(),
        }
    }

    /// The plan file relative to the workspace (`.prometheus/plans/x.md`).
    #[must_use]
    pub fn relative(&self) -> String {
        self.file
            .strip_prefix(&self.cwd)
            .unwrap_or(&self.file)
            .to_string_lossy()
            .replace('\\', "/")
    }

    /// Whether `call` may run during the planning turn (`declared`: its
    /// tool's effect, when the dispatcher knows the tool).
    #[must_use]
    pub fn admits(&self, call: &RequestedCall, declared: Option<&Declared>) -> bool {
        if declared.is_some_and(|d| d.effect == "none") || CONVERSATION_ONLY.contains(&&*call.name)
        {
            return true;
        }
        call.name == "write_file" && self.is_plan_file(&call.arguments)
    }

    fn is_plan_file(&self, arguments: &str) -> bool {
        let Ok(arguments) = serde_json::from_str::<serde_json::Value>(arguments) else {
            return false;
        };
        let Some(path) = arguments["path"].as_str() else {
            return false;
        };
        let path = Path::new(path);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        };
        normalize(&path) == normalize(&self.file)
    }
}

/// The refusal outcome of a call outside the plan.
#[must_use]
pub fn refused() -> ToolOutcome {
    ToolOutcome {
        result: ToolResult::error(REFUSAL),
        duration_ms: 0,
    }
}

/// `path` with `.` and `..` resolved lexically (the plan file need not
/// exist yet, so it cannot be canonicalized).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The file name of a task: lowercase words joined by `-`, at most 60
/// characters; `plan` when nothing is left.
#[must_use]
pub fn slug(task: &str) -> String {
    let mut slug = String::new();
    for word in task
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        let word = word.to_lowercase();
        if slug.len() + word.len() + 1 > 60 {
            break;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(&word);
    }
    if slug.is_empty() {
        "plan".to_owned()
    } else {
        slug
    }
}

/// The user message of a planning turn.
#[must_use]
pub fn request(plan: &Plan, task: &str) -> String {
    let task = if task.trim().is_empty() {
        "the task of the conversation so far".to_owned()
    } else {
        format!("this task: {task}")
    };
    format!(
        "Plan {task}\n\nThis is a planning-only turn: read and search as you need, but change nothing. \
Write the plan as Markdown with write_file to `{}` (the only file you may write now); \
every other change is refused with `{REFUSAL}`.",
        plan.relative()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, arguments: &str) -> RequestedCall {
        RequestedCall {
            id: "c1".into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    fn declared(effect: &str) -> Declared {
        Declared {
            effect: effect.into(),
            idempotent: false,
        }
    }

    #[test]
    fn only_reads_and_the_plan_file_pass() {
        let plan = Plan {
            file: PathBuf::from("/w/.prometheus/plans/add-caching.md"),
            cwd: PathBuf::from("/w"),
        };
        let write = |path: &str| call("write_file", &format!("{{\"path\":\"{path}\"}}"));
        let ws = declared("workspace");
        assert!(plan.admits(&call("read_file", "{}"), Some(&declared("none"))));
        assert!(plan.admits(&write(".prometheus/plans/add-caching.md"), Some(&ws)));
        assert!(plan.admits(
            &write("./src/../.prometheus/plans/add-caching.md"),
            Some(&ws)
        ));
        assert!(!plan.admits(&write("src/main.rs"), Some(&ws)));
        assert!(!plan.admits(&call("terminal", "{}"), Some(&declared("external"))));
        assert!(!plan.admits(&call("patch", "{}"), Some(&ws)));
    }

    #[test]
    fn slugs_come_from_the_task_and_are_deduplicated() {
        assert_eq!(slug("Add caching!"), "add-caching");
        assert_eq!(slug("  "), "plan");
        let dir =
            tempfile::tempdir_in(concat!(env!("CARGO_MANIFEST_DIR"), "/../../target")).unwrap();
        let plans = dir.path().join(PLANS_DIR);
        std::fs::create_dir_all(&plans).unwrap();
        std::fs::write(plans.join("add-caching.md"), "x").unwrap();
        std::fs::write(plans.join("add-caching-2.md"), "x").unwrap();
        let plan = Plan::new(dir.path(), "add caching");
        assert_eq!(plan.relative(), ".prometheus/plans/add-caching-3.md");
    }
}
