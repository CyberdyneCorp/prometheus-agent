//! Skill credential files on container and remote backends (spec: skills,
//! Skill credential file passthrough).
//!
//! A container gets one host directory per container,
//! `<profile>/run/skill-credentials/<container name>/`, mounted read-only
//! at [`CONTAINER_HOME`] (the container's Prometheus home); before each
//! command it is made to hold exactly the declared, existing files of the
//! session's loaded skills that hold the `credential_files` grant. A
//! remote backend (SSH, plugin) gets each such file uploaded under its
//! `~/.prometheus/` before each command and removed when the session ends.
//! The host session's file tools never read them: they live under the
//! profile home, which the session sandbox denies.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use pmt_config::ProfileScope;
use pmt_sandbox::container::options::Volume;
use pmt_skills::credentials::{self, Passed};
use pmt_skills::session::SkillsSession;
use pmt_tools::terminal::BeforeCommand;
use pmt_tools::terminal::remote::exec::RemoteExec;

/// The container's Prometheus home, where credential files appear.
pub const CONTAINER_HOME: &str = "/prometheus";

/// The host directory mounted into container `name`.
#[must_use]
pub fn staging_dir(scope: &ProfileScope, container: &str) -> PathBuf {
    scope.path("run/skill-credentials").join(container)
}

/// The read-only mount of a container's credential directory (created
/// empty when missing).
#[must_use]
pub fn volume(scope: &ProfileScope, container: &str) -> Volume {
    let host = staging_dir(scope, container);
    if let Err(e) = pmt_core::private_fs::create_dir(&host) {
        tracing::warn!(dir = %host.display(), error = %e, "credential directory not created");
    }
    Volume {
        host,
        container: CONTAINER_HOME.to_owned(),
        read_only: true,
    }
}

/// The files to pass now (the catalog read once per command).
fn wanted(skills: &Weak<SkillsSession>) -> Vec<Passed> {
    let Some(session) = skills.upgrade() else {
        return Vec::new();
    };
    let (catalog, _) = session.catalog();
    credentials::passed(&session, &catalog)
}

/// Makes `dir` hold exactly `files` (by their relative paths).
pub fn sync_dir(dir: &Path, files: &[Passed]) {
    let keep: BTreeSet<&str> = files.iter().map(|f| f.rel.as_str()).collect();
    for rel in pmt_skills::layout::walk(dir) {
        if !keep.contains(rel.as_str()) {
            let _ = std::fs::remove_file(dir.join(&rel));
        }
    }
    for file in files {
        let target = file
            .rel
            .split('/')
            .fold(dir.to_path_buf(), |p, c| p.join(c));
        let copy = || -> std::io::Result<()> {
            if let Some(parent) = target.parent() {
                pmt_core::private_fs::create_dir(parent)?;
            }
            pmt_core::private_fs::write_atomic(&target, &std::fs::read(&file.source)?)
        };
        if let Err(e) = copy() {
            tracing::warn!(skill = %file.skill, path = %file.rel, error = %e, "credential file not staged");
        }
    }
}

/// The hook of a container session.
#[must_use]
pub fn container_hook(skills: &Arc<SkillsSession>, dir: PathBuf) -> BeforeCommand {
    let skills = Arc::downgrade(skills);
    Arc::new(move |_: &str, _: &Path| sync_dir(&dir, &wanted(&skills)))
}

/// Uploads to a remote backend, removing what it uploaded when dropped
/// (with the session's terminal).
struct Uploads {
    exec: Arc<dyn RemoteExec>,
    sent: Mutex<BTreeSet<String>>,
}

const UPLOAD: &str = "f=\"$HOME/.prometheus/$1\"; mkdir -p \"$(dirname \"$f\")\" && rm -f \"$f\" && umask 077 && cat > \"$f\" && chmod 400 \"$f\"";
const REMOVE: &str = "rm -f \"$HOME/.prometheus/$1\"";

impl Uploads {
    fn send(&self, files: &[Passed]) {
        for file in files {
            let Ok(bytes) = std::fs::read(&file.source) else {
                continue;
            };
            match self.exec.exec(UPLOAD, &[&file.rel], Some(&bytes)) {
                Ok(_) => {
                    self.sent
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert(file.rel.clone());
                }
                Err(e) => {
                    tracing::warn!(path = %file.rel, error = %e, "credential file not uploaded");
                }
            }
        }
    }
}

impl Drop for Uploads {
    fn drop(&mut self) {
        let sent = std::mem::take(&mut *self.sent.lock().unwrap_or_else(PoisonError::into_inner));
        for rel in sent {
            let _ = self.exec.exec(REMOVE, &[&rel], None);
        }
    }
}

/// The hook of a remote (SSH, plugin) session.
#[must_use]
pub fn remote_hook(skills: &Arc<SkillsSession>, exec: Arc<dyn RemoteExec>) -> BeforeCommand {
    let skills = Arc::downgrade(skills);
    let uploads = Uploads {
        exec,
        sent: Mutex::new(BTreeSet::new()),
    };
    Arc::new(move |_: &str, _: &Path| uploads.send(&wanted(&skills)))
}
