//! Editor-native files and terminals of ACP sessions (spec: acp-adapter,
//! Editor-native file access; Editor terminal delegation).
//!
//! An ACP session whose editor reads and writes files for the agent sees
//! the editor's buffers: its file tools go through [`EditorBackend`],
//! which hands reads and writes of paths under the session's working
//! directory to the session's [`EditorFs`] bridge and everything else (and
//! everything while no bridge answers) to the session's own backend. Its
//! terminal hands foreground commands to the session's
//! [`ForegroundDelegate`] when the host bound one. The host (the daemon)
//! binds both per session id, so a moved working directory or a rebuilt
//! environment keeps them; a bridge that answers `None` (no editor with
//! the capability attached) leaves the call to the session itself.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use pmt_core::capability::Access;

use crate::files::CwdSource;
use crate::files::backend::{Entry, FileBackend, Meta};

/// The editor side of a session's file reads and writes.
pub trait EditorFs: Send + Sync {
    /// The editor's text of `path`; `None` when no editor can read for
    /// the session now. Called on a blocking thread.
    fn read(&self, path: &Path) -> Option<io::Result<String>>;
    /// Writes `text` to `path` through the editor; `None` as for
    /// [`EditorFs::read`]. Called on a blocking thread.
    fn write(&self, path: &Path, text: &str) -> Option<io::Result<()>>;
}

/// A foreground command for the editor's terminal.
#[derive(Debug, Clone, Copy)]
pub struct Delegated<'a> {
    /// The command line.
    pub command: &'a str,
    /// Its working directory.
    pub cwd: &'a Path,
    /// The most bytes of output to keep.
    pub output_bytes: u64,
    /// How long it may run.
    pub timeout_secs: u64,
}

/// What a delegated command did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DelegateOutcome {
    /// What it printed (the retained tail).
    pub output: String,
    /// The editor dropped output beyond the limit.
    pub truncated: bool,
    /// Its exit status, when it exited.
    pub exit_code: Option<i32>,
    /// Why the editor could not run it.
    pub error: Option<String>,
}

/// Future of a delegated command.
pub type DelegateFuture<'a> =
    std::pin::Pin<Box<dyn Future<Output = Option<DelegateOutcome>> + Send + 'a>>;

/// Runs a session's foreground commands elsewhere (the editor's terminal).
pub trait ForegroundDelegate: Send + Sync {
    /// Runs `call`; `None` leaves it to the session's own terminal.
    fn run<'a>(&'a self, call: Delegated<'a>) -> DelegateFuture<'a>;
}

#[derive(Default)]
struct Bound {
    fs: HashMap<String, Arc<dyn EditorFs>>,
    terminal: HashMap<String, Arc<dyn ForegroundDelegate>>,
}

static BOUND: LazyLock<Mutex<Bound>> = LazyLock::new(Mutex::default);

fn bound() -> std::sync::MutexGuard<'static, Bound> {
    BOUND.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Binds session `session`'s editor file bridge.
pub fn bind_fs(session: &str, fs: Arc<dyn EditorFs>) {
    bound().fs.insert(session.to_owned(), fs);
}

/// Binds session `session`'s terminal delegate.
pub fn bind_terminal(session: &str, delegate: Arc<dyn ForegroundDelegate>) {
    bound().terminal.insert(session.to_owned(), delegate);
}

/// Forgets session `session`'s bridges.
pub fn unbind(session: &str) {
    let mut bound = bound();
    bound.fs.remove(session);
    bound.terminal.remove(session);
}

/// Session `session`'s terminal delegate.
#[must_use]
pub fn terminal(session: &str) -> Option<Arc<dyn ForegroundDelegate>> {
    bound().terminal.get(session).cloned()
}

fn fs(session: &str) -> Option<Arc<dyn EditorFs>> {
    bound().fs.get(session).cloned()
}

/// A session's file backend that reads and writes files under its
/// working directory through the editor when one can.
pub struct EditorBackend {
    inner: Arc<dyn FileBackend>,
    session: String,
    cwd: CwdSource,
}

impl EditorBackend {
    /// `inner` (the session's local backend) with session `session`'s
    /// editor bridge in front of it for paths under `cwd`.
    #[must_use]
    pub fn new(inner: Arc<dyn FileBackend>, session: &str, cwd: CwdSource) -> Self {
        Self {
            inner,
            session: session.to_owned(),
            cwd,
        }
    }

    /// The bridge for `path`, when it is under the working directory.
    fn bridge(&self, path: &Path) -> Option<Arc<dyn EditorFs>> {
        path.starts_with((self.cwd)())
            .then(|| fs(&self.session))
            .flatten()
    }
}

impl FileBackend for EditorBackend {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn uses_host_filesystem(&self) -> bool {
        self.inner.uses_host_filesystem()
    }

    fn metadata(&self, path: &Path) -> io::Result<Meta> {
        self.inner.metadata(path)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.inner.check(Access::Read, path)?;
        match self.bridge(path).and_then(|fs| fs.read(path)) {
            Some(text) => text.map(String::into_bytes),
            None => self.inner.read(path),
        }
    }

    fn write(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        self.inner.check(Access::Write, path)?;
        let text = std::str::from_utf8(bytes).ok();
        match text.and_then(|text| self.bridge(path)?.write(path, text)) {
            Some(done) => done,
            None => self.inner.write(path, bytes),
        }
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        self.inner.remove(path)
    }

    fn list_dir(&self, path: &Path) -> io::Result<Vec<Entry>> {
        self.inner.list_dir(path)
    }

    fn check(&self, access: Access, path: &Path) -> io::Result<()> {
        self.inner.check(access, path)
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use super::{EditorBackend, EditorFs, bind_fs, unbind};
    use crate::files::backend::FileBackend;

    struct Buffers;

    impl EditorFs for Buffers {
        fn read(&self, path: &Path) -> Option<io::Result<String>> {
            if path.ends_with("broken.rs") {
                return Some(Err(io::Error::other("buffer closed")));
            }
            Some(Ok("unsaved".to_owned()))
        }

        fn write(&self, _: &Path, _: &str) -> Option<io::Result<()>> {
            Some(Ok(()))
        }
    }

    /// A backend reading anything under `home`, working in `home/work`.
    fn backend(home: &Path, session: &str) -> EditorBackend {
        let capabilities = pmt_core::capability::CapabilitySet::parse(["fs.read:~/**"]).unwrap();
        let places = pmt_core::capability::Places {
            user_home: home.to_path_buf(),
            workspace: home.join("work"),
            scratch: Vec::new(),
            toolchain: Vec::new(),
        };
        let local = crate::files::backend::LocalBackend::new(capabilities, places);
        let cwd = home.join("work");
        EditorBackend::new(Arc::new(local), session, Arc::new(move || cwd.clone()))
    }

    #[test]
    fn reads_under_the_workspace_see_the_editor_buffer_and_others_the_disk() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("work")).unwrap();
        let inside: PathBuf = home.path().join("work/main.rs");
        std::fs::write(&inside, "on disk").unwrap();
        let other = home.path().join("notes.txt");
        std::fs::write(&other, "elsewhere").unwrap();
        let files = backend(home.path(), "ses_editor_test");
        assert_eq!(files.read(&inside).unwrap(), b"on disk");
        bind_fs("ses_editor_test", Arc::new(Buffers));
        assert_eq!(files.read(&inside).unwrap(), b"unsaved");
        assert_eq!(files.read(&other).unwrap(), b"elsewhere");
        let broken = home.path().join("work/broken.rs");
        std::fs::write(&broken, "x").unwrap();
        let err = files.read(&broken).unwrap_err();
        assert_eq!(err.to_string(), "buffer closed");
        unbind("ses_editor_test");
        assert_eq!(files.read(&inside).unwrap(), b"on disk");
    }
}
