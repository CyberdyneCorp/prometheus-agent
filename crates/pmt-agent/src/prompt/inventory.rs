//! Which context files a session sees and why (spec: context-compression,
//! Context window view): every candidate project context file
//! (`PROMETHEUS.md`/`.prometheus.md`, the `AGENTS.md` chain, `CLAUDE.md`,
//! Cursor rules) and the profile's `SOUL.md`, each with its estimated
//! tokens and whether it was loaded, truncated, shadowed by a file that
//! wins over it, blocked by the threat scan, unreadable, or suppressed
//! (context files off, or a working directory inside the installation).
//!
//! The listing applies the same first-match rule as [`super::context`]
//! by asking it what it loads, so the two cannot disagree.

use std::path::{Path, PathBuf};

use serde::Serialize;

use super::blocks::IDENTITY_FILE;
use super::context::{self, Limits};
use super::{PromptInputs, install_tree, scan};

/// What became of one candidate file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FileState {
    /// In the prompt whole.
    Loaded,
    /// In the prompt, cut to the per-file cap.
    Truncated,
    /// Present, but a file that wins over it was loaded instead.
    Shadowed,
    /// Replaced by a marker: it matched the threat scan.
    Blocked,
    /// Could not be read (an error or the read timeout).
    Unreadable,
    /// Context files are off for this session.
    Suppressed,
}

impl FileState {
    /// The label shown.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Loaded => "loaded",
            Self::Truncated => "truncated",
            Self::Shadowed => "shadowed",
            Self::Blocked => "blocked",
            Self::Unreadable => "unreadable",
            Self::Suppressed => "suppressed",
        }
    }
}

/// One candidate file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextFile {
    /// Its name relative to the project root (`SOUL.md` for the identity).
    pub name: String,
    /// Estimated tokens of the whole file.
    pub tokens: u64,
    /// What became of it.
    pub state: FileState,
}

/// The candidate project context files present under `cwd`, nearest
/// `PROMETHEUS.md` first, then the `AGENTS.md` chain from the root down,
/// `CLAUDE.md` and the Cursor rules.
fn candidates(cwd: &Path, root: Option<&Path>) -> Vec<PathBuf> {
    let dirs: Vec<PathBuf> = match root {
        Some(root) => {
            let mut dirs: Vec<PathBuf> = cwd
                .ancestors()
                .take_while(|d| d.starts_with(root))
                .map(Path::to_path_buf)
                .collect();
            dirs.reverse();
            dirs
        }
        None => vec![cwd.to_path_buf()],
    };
    let mut found = Vec::new();
    for dir in dirs.iter().rev() {
        found.extend(["PROMETHEUS.md", ".prometheus.md"].map(|n| dir.join(n)));
    }
    for dir in &dirs {
        found.extend(["AGENTS.override.md", "AGENTS.md", "agents.md"].map(|n| dir.join(n)));
    }
    found.push(cwd.join("CLAUDE.md"));
    found.push(cwd.join(".cursorrules"));
    let mut mdc: Vec<PathBuf> = std::fs::read_dir(cwd.join(".cursor/rules"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "mdc"))
        .collect();
    mdc.sort();
    found.extend(mdc);
    found.retain(|p| std::fs::symlink_metadata(p).is_ok_and(|m| !m.is_dir()));
    found
}

fn relative(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

/// Whether `inputs` load no project context at all.
fn suppressed(inputs: &PromptInputs, cwd: &Path) -> bool {
    !inputs.config.prompt.load_context_files
        || inputs.no_context_files
        || (inputs.cwd_fallback && install_tree::contains(&install_tree::roots(), cwd))
}

/// The state of a file the loader took.
fn loaded_state(text: &str, cap: usize) -> FileState {
    if !scan::findings(text).is_empty() {
        FileState::Blocked
    } else if text.trim_end().chars().count() > cap {
        FileState::Truncated
    } else {
        FileState::Loaded
    }
}

/// Every candidate file of the session `inputs` describe, with its state.
#[must_use]
pub fn inventory(inputs: &PromptInputs) -> Vec<ContextFile> {
    let mut files = Vec::new();
    if let Some(cwd) = inputs.cwd.as_deref() {
        files = project_files(inputs, cwd);
    }
    let soul = inputs.profile_dir.join(IDENTITY_FILE);
    if let Ok(text) = std::fs::read_to_string(&soul) {
        // An empty one leaves the built-in identity in place.
        if !text.trim().is_empty() {
            files.push(ContextFile {
                name: IDENTITY_FILE.to_owned(),
                tokens: tokens(&text),
                state: FileState::Loaded,
            });
        }
    } else if soul.exists() {
        files.push(ContextFile {
            name: IDENTITY_FILE.to_owned(),
            tokens: 0,
            state: FileState::Unreadable,
        });
    }
    files
}

fn project_files(inputs: &PromptInputs, cwd: &Path) -> Vec<ContextFile> {
    let prompt = &inputs.config.prompt;
    let limits = Limits {
        cap: context::cap(prompt.context_file_max_chars, inputs.context_window),
        read_timeout: std::time::Duration::from_secs(prompt.context_file_read_timeout_secs),
    };
    let root = context::git_root(cwd);
    let base = root.clone().unwrap_or_else(|| cwd.to_path_buf());
    let off = suppressed(inputs, cwd);
    let loaded = if off {
        Vec::new()
    } else {
        context::load(cwd, limits).files
    };
    candidates(cwd, root.as_deref())
        .into_iter()
        .filter_map(|path| {
            let name = relative(&path, &base);
            let Ok(bytes) = std::fs::read(&path) else {
                return Some(ContextFile {
                    name,
                    tokens: 0,
                    state: FileState::Unreadable,
                });
            };
            let text = String::from_utf8_lossy(&bytes);
            if text.trim().is_empty() {
                return None;
            }
            let state = if off {
                FileState::Suppressed
            } else if loaded.contains(&name) {
                loaded_state(&text, limits.cap)
            } else {
                FileState::Shadowed
            };
            Some(ContextFile {
                name,
                tokens: tokens(&text),
                state,
            })
        })
        .collect()
}
