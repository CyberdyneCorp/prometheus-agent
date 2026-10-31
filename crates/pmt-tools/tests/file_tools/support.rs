//! A session over a temporary user home with a workspace, an outside
//! directory and a Prometheus home with one profile, under the target
//! directory.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use pmt_core::capability::{BASELINE, CapabilitySet, Places};
use pmt_tools::files::{ControlPaths, FileSession, FileSetup, Linter, LocalBackend, ReadArgs};
use pmt_tools::result::{Status, ToolResult};
use serde_json::Value;

pub(crate) struct Fixture {
    _root: tempfile::TempDir,
    pub(crate) home: PathBuf,
    pub(crate) work: PathBuf,
    pub(crate) outside: PathBuf,
    pub(crate) prometheus: PathBuf,
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
        let base = dunce::canonicalize(root.path()).unwrap();
        let home = base.join("home");
        let work = home.join("work");
        let outside = base.join("outside");
        let prometheus = home.join(".prometheus");
        let profile = prometheus.join("profiles/default");
        for dir in [&work, &outside, &profile] {
            fs::create_dir_all(dir).unwrap();
        }
        for name in ["config.toml", "grants.toml", "secrets.toml"] {
            fs::write(profile.join(name), "# control\n").unwrap();
        }
        Self {
            _root: root,
            home,
            work,
            outside,
            prometheus,
        }
    }

    pub(crate) fn session(&self) -> FileSession {
        self.session_with(Linter::builtin(), &[])
    }

    /// A session with `linter` and capabilities beyond the fixture's.
    pub(crate) fn session_with(&self, linter: Linter, extra: &[&str]) -> FileSession {
        let places = Places {
            user_home: self.home.clone(),
            workspace: self.work.clone(),
            scratch: Vec::new(),
            toolchain: Vec::new(),
        };
        // The baseline plus write access to the Prometheus home, so the
        // control-file guard (not the capability check) is what refuses.
        let caps = BASELINE
            .iter()
            .map(|c| (*c).to_owned())
            .chain([format!("fs.write:{}/**", glob_root(&self.prometheus))])
            .chain([format!("fs.read:{}/**", glob_root(&self.outside))])
            .chain(extra.iter().map(|c| (*c).to_owned()));
        let capabilities = CapabilitySet::parse(caps).unwrap();
        let work = self.work.clone();
        FileSession::new(FileSetup {
            backend: Arc::new(LocalBackend::new(capabilities, places)),
            cwd: Arc::new(move || work.clone()),
            user_home: Some(self.home.clone()),
            control: ControlPaths {
                prometheus_home: self.prometheus.clone(),
            },
            read_max_chars: 100_000,
            linter,
        })
    }

    pub(crate) fn path(&self, rel: &str) -> PathBuf {
        self.work.join(rel)
    }

    pub(crate) fn write(&self, rel: &str, content: impl AsRef<[u8]>) -> PathBuf {
        let path = self.path(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        path
    }

    pub(crate) fn read(&self, rel: &str) -> String {
        fs::read_to_string(self.path(rel)).unwrap()
    }
}

/// The JSON body of a result.
pub(crate) fn body(result: &ToolResult) -> Value {
    serde_json::from_str(&result.text_content())
        .unwrap_or_else(|_| panic!("not JSON: {}", result.text_content()))
}

/// Reads the whole of `path` (asserting success).
pub(crate) fn read_all(session: &FileSession, path: &str) -> Value {
    let result = session.read_file(&ReadArgs::new(path));
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    body(&result)
}

/// A capability glob root: `/` separators on every platform.
fn glob_root(path: &std::path::Path) -> String {
    path.display().to_string().replace('\\', "/")
}
