//! Composer completion of `@` references (spec: agent-turn-loop, Context
//! reference safety and completion): `@` completes to the reference kinds,
//! `@file:`/`@folder:` to paths below the working directory with sizes.

use std::path::Path;

/// The reference kinds a bare `@` completes to.
pub const KINDS: &[&str] = &["@file:", "@folder:", "@diff", "@staged", "@git:", "@url:"];

/// Completions shown at most.
pub const MAX_COMPLETIONS: usize = 50;

/// One completion candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The text that replaces the word being completed.
    pub text: String,
    /// What the menu shows (the path with its size, or the kind).
    pub display: String,
}

/// A human size (`512 B`, `1.5 KiB`, `3.0 MiB`).
#[must_use]
pub fn size(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)] // display only
    let value = bytes as f64;
    match bytes {
        0..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KiB", value / 1024.0),
        _ => format!("{:.1} MiB", value / 1_048_576.0),
    }
}

/// Candidates for `word` (the word under the cursor, starting with `@`).
#[must_use]
pub fn complete(word: &str, cwd: &Path) -> Vec<Completion> {
    if let Some(partial) = word.strip_prefix("@file:") {
        return paths("@file:", partial, cwd, false);
    }
    if let Some(partial) = word.strip_prefix("@folder:") {
        return paths("@folder:", partial, cwd, true);
    }
    KINDS
        .iter()
        .filter(|kind| kind.starts_with(word))
        .map(|kind| Completion {
            text: (*kind).to_owned(),
            display: (*kind).to_owned(),
        })
        .collect()
}

fn paths(kind: &str, partial: &str, cwd: &Path, folders_only: bool) -> Vec<Completion> {
    let (dir_part, prefix) = match partial.rfind('/') {
        Some(i) => (&partial[..=i], &partial[i + 1..]),
        None => ("", partial),
    };
    let mut entries: Vec<_> = std::fs::read_dir(cwd.join(dir_part))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(prefix))
        .filter(|e| e.file_name() != ".git")
        .collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    entries
        .into_iter()
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            if folders_only && !meta.is_dir() {
                return None;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = format!("{dir_part}{name}");
            Some(if meta.is_dir() {
                Completion {
                    text: format!("{kind}{path}/"),
                    display: format!("{path}/"),
                }
            } else {
                Completion {
                    text: format!("{kind}{path}"),
                    display: format!("{path} ({})", size(meta.len())),
                }
            })
        })
        .take(MAX_COMPLETIONS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_paths_complete() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), vec![b'x'; 2048]).unwrap();
        std::fs::write(dir.path().join("README.md"), "hi").unwrap();

        let kinds: Vec<_> = complete("@f", dir.path())
            .into_iter()
            .map(|c| c.text)
            .collect();
        assert_eq!(kinds, ["@file:", "@folder:"]);
        assert_eq!(complete("@", dir.path()).len(), KINDS.len());

        let files = complete("@file:", dir.path());
        assert_eq!(
            files,
            [
                Completion {
                    text: "@file:README.md".into(),
                    display: "README.md (2 B)".into()
                },
                Completion {
                    text: "@file:src/".into(),
                    display: "src/".into()
                },
            ]
        );
        let nested = complete("@file:src/m", dir.path());
        assert_eq!(nested[0].display, "src/main.rs (2.0 KiB)");
        let folders = complete("@folder:", dir.path());
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].text, "@folder:src/");
    }
}
