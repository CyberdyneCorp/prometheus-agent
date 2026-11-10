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

#[cfg(all(test, unix))]
mod tests {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::process::{Command, Stdio};

    use pmt_config::grants::{GrantsFile, Subject};
    use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
    use pmt_core::capability::Capability;
    use pmt_tools::terminal::remote::exec::ExecResult;

    use super::*;

    /// A remote host that is a local directory: scripts run in `sh` with
    /// `HOME` there, as on an SSH host; every script is logged.
    #[derive(Debug)]
    struct LocalHost {
        home: PathBuf,
        log: Mutex<Vec<(String, Vec<String>)>>,
    }

    impl RemoteExec for LocalHost {
        fn exec(
            &self,
            script: &str,
            args: &[&str],
            stdin: Option<&[u8]>,
        ) -> Result<ExecResult, String> {
            self.log.lock().unwrap().push((
                script.to_owned(),
                args.iter().map(|a| (*a).to_owned()).collect(),
            ));
            let mut child = Command::new("sh")
                .args(["-c", script, "pmt"])
                .args(args)
                .env("HOME", &self.home)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| e.to_string())?;
            let mut pipe = child.stdin.take().unwrap();
            pipe.write_all(stdin.unwrap_or_default()).unwrap();
            drop(pipe);
            let out = child.wait_with_output().map_err(|e| e.to_string())?;
            Ok(ExecResult {
                code: out.status.code().unwrap_or(-1),
                stdout: out.stdout,
                stderr: out.stderr,
            })
        }
    }

    /// A session with skill `gcloud-ops` (declaring `creds/gcp.json`,
    /// which exists) loaded; `grant` adds its `credential_files` grant.
    fn session(root: &Path, grant: bool) -> Arc<SkillsSession> {
        let home = Home::open(&root.join("home")).unwrap();
        let dir = home
            .profile(&ProfileName::parse("default").unwrap())
            .unwrap();
        dir.config_file()
            .replace("[model]\nprovider = \"mock\"\ndefault = \"mock\"\n")
            .unwrap();
        let scope = ProfileScope::open(dir.clone(), &Overrides::default(), Launch::Other).unwrap();
        let profile = scope.dir().to_path_buf();
        std::fs::create_dir_all(profile.join("skills/gcloud-ops")).unwrap();
        std::fs::write(
            profile.join("skills/gcloud-ops/SKILL.md"),
            "---\nname: gcloud-ops\ndescription: Operate Google Cloud.\nmetadata:\n  prometheus:\n    credential_files:\n      - path: creds/gcp.json\n        description: Service account key.\n---\nUse gcloud.\n",
        )
        .unwrap();
        std::fs::create_dir_all(profile.join("creds")).unwrap();
        std::fs::write(profile.join("creds/gcp.json"), "{\"key\": \"s3cret\"}").unwrap();
        if grant {
            GrantsFile::of(&dir)
                .add(
                    &Subject::Skill("gcloud-ops".to_owned()),
                    &std::iter::once(Capability::CredentialFiles).collect(),
                    "cli",
                )
                .unwrap();
        }
        let store = Arc::new(pmt_store::Store::open(&scope).unwrap());
        let mut setup = pmt_skills::session::SessionSetup::new(scope, store, "s1");
        setup.bundled = Arc::new(pmt_skills::bundled::Bundled::default());
        let skills = Arc::new(SkillsSession::new(setup));
        skills.mark_loaded("gcloud-ops", Vec::new());
        skills
    }

    fn host(root: &Path) -> Arc<LocalHost> {
        let home = root.join("remote-home");
        std::fs::create_dir_all(&home).unwrap();
        Arc::new(LocalHost {
            home,
            log: Mutex::new(Vec::new()),
        })
    }

    /// Spec: skills, Skill credential file passthrough (remote backends
    /// upload before each command and delete after the session).
    #[test]
    fn a_remote_backend_gets_the_file_before_each_command_and_loses_it_after() {
        let root = tempfile::tempdir().unwrap();
        let skills = session(root.path(), true);
        let host = host(root.path());
        let remote = host.home.join(".prometheus/creds/gcp.json");
        let hook = remote_hook(&skills, Arc::clone(&host) as Arc<dyn RemoteExec>);
        hook("gcloud auth list", Path::new("/work"));
        assert_eq!(
            std::fs::read_to_string(&remote).unwrap(),
            "{\"key\": \"s3cret\"}"
        );
        assert_eq!(
            std::fs::metadata(&remote).unwrap().permissions().mode() & 0o777,
            0o400
        );
        // A command that changed the copy gets it back before the next.
        std::fs::remove_file(&remote).unwrap();
        hook("gcloud config list", Path::new("/work"));
        assert!(remote.is_file());
        assert_eq!(
            host.log
                .lock()
                .unwrap()
                .iter()
                .filter(|(s, _)| s == UPLOAD)
                .count(),
            2
        );
        drop(hook);
        assert!(!remote.exists(), "the upload outlived the session");
        let log = host.log.lock().unwrap();
        assert_eq!(
            log.last().unwrap(),
            &(REMOVE.to_owned(), vec!["creds/gcp.json".to_owned()])
        );
    }

    /// Scenario: No grant, no file (on a remote backend).
    #[test]
    fn without_the_grant_nothing_is_uploaded() {
        let root = tempfile::tempdir().unwrap();
        let skills = session(root.path(), false);
        let host = host(root.path());
        let hook = remote_hook(&skills, Arc::clone(&host) as Arc<dyn RemoteExec>);
        hook("gcloud auth list", Path::new("/work"));
        drop(hook);
        assert!(host.log.lock().unwrap().is_empty());
        assert!(!host.home.join(".prometheus").exists());
    }
}
