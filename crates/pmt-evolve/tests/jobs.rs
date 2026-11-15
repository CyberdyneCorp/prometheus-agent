//! Evolution job contracts: the opt-in gate, task capture and triggers
//! (spec: harness-evolution, Opt-in per profile; Evolution task capture;
//! On-demand and scheduled triggers; Automatic triggers after failed or
//! long tasks; Leak globs and verdict tripwire).

use std::path::Path;
use std::process::Command;

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::secret::Redactor;
use pmt_evolve::job::{self, Jobs, Profile, Request, TaskInput, Trigger};
use pmt_store::{EndReason, NewMessage, NewSession, Role, Source, Store};
use serde_json::json;

struct Fixture {
    root: tempfile::TempDir,
    scope: ProfileScope,
}

impl Fixture {
    fn new(overrides: &[&str]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = Home::open(&root.path().join("home")).unwrap();
        let profile = home.profile(&ProfileName::parse("work").unwrap()).unwrap();
        let scope = ProfileScope::open(
            profile,
            &Overrides::parse(overrides).unwrap(),
            Launch::Other,
        )
        .unwrap();
        Self { root, scope }
    }

    fn enabled() -> Self {
        Self::new(&["evolve.enabled=true"])
    }

    fn profile<'a>(&'a self, store: Option<&'a Store>) -> Profile<'a> {
        Profile {
            dir: self.scope.dir(),
            name: "work",
            config: self.scope.config(),
            store,
            redactor: Redactor::default(),
        }
    }

    fn workspace(&self) -> std::path::PathBuf {
        let dir = self.root.path().join("workspace");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn task_file(&self, body: &str) -> std::path::PathBuf {
        let path = self.root.path().join("task.toml");
        std::fs::write(&path, body).unwrap();
        path
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
        ])
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Scenario: Disabled profile.
#[test]
fn a_disabled_profile_refuses_with_78_and_creates_nothing() {
    let fixture = Fixture::new(&[]);
    let task = fixture.task_file("statement = \"sum the ledger\"\n");
    let err = job::queue(
        &fixture.profile(None),
        &Request::new(Trigger::Cli, TaskInput::File(task)),
    )
    .unwrap_err();
    assert_eq!(err.exit_code(), 78);
    assert_eq!(
        err.message(),
        "harness evolution is disabled for profile 'work'; set evolve.enabled = true"
    );
    assert!(!fixture.scope.dir().join("evolve").exists());
}

/// Scenario: Git workspace.
#[test]
fn a_git_session_freezes_head_and_a_patch_that_reproduces_it() {
    let fixture = Fixture::enabled();
    let repo = fixture.workspace();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("ledger.txt"), "10\n20\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "first"]);
    let head = git(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
    // Uncommitted: a modified tracked file and a new one.
    std::fs::write(repo.join("ledger.txt"), "10\n20\n-5\n").unwrap();
    std::fs::write(repo.join("notes.md"), "check the refund\n").unwrap();
    let index_before = std::fs::read(repo.join(".git/index")).unwrap();

    let store = Store::open(&fixture.scope).unwrap();
    let mut new = NewSession::new(Source::Cli);
    new.cwd = Some(repo.display().to_string());
    let session = store.create_session(&new).unwrap();
    store
        .append_message(
            session.id,
            &NewMessage::new(Role::User, "Reconcile the ledger"),
        )
        .unwrap();
    let mut call = NewMessage::new(Role::Assistant, "");
    call.tool_calls = Some(json!([{"id": "c1", "name": "write_file",
        "arguments": {"path": "ledger.txt", "content": "x"}}]));
    store.append_message(session.id, &call).unwrap();

    let record = job::queue(
        &fixture.profile(Some(&store)),
        &Request::new(Trigger::Cli, TaskInput::Session(session.id)),
    )
    .unwrap();
    let task = Jobs::of(fixture.scope.dir()).dir(&record.id).join("task");
    assert_eq!(
        std::fs::read_to_string(task.join("task.md")).unwrap(),
        "Reconcile the ledger"
    );
    let repo_toml = std::fs::read_to_string(task.join("source/repo.toml")).unwrap();
    assert_eq!(repo_toml, format!("commit = \"{head}\"\n"));
    assert_eq!(
        std::fs::read(repo.join(".git/index")).unwrap(),
        index_before,
        "user index untouched"
    );

    // Applying the patch to a checkout of the commit reproduces the state.
    let replay = fixture.root.path().join("replay");
    git(
        fixture.root.path(),
        &["clone", "-q", repo.to_str().unwrap(), "replay"],
    );
    git(&replay, &["checkout", "-q", &head]);
    git(
        &replay,
        &[
            "apply",
            task.join("source/uncommitted.patch").to_str().unwrap(),
        ],
    );
    assert_eq!(
        std::fs::read_to_string(replay.join("ledger.txt")).unwrap(),
        "10\n20\n-5\n"
    );
    assert_eq!(
        std::fs::read_to_string(replay.join("notes.md")).unwrap(),
        "check the refund\n"
    );

    let meta: toml::Table =
        toml::from_str(&std::fs::read_to_string(task.join("task.toml")).unwrap()).unwrap();
    assert_eq!(
        meta["source_session"].as_str(),
        Some(session.id.to_string().as_str())
    );
    assert_eq!(meta["domain"].as_str(), Some("general"));
    assert_eq!(meta["fingerprint"].as_str().unwrap().len(), 16);
    assert_eq!(record.status, "queued");
    assert_eq!(record.plan[0].rollouts, 8);
    assert_eq!(record.plan[0].sessions, 6);
}

/// Scenario: Dot-file glob honoured.
#[test]
fn a_dot_file_leak_glob_is_excluded_from_the_snapshot() {
    let fixture = Fixture::new(&["evolve.enabled=true", "evolve.leak_globs=[\".answers/**\"]"]);
    let workspace = fixture.workspace();
    std::fs::create_dir_all(workspace.join(".answers")).unwrap();
    std::fs::write(workspace.join(".answers/q1.txt"), "42").unwrap();
    std::fs::write(workspace.join("q1.md"), "What is 6 x 7?").unwrap();
    let task = fixture
        .task_file("statement = \"answer q1\"\nworkspace = \"workspace\"\ndomain = \"quiz\"\n");
    let record = job::queue(
        &fixture.profile(None),
        &Request::new(Trigger::Cli, TaskInput::File(task)),
    )
    .unwrap();
    let source = Jobs::of(fixture.scope.dir())
        .dir(&record.id)
        .join("task/source");
    assert!(source.join("q1.md").is_file());
    assert!(!source.join(".answers").exists());
    assert_eq!(record.domain, "quiz");
}

#[test]
fn a_snapshot_over_the_cap_fails_with_65_and_queues_nothing() {
    let fixture = Fixture::new(&["evolve.enabled=true", "evolve.task.max_source_bytes=10"]);
    let workspace = fixture.workspace();
    std::fs::write(workspace.join("big.txt"), "x".repeat(64)).unwrap();
    let task = fixture.task_file("statement = \"t\"\nworkspace = \"workspace\"\n");
    let err = job::queue(
        &fixture.profile(None),
        &Request::new(Trigger::Cli, TaskInput::File(task)),
    )
    .unwrap_err();
    assert_eq!(err.exit_code(), 65);
    assert!(Jobs::of(fixture.scope.dir()).list().unwrap().is_empty());
}

/// Scenario: Cron-triggered evolution (the record a scheduled job's
/// trigger writes).
#[test]
fn a_cron_trigger_records_its_trigger_and_job_id() {
    let fixture = Fixture::enabled();
    let task = fixture.task_file("statement = \"write the weekly report\"\n");
    let mut request = Request::new(Trigger::Cron, TaskInput::File(task));
    request.cron_job_id = Some("job_1".to_owned());
    let record = job::queue(&fixture.profile(None), &request).unwrap();
    let stored = Jobs::of(fixture.scope.dir()).get(&record.id).unwrap();
    assert_eq!(stored.trigger, Trigger::Cron);
    assert_eq!(stored.cron_job_id.as_deref(), Some("job_1"));
    assert_eq!(stored.status, "queued");
}

/// What `f` logs at INFO and above.
fn logged(f: impl FnOnce()) -> String {
    use std::sync::{Arc, Mutex};
    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let buffer = Buffer::default();
    let writer = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap()
}

/// Scenario: Daily cap.
#[test]
fn the_fourth_automatic_job_of_a_day_is_not_queued() {
    let fixture = Fixture::new(&["evolve.enabled=true", "evolve.auto.enabled=true"]);
    let store = Store::open(&fixture.scope).unwrap();
    let profile = fixture.profile(Some(&store));
    let mut queued = 0;
    let mut log = String::new();
    for _ in 0..4 {
        let session = store.create_session(&NewSession::new(Source::Cli)).unwrap();
        store
            .append_message(session.id, &NewMessage::new(Role::User, "fix the build"))
            .unwrap();
        let mut call = NewMessage::new(Role::Assistant, "");
        call.tool_calls = Some(json!([{"id": "c", "name": "terminal", "arguments": {}}]));
        store.append_message(session.id, &call).unwrap();
        let mut outcome = None;
        log = logged(|| {
            outcome = Some(
                job::auto::after_session(&profile, &store, session.id, EndReason::Error).unwrap(),
            );
        });
        if outcome.flatten().is_some() {
            queued += 1;
        }
    }
    assert_eq!(queued, 3);
    assert!(log.contains("INFO"), "{log}");
    assert!(log.contains("daily auto-evolve cap reached"), "{log}");
    let jobs = Jobs::of(fixture.scope.dir()).list().unwrap();
    assert_eq!(jobs.len(), 3);
    assert!(jobs.iter().all(|j| j.trigger == Trigger::Auto));
}
