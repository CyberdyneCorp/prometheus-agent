//! The bundled `smoke` eval suite (spec: replay-and-evaluation, Built-in
//! smoke suite): offline tasks on the mock provider, one per core
//! capability, each with an `exact` grader. A task directory holds
//! `task.toml` (`prompt`, `files`), `files/` (its workspace), `mock.toml`
//! (the mock script the task's model plays), `grader/grader.toml`
//! (`type = "exact"`, `of = "answer" | "exit" | "file:<path>"`,
//! `expect`) and an optional `run.toml` (`set` overrides, `then`
//! follow-up prompts that resume the session, `config` for the profile,
//! `interrupt_after_turn_start`, and `requires`: a harness capability a
//! platform may lack, which skips and lists the task there).
//!
//! The binary runs it itself (`prometheus smoke-suite`, [`runner`]), so
//! every CI target can, including Termux where there is no cargo:
//! `cargo xtask smoke` on the Tier 1 targets, the offline smoke run on the
//! Tier 2 ones. M3's `prometheus eval run smoke` scores the same files.

pub mod runner;

/// One bundled file: its path inside the suite and its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct File {
    /// The path below the suite directory (`tasks/<id>/task.toml`).
    pub path: &'static str,
    /// The contents.
    pub text: &'static str,
}

macro_rules! file {
    ($path:literal) => {
        File {
            path: $path,
            text: include_str!(concat!("../../evals/smoke/", $path)),
        }
    };
}

/// Every file of the suite.
pub const FILES: &[File] = &[
    file!("suite.toml"),
    file!("tasks/approval_denial/files/README.md"),
    file!("tasks/approval_denial/grader/grader.toml"),
    file!("tasks/approval_denial/mock.toml"),
    file!("tasks/approval_denial/run.toml"),
    file!("tasks/approval_denial/task.toml"),
    file!("tasks/budget_refusal/files/README.md"),
    file!("tasks/budget_refusal/grader/grader.toml"),
    file!("tasks/budget_refusal/mock.toml"),
    file!("tasks/budget_refusal/run.toml"),
    file!("tasks/budget_refusal/task.toml"),
    file!("tasks/clarify/files/README.md"),
    file!("tasks/clarify/grader/grader.toml"),
    file!("tasks/clarify/mock.toml"),
    file!("tasks/clarify/task.toml"),
    file!("tasks/compression/files/a.md"),
    file!("tasks/compression/files/b.md"),
    file!("tasks/compression/grader/grader.toml"),
    file!("tasks/compression/mock.toml"),
    file!("tasks/compression/run.toml"),
    file!("tasks/compression/task.toml"),
    file!("tasks/delegation/files/README.md"),
    file!("tasks/delegation/grader/grader.toml"),
    file!("tasks/delegation/mock.toml"),
    file!("tasks/delegation/task.toml"),
    file!("tasks/error_recovery/files/README.md"),
    file!("tasks/error_recovery/grader/grader.toml"),
    file!("tasks/error_recovery/mock.toml"),
    file!("tasks/error_recovery/task.toml"),
    file!("tasks/file_edit/files/README.md"),
    file!("tasks/file_edit/grader/grader.toml"),
    file!("tasks/file_edit/mock.toml"),
    file!("tasks/file_edit/task.toml"),
    file!("tasks/interrupt/files/README.md"),
    file!("tasks/interrupt/grader/grader.toml"),
    file!("tasks/interrupt/mock.toml"),
    file!("tasks/interrupt/run.toml"),
    file!("tasks/interrupt/task.toml"),
    file!("tasks/memory_write/files/README.md"),
    file!("tasks/memory_write/grader/grader.toml"),
    file!("tasks/memory_write/mock.toml"),
    file!("tasks/memory_write/task.toml"),
    file!("tasks/parallel_tools/files/OWNERS.md"),
    file!("tasks/parallel_tools/files/README.md"),
    file!("tasks/parallel_tools/grader/grader.toml"),
    file!("tasks/parallel_tools/mock.toml"),
    file!("tasks/parallel_tools/task.toml"),
    file!("tasks/skill_invocation/files/README.md"),
    file!("tasks/skill_invocation/grader/grader.toml"),
    file!("tasks/skill_invocation/mock.toml"),
    file!("tasks/skill_invocation/task.toml"),
    file!("tasks/terminal_steps/files/README.md"),
    file!("tasks/terminal_steps/grader/grader.toml"),
    file!("tasks/terminal_steps/mock.toml"),
    file!("tasks/terminal_steps/task.toml"),
    file!("tasks/tool_call/files/README.md"),
    file!("tasks/tool_call/grader/grader.toml"),
    file!("tasks/tool_call/mock.toml"),
    file!("tasks/tool_call/task.toml"),
];

/// The ids of the suite's tasks, sorted.
#[must_use]
pub fn task_ids() -> Vec<&'static str> {
    let mut ids: Vec<&str> = FILES
        .iter()
        .filter_map(|f| f.path.strip_prefix("tasks/")?.strip_suffix("/task.toml"))
        .collect();
    ids.sort_unstable();
    ids
}

/// The text of `path` in the suite.
#[must_use]
pub fn file(path: &str) -> Option<&'static str> {
    FILES.iter().find(|f| f.path == path).map(|f| f.text)
}
