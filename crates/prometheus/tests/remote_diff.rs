//! `/diff`'s git modes on a session whose files are not host files run git
//! on that backend, in the session's cwd there (spec: execution-
//! environments, Working-tree diff command: "for the session cwd's git
//! repository"). The backend is a fake `terminal-backend` plugin whose
//! "remote" filesystem is a directory of its own, distinct from the
//! directory the host session was launched in.
#![cfg(unix)] // the fake backend runs commands with `sh`

mod common;

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_sandbox::backend::{
    BackendRegistry, Descriptor, ExecOutput, ExecRequest, Isolation, RemoteBackend,
};
use pmt_tools::snapshots::worktree::{self, Request};
use pmt_tools::snapshots::{HostTree, SnapshotStore};
use prometheus::chat::session::{backend, snapshots};

use common::temp_home;

/// A remote sandbox: commands run with `sh` in its own root directory,
/// git never looking above it (the test directory sits inside this
/// project's own repository).
#[derive(Debug)]
struct FakeRemote {
    descriptor: Descriptor,
    root: PathBuf,
}

impl RemoteBackend for FakeRemote {
    fn name(&self) -> &'static str {
        "fakebox"
    }
    fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }
    fn check(&self) -> Result<String, String> {
        Ok("ok".to_owned())
    }
    fn execute(&self, request: &ExecRequest) -> Result<ExecOutput, String> {
        let output = Command::new("sh")
            .arg("-c")
            .arg(&request.command)
            .current_dir(&request.cwd)
            .env("GIT_CEILING_DIRECTORIES", &self.root)
            .output()
            .map_err(|e| e.to_string())?;
        Ok(ExecOutput {
            exit_code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            cwd_observed: None,
        })
    }
    fn upload(&self, _: &str, _: &[u8]) -> Result<(), String> {
        Err("not used".to_owned())
    }
    fn download(&self, _: &str) -> Result<Vec<u8>, String> {
        Err("not used".to_owned())
    }
    fn cancel(&self, _: &str) {}
    fn teardown(&self) {}
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A repository at `dir` with `file` committed as `old`, then changed to
/// `new` (unstaged).
fn repo(dir: &Path, file: &str, old: &str, new: &str) {
    fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"]);
    fs::write(dir.join(file), old).unwrap();
    git(dir, &["add", file]);
    git(dir, &["commit", "-q", "-m", "init"]);
    fs::write(dir.join(file), new).unwrap();
}

struct Fixture {
    _root: tempfile::TempDir,
    scope: ProfileScope,
    registry: BackendRegistry,
    host: PathBuf,
    remote: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = temp_home();
        let base = fs::canonicalize(root.path()).unwrap();
        let host = base.join("host");
        let remote = base.join("remote");
        // The host launch directory is a repository too: a diff run on
        // the host would show `host.txt`.
        repo(&host, "host.txt", "host one\n", "host two\n");
        fs::create_dir_all(&remote).unwrap();
        let home = Home::open(&base.join("home")).unwrap();
        let dir = home
            .profile(&ProfileName::parse("default").unwrap())
            .unwrap();
        fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join("config.toml"))
            .unwrap()
            .write_all(b"\n[terminal]\nbackend = \"fakebox\"\n")
            .unwrap();
        let scope = ProfileScope::open(dir, &Overrides::default(), Launch::Other).unwrap();
        let mut descriptor = Descriptor::ssh();
        descriptor.default_cwd = remote.to_string_lossy().into_owned();
        assert_eq!(descriptor.isolation, Isolation::Remote);
        let mut registry = BackendRegistry::builtin();
        registry
            .register_plugin(Arc::new(FakeRemote {
                descriptor,
                root: base.clone(),
            }))
            .unwrap();
        Self {
            _root: root,
            scope,
            registry,
            host,
            remote,
        }
    }

    /// `/diff <args>` of a session launched in the host directory.
    fn diff(&self, args: &str) -> String {
        let remote = backend::select(&self.scope, &self.registry, "ses_r", &self.host, None)
            .expect("the session runs on the plugin backend");
        assert_eq!(remote.start_cwd, self.remote);
        let worktree = snapshots::worktree(&self.host, Some(&remote));
        let store = SnapshotStore::of_profile(self.scope.dir());
        worktree::diff(&store, &HostTree, &worktree, "ses_r", &Request::parse(args))
    }
}

/// Scenario: a remote session's working-tree diff is its own repository's.
#[test]
fn the_git_modes_diff_the_remote_session_cwd_not_the_host_one() {
    let fx = Fixture::new();
    assert_eq!(
        fx.diff(""),
        "not a git repository",
        "the remote cwd is no repository, whatever the host directory is"
    );

    repo(&fx.remote, "a.txt", "one\n", "two\n");
    fs::write(fx.remote.join("b.txt"), "new\n").unwrap();
    let unstaged = fx.diff("");
    assert!(unstaged.contains("-one\n+two\n"), "{unstaged}");
    assert!(unstaged.contains("+new\n"), "untracked b.txt: {unstaged}");
    assert!(!unstaged.contains("host"), "{unstaged}");

    git(&fx.remote, &["add", "a.txt"]);
    let staged = fx.diff("staged --stat");
    assert!(staged.contains("a.txt"), "{staged}");
    assert!(!staged.contains("b.txt"), "{staged}");
    let all = fx.diff("all --stat");
    assert!(all.contains("a.txt") && all.contains("b.txt"), "{all}");
}
