//! File snapshots of sessions on non-local backends (spec:
//! execution-environments, File snapshots and undo; Snapshot triggers,
//! caps and per-run toggle; Rollback listing, diff and selective
//! restore): a container whose workspace is a host bind mount is
//! snapshotted on the host, and a backend with its own filesystem (SSH, a
//! plugin sandbox) is read, checkpointed and restored through its file
//! backend. Both backends here are in-process stand-ins with the same
//! path semantics as the real ones.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_core::capability::Access;
use pmt_tools::files::edits::{EditCall, with_call};
use pmt_tools::files::patch::PatchArgs;
use pmt_tools::files::{
    ControlPaths, Entry, FileBackend, FileSession, FileSetup, Kind, Linter, Meta,
};
use pmt_tools::snapshots::{
    BackendTree, HostTree, MountedTree, SessionSnapshots, Settings, SnapshotStore, Tree, restore,
};

/// A backend's own filesystem, in memory: absolute paths to contents.
#[derive(Debug, Default)]
struct Remote(Mutex<BTreeMap<PathBuf, Vec<u8>>>);

impl Remote {
    fn files(&self) -> std::sync::MutexGuard<'_, BTreeMap<PathBuf, Vec<u8>>> {
        self.0.lock().unwrap()
    }
}

fn missing() -> io::Error {
    io::Error::from(io::ErrorKind::NotFound)
}

impl FileBackend for Remote {
    fn name(&self) -> &'static str {
        "ssh"
    }

    fn uses_host_filesystem(&self) -> bool {
        false
    }

    fn metadata(&self, path: &Path) -> io::Result<Meta> {
        let files = self.files();
        let (kind, len) = if let Some(bytes) = files.get(path) {
            (Kind::File, bytes.len() as u64)
        } else if files.keys().any(|f| f.starts_with(path)) {
            (Kind::Dir, 0)
        } else {
            return Err(missing());
        };
        Ok(Meta {
            kind,
            len,
            modified: None,
        })
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.files().get(path).cloned().ok_or_else(missing)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.files().insert(path.to_path_buf(), bytes.to_vec());
        Ok(())
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        self.files().remove(path).map(drop).ok_or_else(missing)
    }

    fn list_dir(&self, path: &Path) -> io::Result<Vec<Entry>> {
        let files = self.files();
        let mut entries: BTreeMap<String, Entry> = BTreeMap::new();
        for (file, bytes) in files.iter() {
            let Ok(rel) = file.strip_prefix(path) else {
                continue;
            };
            let mut parts = rel.components();
            let Some(first) = parts.next() else { continue };
            let name = first.as_os_str().to_string_lossy().into_owned();
            let (kind, len) = if parts.next().is_some() {
                (Kind::Dir, 0)
            } else {
                (Kind::File, bytes.len() as u64)
            };
            entries.insert(name.clone(), Entry { name, kind, len });
        }
        Ok(entries.into_values().collect())
    }

    fn check(&self, _access: Access, _path: &Path) -> io::Result<()> {
        Ok(())
    }
}

/// A container's view of a host directory bind-mounted at `/workspace`.
#[derive(Debug)]
struct Mounted {
    host: PathBuf,
}

impl Mounted {
    fn host_of(&self, path: &Path) -> io::Result<PathBuf> {
        path.strip_prefix("/workspace")
            .map(|rel| self.host.join(rel))
            .map_err(|_| missing())
    }
}

impl FileBackend for Mounted {
    fn name(&self) -> &'static str {
        "container"
    }

    fn uses_host_filesystem(&self) -> bool {
        false
    }

    fn metadata(&self, path: &Path) -> io::Result<Meta> {
        let meta = std::fs::metadata(self.host_of(path)?)?;
        Ok(Meta {
            kind: if meta.is_dir() { Kind::Dir } else { Kind::File },
            len: meta.len(),
            modified: None,
        })
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(self.host_of(path)?)
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let host = self.host_of(path)?;
        std::fs::create_dir_all(host.parent().unwrap())?;
        std::fs::write(host, bytes)
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        std::fs::remove_file(self.host_of(path)?)
    }

    fn list_dir(&self, _path: &Path) -> io::Result<Vec<Entry>> {
        unreachable!("snapshots of a mounted workspace walk the host")
    }

    fn check(&self, _access: Access, _path: &Path) -> io::Result<()> {
        Ok(())
    }
}

struct Session {
    _root: tempfile::TempDir,
    store: SnapshotStore,
    snapshots: Arc<SessionSnapshots>,
    files: FileSession,
}

impl Session {
    /// A session whose file tools use `backend` in `cwd`, its snapshots
    /// kept in `tree` with workspace `workspace`.
    fn new(
        backend: Arc<dyn FileBackend>,
        cwd: &str,
        tree: Arc<dyn Tree>,
        workspace: &Path,
    ) -> Self {
        let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let store = SnapshotStore::of_profile(&root.path().join("profile"));
        let settings = Settings {
            enabled: true,
            terminal: true,
            max_file_bytes: 1024,
            max_checkpoints: 20,
            retention: Duration::from_hours(24),
            max_bytes: 1 << 30,
            prune_interval: Duration::from_hours(24),
        };
        let snapshots = Arc::new(SessionSnapshots::new(
            store.clone(),
            tree,
            "ses_r",
            workspace,
            settings,
            Arc::new(|| 1),
            Vec::new(),
        ));
        let cwd = PathBuf::from(cwd);
        let files = FileSession::new(FileSetup {
            backend,
            cwd: Arc::new(move || cwd.clone()),
            user_home: None,
            control: ControlPaths {
                prometheus_home: root.path().join("home"),
            },
            read_max_chars: pmt_tools::setting::fixed(100_000),
            linter: Linter::builtin(),
            documents: None,
            edits: Some(Arc::clone(&snapshots) as _),
        });
        Self {
            _root: root,
            store,
            snapshots,
            files,
        }
    }

    /// `write_file` of a new file.
    fn create(&self, path: &str, content: &str) {
        self.check("write_file", |files| files.write_file(path, content));
    }

    /// `patch` of an existing file.
    fn patch(&self, path: &str, old: &str, new: &str) {
        let args = PatchArgs::from_value(&serde_json::json!({
            "path": path, "old_string": old, "new_string": new
        }))
        .unwrap();
        self.check("patch", |files| files.patch(&args));
    }

    fn check(&self, tool: &str, run: impl FnOnce(&FileSession) -> pmt_tools::result::ToolResult) {
        let call = EditCall {
            call_id: "call_1".into(),
            tool: tool.into(),
        };
        let result = with_call(call, || run(&self.files));
        assert_eq!(
            result.status,
            pmt_tools::result::Status::Ok,
            "{}",
            result.text_content()
        );
    }

    fn undo(&self, tree: &dyn Tree) -> restore::UndoReport {
        restore::undo(&self.store, tree, "ses_r", Some(1), 1, false, false).unwrap()
    }
}

/// Scenario: Undo last turn, on a backend with its own filesystem: the
/// prior content is read, checkpointed and put back through the backend;
/// this host's tree never touches it.
#[test]
fn a_remote_backend_session_is_snapshotted_and_undone_through_its_backend() {
    let remote = Arc::new(Remote::default());
    remote
        .write(Path::new("/srv/app/src/lib.rs"), b"fn a() {}\n")
        .unwrap();
    remote
        .write(Path::new("/srv/app/README"), b"readme\n")
        .unwrap();
    let tree: Arc<dyn Tree> = Arc::new(BackendTree::new("ssh", Arc::clone(&remote) as _));
    let session = Session::new(
        Arc::clone(&remote) as _,
        "/srv/app",
        Arc::clone(&tree),
        Path::new("/srv/app"),
    );
    session.patch("src/lib.rs", "fn a() {}", "fn b() {}");
    session.create("src/new.rs", "new\n");

    let checkpoints = restore::checkpoints(&session.store, &*tree, Path::new("/srv/app"));
    assert_eq!(checkpoints.len(), 1, "one checkpoint per project per turn");
    let kept: Vec<&str> = checkpoints[0]
        .files
        .iter()
        .map(|f| f.path.as_str())
        .collect();
    assert_eq!(kept, ["README", "src/lib.rs"]);
    // Not a host project: the host never lists it.
    assert!(restore::checkpoints(&session.store, &HostTree, Path::new("/srv/app")).is_empty());

    // An undo from this host refuses instead of writing host paths.
    let refused = session.undo(&HostTree);
    assert!(
        refused
            .conflicts
            .iter()
            .any(|c| c.contains("is on the ssh backend")),
        "{refused:?}"
    );
    assert_eq!(
        remote.read(Path::new("/srv/app/src/lib.rs")).unwrap(),
        b"fn b() {}\n"
    );

    let report = session.undo(session.snapshots.tree());
    assert!(report.conflicts.is_empty(), "{report:?}");
    assert_eq!(
        remote.read(Path::new("/srv/app/src/lib.rs")).unwrap(),
        b"fn a() {}\n"
    );
    assert!(remote.read(Path::new("/srv/app/src/new.rs")).is_err());
}

/// Scenario: Rollback to a checkpoint, on a backend with its own
/// filesystem: the destructive command's checkpoint restores the files it
/// removed.
#[test]
fn a_destructive_remote_command_is_checkpointed_and_rolled_back() {
    let remote = Arc::new(Remote::default());
    remote
        .write(Path::new("/srv/app/data.txt"), b"keep me\n")
        .unwrap();
    remote
        .write(Path::new("/srv/app/README"), b"readme\n")
        .unwrap();
    let tree: Arc<dyn Tree> = Arc::new(BackendTree::new("ssh", Arc::clone(&remote) as _));
    let session = Session::new(
        Arc::clone(&remote) as _,
        "/srv/app",
        Arc::clone(&tree),
        Path::new("/srv/app"),
    );
    session
        .snapshots
        .before_command("rm -rf data.txt", Path::new("/srv/app"));
    remote.remove(Path::new("/srv/app/data.txt")).unwrap();

    let checkpoints = restore::checkpoints(&session.store, &*tree, Path::new("/srv/app"));
    assert_eq!(checkpoints.len(), 1);
    let report = restore::rollback(&session.store, &*tree, &checkpoints[0], false, None).unwrap();
    assert_eq!(report.restored, ["data.txt"]);
    assert_eq!(
        remote.read(Path::new("/srv/app/data.txt")).unwrap(),
        b"keep me\n"
    );
}

/// Scenario: Undo last turn, in a container whose workspace is a host
/// bind mount: the change is recorded on the host path, so the session
/// and the host CLI undo it alike.
#[test]
fn a_mounted_container_workspace_is_snapshotted_on_the_host() {
    let host = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let host = dunce::canonicalize(host.path()).unwrap().join("project");
    std::fs::create_dir_all(host.join("src")).unwrap();
    std::fs::write(host.join("src/lib.rs"), "fn a() {}\n").unwrap();
    let tree: Arc<dyn Tree> = Arc::new(MountedTree::new(Path::new("/workspace"), &host));
    let session = Session::new(
        Arc::new(Mounted { host: host.clone() }),
        "/workspace",
        tree,
        &host,
    );
    session.patch("src/lib.rs", "fn a() {}", "fn b() {}");
    assert_eq!(
        std::fs::read_to_string(host.join("src/lib.rs")).unwrap(),
        "fn b() {}\n"
    );

    let checkpoints = restore::checkpoints(&session.store, &HostTree, &host);
    assert_eq!(checkpoints.len(), 1, "the host project was checkpointed");
    assert_eq!(checkpoints[0].root, host);

    // `prometheus files undo` on the host reaches the same files.
    let report = session.undo(&HostTree);
    assert!(report.conflicts.is_empty(), "{report:?}");
    assert_eq!(report.restored, [host.join("src/lib.rs")]);
    assert_eq!(
        std::fs::read_to_string(host.join("src/lib.rs")).unwrap(),
        "fn a() {}\n"
    );
}
