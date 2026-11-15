//! Evidence directories (spec: harness-evolution, Evidence context):
//! what an improver session reads and the harness it edits, laid out
//! under `context/` of its private working directory.
//!
//! ```text
//! context/task.md, source/            the frozen task
//! context/rollouts/<label>.md         base rollout trajectories
//! context/deliverables/<label>/       final.md, workspace.diff, external_calls.json, images
//! context/harness/                    the tree to edit
//! context/pool/<label>/               harness/, harness.diff, rollout.md, produced.md
//! context/decisions/<label>.md        each candidate's decision notes
//! context/library/                    the domain's experience library (when it has entries)
//! context/MISSION.md
//! ```

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pmt_config::harness::Tree;
use pmt_core::private_fs;

use super::ports::{Entry, EntryKind, RolloutOutcome};
use super::prompts::Layout;
use crate::harness::diff;

/// Images kept per deliverable.
pub const MAX_IMAGES: usize = 8;

/// A finished rollout as evidence.
#[derive(Debug, Clone)]
pub struct Rollout<'a> {
    /// Its label.
    pub label: &'a str,
    /// What it produced.
    pub outcome: &'a RolloutOutcome,
    /// Its images directory.
    pub images: Option<&'a Path>,
}

/// One candidate in the pool.
#[derive(Debug, Clone)]
pub struct PoolItem<'a> {
    /// Its label.
    pub label: &'a str,
    /// Its tree.
    pub tree: &'a Tree,
    /// The tree its diff is taken against.
    pub diff_base: &'a Tree,
    /// Its rollout.
    pub rollout: &'a RolloutOutcome,
    /// Its decision notes (the improver's reply).
    pub decision: &'a str,
}

/// What to lay out.
#[derive(Debug, Clone)]
pub struct Spec<'a> {
    /// `<job>/task`.
    pub task: &'a Path,
    /// The base rollouts.
    pub rollouts: &'a [Rollout<'a>],
    /// The tree to edit.
    pub harness: &'a Tree,
    /// The pool.
    pub pool: &'a [PoolItem<'a>],
    /// Include `decisions/`.
    pub decisions: bool,
    /// The domain's library directory, when it has a visible entry.
    pub library: Option<&'a Path>,
    /// `evolve.evidence.obs_trunc_chars`.
    pub obs_trunc: usize,
}

/// `<workspace>/context`.
#[must_use]
pub fn context(workspace: &Path) -> PathBuf {
    workspace.join("context")
}

/// Lays `spec` out under `<workspace>/context`.
///
/// # Errors
///
/// An I/O error, or `no rollout rendered` when rollouts were given but
/// none had a trajectory.
pub fn build(workspace: &Path, spec: &Spec<'_>) -> Result<Layout, String> {
    let root = context(workspace);
    let io = |e: std::io::Error| format!("evidence: {e}");
    private_fs::create_dir(&root).map_err(io)?;
    std::fs::copy(spec.task.join("task.md"), root.join("task.md")).map_err(io)?;
    let source = spec.task.join("source");
    if source.is_dir() {
        private_fs::copy_tree(&source, &root.join("source")).map_err(io)?;
    }
    let mut layout = Layout::default();
    for rollout in spec.rollouts {
        if rollout.outcome.entries.is_empty() {
            continue;
        }
        write(
            &root.join("rollouts").join(format!("{}.md", rollout.label)),
            &render(rollout.label, rollout.outcome, spec.obs_trunc),
        )?;
        deliverables(&root.join("deliverables").join(rollout.label), rollout)?;
        layout.rollouts.push(rollout.label.to_owned());
    }
    if !spec.rollouts.is_empty() && layout.rollouts.is_empty() {
        return Err("evidence: rollouts were given but none rendered".to_owned());
    }
    spec.harness.write(&root.join("harness")).map_err(io)?;
    for item in spec.pool {
        pool_item(&root, item, spec.obs_trunc)?;
        layout.pool.push(item.label.to_owned());
        if spec.decisions {
            write(
                &root.join("decisions").join(format!("{}.md", item.label)),
                item.decision,
            )?;
        }
    }
    layout.decisions = spec.decisions && !spec.pool.is_empty();
    if let Some(library) = spec.library {
        private_fs::copy_tree(library, &root.join("library")).map_err(io)?;
        layout.library = true;
    }
    Ok(layout)
}

fn pool_item(root: &Path, item: &PoolItem<'_>, obs_trunc: usize) -> Result<(), String> {
    let dir = root.join("pool").join(item.label);
    item.tree
        .write(&dir.join("harness"))
        .map_err(|e| format!("evidence: {e}"))?;
    write(
        &dir.join("harness.diff"),
        &diff::unified(item.diff_base, item.tree, ("base", item.label)),
    )?;
    write(
        &dir.join("rollout.md"),
        &render(item.label, item.rollout, obs_trunc),
    )?;
    write(&dir.join("produced.md"), &produced(item.rollout))
}

fn deliverables(dir: &Path, rollout: &Rollout<'_>) -> Result<(), String> {
    let outcome = rollout.outcome;
    write(
        &dir.join("final.md"),
        outcome.final_message.as_deref().unwrap_or_default(),
    )?;
    write(&dir.join("workspace.diff"), &outcome.diff)?;
    write(
        &dir.join("external_calls.json"),
        &serde_json::to_string_pretty(&outcome.external_calls).unwrap_or_default(),
    )?;
    if let Some(images) = rollout.images {
        copy_images(images, &dir.join("images")).map_err(|e| format!("evidence: {e}"))?;
    }
    Ok(())
}

/// Copies at most [`MAX_IMAGES`] files of `from` into `to`.
///
/// # Errors
///
/// I/O errors.
pub fn copy_images(from: &Path, to: &Path) -> std::io::Result<()> {
    let Ok(entries) = std::fs::read_dir(from) else {
        return Ok(());
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    if files.is_empty() {
        return Ok(());
    }
    private_fs::create_dir(to)?;
    for file in files.into_iter().take(MAX_IMAGES) {
        std::fs::copy(&file, to.join(file.file_name().unwrap_or_default()))?;
    }
    Ok(())
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        private_fs::create_dir(parent).map_err(|e| format!("evidence: {e}"))?;
    }
    std::fs::write(path, text).map_err(|e| format!("evidence: {}: {e}", path.display()))
}

/// A rollout's trajectory as Markdown; tool observations longer than
/// `obs_trunc` characters are cut.
#[must_use]
pub fn render(label: &str, outcome: &RolloutOutcome, obs_trunc: usize) -> String {
    let mut text = format!("# Rollout {label}\n\n");
    match &outcome.error {
        Some(error) => {
            let _ = writeln!(text, "Status: failed ({error})\n");
        }
        None if outcome.ok => text.push_str("Status: ok\n\n"),
        None => text.push_str("Status: no final message\n\n"),
    }
    for (i, entry) in outcome.entries.iter().enumerate() {
        let _ = writeln!(text, "## Step {}: {}\n", i + 1, heading(entry));
        text.push_str(&body(entry, obs_trunc));
        text.push_str("\n\n");
    }
    text
}

fn heading(entry: &Entry) -> String {
    let tool = entry.tool.as_deref().unwrap_or("?");
    match entry.kind {
        EntryKind::User => "user".to_owned(),
        EntryKind::Assistant => "assistant".to_owned(),
        EntryKind::ToolCall => format!("tool call {tool}"),
        EntryKind::ToolResult => format!("tool result {tool}"),
        EntryKind::Note => "note".to_owned(),
    }
}

fn body(entry: &Entry, obs_trunc: usize) -> String {
    let chars = entry.text.chars().count();
    if entry.kind != EntryKind::ToolResult || chars <= obs_trunc {
        return entry.text.clone();
    }
    let kept: String = entry.text.chars().take(obs_trunc).collect();
    format!(
        "{kept}\n[... {} characters truncated ...]",
        chars - obs_trunc
    )
}

/// What a rollout produced: its final message, workspace diff and
/// recorded external calls.
#[must_use]
pub fn produced(outcome: &RolloutOutcome) -> String {
    let mut text = String::from("## Final message\n\n");
    text.push_str(outcome.final_message.as_deref().unwrap_or("(none)"));
    text.push_str("\n\n## Workspace diff\n\n");
    if outcome.diff.trim().is_empty() {
        text.push_str("(no change)\n");
    } else {
        let _ = writeln!(text, "```diff\n{}\n```", outcome.diff.trim_end());
    }
    text.push_str("\n## Recorded external calls\n\n");
    if outcome.external_calls.is_empty() {
        text.push_str("(none)\n");
    }
    for call in &outcome.external_calls {
        let _ = writeln!(text, "- {} {}", call.tool, call.arguments);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_observations_are_truncated_and_others_kept() {
        let outcome = RolloutOutcome {
            ok: true,
            entries: vec![
                Entry::new(EntryKind::ToolResult, "x".repeat(12)).tool("read_file"),
                Entry::new(EntryKind::Assistant, "y".repeat(12)),
            ],
            ..RolloutOutcome::default()
        };
        let text = render("base1", &outcome, 10);
        assert!(text.contains("[... 2 characters truncated ...]"), "{text}");
        assert!(text.contains(&"y".repeat(12)));
        assert!(text.contains("## Step 1: tool result read_file"));
    }
}
