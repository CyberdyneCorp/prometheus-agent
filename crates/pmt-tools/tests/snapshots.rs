//! File snapshots, undo, checkpoints, rollback, store management and the
//! session diff (spec: execution-environments, File snapshots and undo;
//! Snapshot triggers, caps and per-run toggle; Rollback listing, diff and
//! selective restore; Snapshot store management CLI; Working-tree diff
//! command).

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use pmt_core::capability::{BASELINE, CapabilitySet, Places};
use pmt_tools::files::edits::{EditCall, with_call};
use pmt_tools::files::patch::PatchArgs;
use pmt_tools::files::{ControlPaths, FileSession, FileSetup, Linter, LocalBackend, ReadArgs};
use pmt_tools::snapshots::manage::{self, PrunePolicy};
use pmt_tools::snapshots::{
    SessionSnapshots, Settings, SnapshotStore, checkpoint, restore, worktree,
};

struct Fixture {
    _root: tempfile::TempDir,
    work: PathBuf,
    store: SnapshotStore,
    turn: Arc<AtomicU64>,
    snapshots: Arc<SessionSnapshots>,
    files: FileSession,
}

fn settings() -> Settings {
    Settings {
        enabled: true,
        terminal: true,
        max_file_bytes: 1024,
        max_checkpoints: 20,
        retention: Duration::from_hours(24 * 7),
        max_bytes: 1 << 30,
        prune_interval: Duration::from_hours(24),
    }
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let base = dunce::canonicalize(root.path()).unwrap();
        let work = base.join("work");
        let profile = base.join("home/.prometheus/profiles/default");
        fs::create_dir_all(work.join("src")).unwrap();
        fs::create_dir_all(&profile).unwrap();
        let store = SnapshotStore::of_profile(&profile);
        let turn = Arc::new(AtomicU64::new(1));
        let source = Arc::clone(&turn);
        let snapshots = Arc::new(SessionSnapshots::new(
            store.clone(),
            "ses_a",
            &work,
            settings(),
            Arc::new(move || source.load(Ordering::SeqCst)),
            vec![base.join("home")],
        ));
        let places = Places {
            user_home: base.join("home"),
            workspace: work.clone(),
            scratch: Vec::new(),
            toolchain: Vec::new(),
        };
        let capabilities = CapabilitySet::parse(BASELINE).unwrap();
        let cwd = work.clone();
        let files = FileSession::new(FileSetup {
            backend: Arc::new(LocalBackend::new(capabilities, places)),
            cwd: Arc::new(move || cwd.clone()),
            user_home: Some(base.join("home")),
            control: ControlPaths {
                prometheus_home: base.join("home/.prometheus"),
            },
            read_max_chars: pmt_tools::setting::fixed(100_000),
            linter: Linter::builtin(),
            documents: None,
            edits: Some(Arc::clone(&snapshots) as _),
        });
        Self {
            _root: root,
            work,
            store,
            turn,
            snapshots,
            files,
        }
    }

    fn put(&self, rel: &str, text: &str) {
        let path = self.work.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn get(&self, rel: &str) -> Option<String> {
        fs::read_to_string(self.work.join(rel)).ok()
    }

    fn call<R>(&self, tool: &str, f: impl FnOnce(&FileSession) -> R) -> R {
        let call = EditCall {
            call_id: format!("call_{tool}"),
            tool: tool.to_owned(),
        };
        with_call(call, || f(&self.files))
    }

    fn patch(&self, path: &str, old: &str, new: &str) {
        let args = PatchArgs::from_value(&serde_json::json!({
            "path": path, "old_string": old, "new_string": new
        }))
        .unwrap();
        let result = self.call("patch", |files| files.patch(&args));
        assert_eq!(
            result.status,
            pmt_tools::result::Status::Ok,
            "{}",
            result.text_content()
        );
    }

    fn write(&self, path: &str, content: &str) {
        let result = self.call("write_file", |files| files.write_file(path, content));
        assert_eq!(
            result.status,
            pmt_tools::result::Status::Ok,
            "{}",
            result.text_content()
        );
    }

    fn checkpoints(&self) -> Vec<checkpoint::Checkpoint> {
        restore::checkpoints(&self.store, &self.work)
    }
}

/// Scenario: Undo last turn.
#[test]
fn undo_restores_a_patched_file_and_removes_a_created_one() {
    let fx = Fixture::new();
    fx.put("src/lib.rs", "fn a() {}\n");
    fx.patch("src/lib.rs", "fn a() {}", "fn b() {}");
    fx.write("src/new.rs", "pub fn new() {}\n");
    let report = restore::undo(&fx.store, "ses_a", Some(1), 1, false, false).unwrap();
    assert_eq!(fx.get("src/lib.rs").as_deref(), Some("fn a() {}\n"));
    assert_eq!(fx.get("src/new.rs"), None);
    assert_eq!(report.turns, [1]);
    assert!(report.conflicts.is_empty());
    // The turn is undone once.
    let again = restore::undo(&fx.store, "ses_a", Some(1), 1, false, false).unwrap();
    assert!(again.turns.is_empty());
}

/// Scenario: Conflicting edit.
#[test]
fn a_file_edited_after_the_agent_is_not_undone_without_force() {
    let fx = Fixture::new();
    fx.put("src/lib.rs", "fn a() {}\n");
    fx.patch("src/lib.rs", "fn a() {}", "fn b() {}");
    fx.put("src/lib.rs", "fn mine() {}\n");
    let report = restore::undo(&fx.store, "ses_a", Some(1), 1, false, false).unwrap();
    assert_eq!(fx.get("src/lib.rs").as_deref(), Some("fn mine() {}\n"));
    assert_eq!(report.conflicts.len(), 1);
    assert!(
        report.conflicts[0].starts_with("conflict: ")
            && report.conflicts[0]
                .ends_with("src/lib.rs changed since turn 1; use --force to overwrite"),
        "{:?}",
        report.conflicts
    );
    restore::undo(&fx.store, "ses_a", Some(1), 1, true, false).unwrap();
    assert_eq!(fx.get("src/lib.rs").as_deref(), Some("fn a() {}\n"));
}

/// Scenario: One checkpoint per directory per turn.
#[test]
fn three_patches_in_one_turn_make_one_checkpoint() {
    let fx = Fixture::new();
    for name in ["a", "b", "c"] {
        fx.put(&format!("src/{name}.rs"), "x\n");
    }
    for name in ["a", "b", "c"] {
        fx.patch(&format!("src/{name}.rs"), "x", "y");
    }
    let taken = fx.checkpoints();
    assert_eq!(taken.len(), 1);
    assert_eq!((taken[0].turn, taken[0].tool.as_str()), (1, "patch"));
    // A new turn checkpoints again.
    fx.turn.store(2, Ordering::SeqCst);
    fx.patch("src/a.rs", "y", "z");
    assert_eq!(fx.checkpoints().len(), 2);
}

/// Scenario: Large file.
#[test]
fn a_large_file_is_listed_as_skipped_by_a_destructive_command_checkpoint() {
    let fx = Fixture::new();
    fx.put("data/dataset.bin", &"0".repeat(4096));
    fx.put("data/notes.txt", "keep\n");
    fx.snapshots.before_command("ls data", &fx.work);
    assert!(fx.checkpoints().is_empty(), "ls is not destructive");
    fx.snapshots.before_command("rm data/notes.txt", &fx.work);
    let taken = fx.checkpoints();
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].skipped(), ["data/dataset.bin"]);
    assert_eq!(taken[0].tool, "terminal");
}

/// Scenario: Hand-edit preserved.
#[test]
fn rollback_keeps_hand_edited_files_and_restores_the_rest() {
    let fx = Fixture::new();
    fx.put("a.rs", "a0\n");
    fx.put("b.rs", "b0\n");
    let _ = fx.files.read_file(&ReadArgs::new("a.rs"));
    let _ = fx.files.read_file(&ReadArgs::new("b.rs"));
    fx.write("a.rs", "a1\n");
    fx.write("b.rs", "b1\n");
    fx.put("b.rs", "b-by-hand\n");
    let checkpoint = &fx.checkpoints()[0];
    let report = restore::rollback(&fx.store, checkpoint, false, None).unwrap();
    assert_eq!(fx.get("a.rs").as_deref(), Some("a0\n"));
    assert_eq!(fx.get("b.rs").as_deref(), Some("b-by-hand\n"));
    assert_eq!(report.restored, ["a.rs"]);
    assert_eq!(report.kept, ["b.rs"]);
    assert!(report.render().contains("kept (hand-edited): b.rs"));
    restore::rollback(&fx.store, checkpoint, true, Some("b.rs")).unwrap();
    assert_eq!(fx.get("b.rs").as_deref(), Some("b0\n"));
}

/// Scenario: Preview only.
#[test]
fn a_rollback_diff_changes_nothing() {
    let fx = Fixture::new();
    fx.put("src/lib.rs", "one\n");
    fx.patch("src/lib.rs", "one", "two");
    let listing = restore::listing(&fx.store, &fx.checkpoints());
    assert_eq!(
        (
            listing[0].number,
            listing[0].files_changed,
            listing[0].lines_changed
        ),
        (1, 1, 2)
    );
    let diff = restore::diff(&fx.store, &fx.checkpoints()[0]);
    assert!(diff.contains("-one\n+two\n"), "{diff}");
    assert_eq!(fx.get("src/lib.rs").as_deref(), Some("two\n"));
}

/// Scenario: Session diff.
#[test]
fn the_session_diff_lists_every_changed_file_with_counts() {
    let fx = Fixture::new();
    fx.put("a.rs", "one\ntwo\n");
    fx.patch("a.rs", "two", "three");
    fx.write("b.rs", "new\nfile\n");
    let request = worktree::Request::parse("session --stat");
    let text = worktree::diff(&fx.store, "ses_a", &fx.work, &request);
    assert!(text.contains("a.rs | +1 -1"), "{text}");
    assert!(text.contains("b.rs | +2 -0"), "{text}");
}

/// Scenario: Unmounted volume.
#[test]
fn the_automatic_prune_keeps_a_missing_workspace() {
    let fx = Fixture::new();
    let gone = fx.work.parent().unwrap().join("mounted");
    // A repository of its own: the project a checkpoint of it records.
    fs::create_dir_all(gone.join(".git")).unwrap();
    fs::write(gone.join("x.txt"), "x").unwrap();
    fx.snapshots.before_command("rm x.txt", &gone);
    fs::remove_dir_all(&gone).unwrap();
    let pruned = manage::auto_prune(
        &fx.store,
        Duration::from_secs(0),
        1 << 30,
        Duration::from_secs(0),
    )
    .unwrap()
    .unwrap();
    assert!(pruned.workspaces.is_empty());
    let status = manage::status(&fx.store, 20);
    assert!(status.workspaces.iter().any(|w| w.orphan && w.root == gone));
    let pruned = manage::prune(
        &fx.store,
        PrunePolicy {
            retention: Duration::from_hours(24 * 7),
            max_bytes: 1 << 30,
            orphans: true,
        },
    )
    .unwrap();
    assert_eq!(pruned.workspaces, std::slice::from_ref(&gone));
}
