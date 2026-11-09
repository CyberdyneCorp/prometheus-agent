//! On-demand upload of granted host inputs to a backend that does not use
//! the host filesystem (spec: execution-environments, Remote file sync and
//! sync-back): attached files (the profile's `cache/media/`) and skill
//! scripts live on this host, so a command naming one of them would find
//! nothing on a container, SSH or plugin backend.
//!
//! Before such a command runs, every granted input root it names is
//! pointed at the root's copy on the backend (`<base>/<n>`, the same
//! layout below it), and each named path is uploaded first: a file inside
//! a skill (the nearest directory holding `SKILL.md`) brings its whole
//! skill directory, any other path itself (a directory recursively).
//! Files already uploaded and unchanged (same size and modification time)
//! are not sent again; one command uploads at most [`MAX_BYTES`], and a
//! notice lists what was left out.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

use crate::files::backend::FileBackend as _;
use crate::files::remote::RemoteFiles;

use super::remote::RemoteExec;

/// The most one command uploads.
pub const MAX_BYTES: u64 = 512 * 1024 * 1024;

/// The most files one named path brings.
const MAX_FILES: usize = 10_000;

/// A granted host directory and where its copy lives on the backend.
#[derive(Debug, Clone)]
struct Root {
    host: PathBuf,
    remote: String,
}

/// The inputs of one session on one remote backend.
pub struct Inputs {
    exec: Arc<dyn RemoteExec>,
    files: RemoteFiles,
    roots: Vec<Root>,
    sent: Mutex<HashMap<PathBuf, (u64, Option<SystemTime>)>>,
}

impl std::fmt::Debug for Inputs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inputs")
            .field("roots", &self.roots)
            .finish_non_exhaustive()
    }
}

/// A command ready for the backend.
#[derive(Debug, Default)]
pub struct Prepared {
    /// The command with host input paths pointed at their copies.
    pub command: String,
    /// What could not be uploaded.
    pub notices: Vec<String>,
}

impl Inputs {
    /// Inputs under `hosts` (existing directories only), copied below
    /// `base` on backend `name`.
    #[must_use]
    pub fn new(name: &str, exec: Arc<dyn RemoteExec>, base: &str, hosts: &[PathBuf]) -> Self {
        let roots = hosts
            .iter()
            .filter(|host| host.is_dir())
            .enumerate()
            .map(|(n, host)| Root {
                host: host.clone(),
                remote: format!("{}/{n}", base.trim_end_matches('/')),
            })
            .collect();
        Self {
            files: RemoteFiles::new(name, Arc::clone(&exec)),
            exec,
            roots,
            sent: Mutex::default(),
        }
    }

    /// Uploads what `command` names and returns it with every input root
    /// pointed at its copy. Runs blocking backend calls.
    #[must_use]
    pub fn prepare(&self, command: &str) -> Prepared {
        let mut prepared = Prepared {
            command: command.to_owned(),
            notices: Vec::new(),
        };
        let mut budget = MAX_BYTES;
        // Longest roots first, so a root inside another is matched whole.
        let mut roots: Vec<&Root> = self.roots.iter().collect();
        roots.sort_by_key(|root| std::cmp::Reverse(root.host.as_os_str().len()));
        for root in roots {
            let host = root.host.to_string_lossy();
            if !prepared.command.contains(host.as_ref()) {
                continue;
            }
            for named in mentioned(&prepared.command, &root.host) {
                self.upload_unit(
                    root,
                    &unit_of(&root.host, &named),
                    &mut budget,
                    &mut prepared,
                );
            }
            prepared.command = prepared.command.replace(host.as_ref(), &root.remote);
        }
        prepared
    }

    /// Uploads `unit` (a file, or a directory recursively) below `root`.
    fn upload_unit(&self, root: &Root, unit: &Path, budget: &mut u64, prepared: &mut Prepared) {
        let mut skipped = Vec::new();
        for file in files_under(unit) {
            let Ok(meta) = std::fs::metadata(&file) else {
                continue;
            };
            let stamp = (meta.len(), meta.modified().ok());
            let unchanged = self
                .sent
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&file)
                == Some(&stamp);
            if unchanged {
                continue;
            }
            if meta.len() > *budget {
                skipped.push(file.display().to_string());
                continue;
            }
            match self.send(root, &file, &meta) {
                Ok(()) => {
                    *budget -= meta.len();
                    self.sent
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(file, stamp);
                }
                Err(why) => prepared
                    .notices
                    .push(format!("{} was not uploaded: {why}", file.display())),
            }
        }
        if !skipped.is_empty() {
            prepared.notices.push(format!(
                "inputs over the {} MiB upload limit were not uploaded: {}",
                MAX_BYTES / (1024 * 1024),
                skipped.join(", ")
            ));
        }
    }

    fn send(&self, root: &Root, file: &Path, meta: &std::fs::Metadata) -> Result<(), String> {
        let relative = file.strip_prefix(&root.host).map_err(|e| e.to_string())?;
        let mut remote = root.remote.clone();
        for part in relative.components() {
            remote.push('/');
            remote.push_str(&part.as_os_str().to_string_lossy());
        }
        let bytes = std::fs::read(file).map_err(|e| e.to_string())?;
        self.files
            .write(Path::new(&remote), &bytes)
            .map_err(|e| e.to_string())?;
        if executable(meta) {
            let done = self
                .exec
                .exec("chmod +x -- \"$1\"", &[&remote], None)
                .map_err(|why| why.clone())?;
            if done.code != 0 {
                return Err(String::from_utf8_lossy(&done.stderr).trim().to_owned());
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
fn executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_meta: &std::fs::Metadata) -> bool {
    false
}

/// The existing host paths below `root` that `command` names (each
/// mention runs from the root to the next blank, quote or shell operator).
fn mentioned(command: &str, root: &Path) -> Vec<PathBuf> {
    let host = root.to_string_lossy();
    let mut found: Vec<PathBuf> = Vec::new();
    for (at, _) in command.match_indices(host.as_ref()) {
        let rest = &command[at..];
        let end = rest
            .find(|c: char| c.is_whitespace() || "'\";|&<>()`$".contains(c))
            .unwrap_or(rest.len());
        let mut path = PathBuf::from(&rest[..end]);
        // Trailing punctuation (`file.txt,`) is not part of the path.
        while !path.exists() && path.starts_with(root) && path != root {
            let text = path.to_string_lossy();
            let trimmed = text.trim_end_matches(|c: char| ",.:!?".contains(c));
            if trimmed.len() == text.len() {
                break;
            }
            path = PathBuf::from(trimmed);
        }
        if path.exists() && path.starts_with(root) && !found.contains(&path) {
            found.push(path);
        }
    }
    found
}

/// What a named path brings: the skill directory holding it, else itself.
fn unit_of(root: &Path, named: &Path) -> PathBuf {
    named
        .ancestors()
        .take_while(|dir| dir.starts_with(root) && *dir != root)
        .find(|dir| dir.join("SKILL.md").is_file())
        .unwrap_or(named)
        .to_path_buf()
}

/// The regular files at or below `path` (symbolic links not followed).
fn files_under(path: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![path.to_path_buf()];
    while let Some(next) = pending.pop() {
        let Ok(meta) = std::fs::symlink_metadata(&next) else {
            continue;
        };
        if meta.is_file() {
            files.push(next);
        } else if meta.is_dir()
            && let Ok(entries) = std::fs::read_dir(&next)
        {
            pending.extend(entries.flatten().map(|entry| entry.path()));
        }
        if files.len() >= MAX_FILES {
            break;
        }
    }
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_inside_a_skill_brings_its_skill_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("skills");
        let skill = root.join("devops/deploy");
        std::fs::create_dir_all(skill.join("scripts")).unwrap();
        std::fs::write(skill.join("SKILL.md"), "---\nname: deploy\n---\n").unwrap();
        std::fs::write(skill.join("scripts/run.sh"), "echo hi\n").unwrap();
        let command = format!("sh {}/scripts/run.sh, then", skill.display());
        let named = mentioned(&command, &root);
        assert_eq!(named, [skill.join("scripts/run.sh")]);
        assert_eq!(unit_of(&root, &named[0]), skill);
        let loose = root.join("notes.txt");
        std::fs::write(&loose, "x").unwrap();
        assert_eq!(unit_of(&root, &loose), loose);
    }
}
