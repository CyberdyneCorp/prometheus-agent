//! A user home with a git workspace, a Prometheus home and the stand-in
//! language server (`fake_lsp.py`, run by `python3`), in a temporary
//! directory outside any git repository (the target directory is inside
//! this one, which would make every path a workspace).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use pmt_config::schema::LspConfig;
use pmt_core::capability::{BASELINE, CapabilitySet, Places};
use pmt_tools::files::{ControlPaths, FileSession, FileSetup, Linter, LocalBackend};
use pmt_tools::lsp::install::Installs;
use pmt_tools::lsp::launch::{LaunchSpec, Launcher, Unconfined};
use pmt_tools::lsp::workspace::{TrustInputs, Workspaces};
use pmt_tools::lsp::{LspSetup, SessionDiagnostics, Toolchain};
use serde_json::Value;

pub(crate) struct Fixture {
    _root: tempfile::TempDir,
    pub(crate) base: PathBuf,
    pub(crate) home: PathBuf,
    /// A git working tree: the directory sessions are pointed at.
    pub(crate) app: PathBuf,
    pub(crate) prometheus: PathBuf,
    pub(crate) profile: PathBuf,
    /// What the stand-in server logs.
    pub(crate) log: PathBuf,
}

/// A git working tree at `dir`.
pub(crate) fn repo(dir: &Path) -> PathBuf {
    fs::create_dir_all(dir.join(".git")).unwrap();
    dunce::canonicalize(dir).unwrap()
}

impl Fixture {
    pub(crate) fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let base = dunce::canonicalize(root.path()).unwrap();
        let home = base.join("home");
        let app = repo(&home.join("work/app"));
        let prometheus = home.join(".prometheus");
        let profile = prometheus.join("profiles/default");
        fs::create_dir_all(&profile).unwrap();
        Self {
            _root: root,
            log: base.join("server.log"),
            base,
            home,
            app,
            prometheus,
            profile,
        }
    }

    /// The stand-in server's argv.
    pub(crate) fn server_command() -> String {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/lsp/fake_lsp.py");
        format!("[\"python3\", \"{}\"]", script.display())
    }

    /// `[lsp]` with `pyright` played by the stand-in, then `extra`
    /// (more keys of `[lsp]`, then tables).
    pub(crate) fn config(&self, extra: &str) -> LspConfig {
        self.config_for("pyright", "", "", extra)
    }

    /// `[lsp]` keys `extra`, with server `id` played by the stand-in: `env`
    /// is more of its environment (a TOML inline-table body), `table` more
    /// keys of its `[lsp.servers.<id>]` table.
    pub(crate) fn config_for(&self, id: &str, env: &str, table: &str, extra: &str) -> LspConfig {
        let sep = if env.is_empty() { "" } else { ", " };
        let text = format!(
            "wait_timeout_ms = 4000\n{extra}\n[servers.{id}]\ncommand = {}\nenv = {{ FAKE_LSP_LOG = \"{}\"{sep}{env} }}\n{table}\n",
            Self::server_command(),
            self.log.display(),
        );
        toml::from_str(&text).unwrap_or_else(|e| panic!("{e}\n{text}"))
    }

    pub(crate) fn diagnostics(&self, config: LspConfig, start: &Path) -> SessionDiagnostics {
        self.diagnostics_with(config, start, true, Arc::new(Unconfined))
    }

    pub(crate) fn diagnostics_with(
        &self,
        config: LspConfig,
        start: &Path,
        automatic: bool,
        launcher: Arc<dyn Launcher>,
    ) -> SessionDiagnostics {
        let workspaces = Workspaces::new(&TrustInputs {
            start_dir: start,
            automatic,
            user_home: Some(&self.home),
            exclude_roots: &config.exclude_roots,
            trusted_workspaces: &config.trusted_workspaces,
        });
        let setup = LspSetup {
            installs: Installs::of_home(&self.prometheus, std::env::var_os("PATH")),
            config,
            profile_dir: self.profile.clone(),
            session_id: "ses_lsp".to_owned(),
            workspaces,
            launcher,
            toolchain: Toolchain::default(),
            handle: tokio::runtime::Handle::current(),
        };
        SessionDiagnostics::new(setup, true).expect("lsp enabled")
    }

    /// File tools at `cwd` (writes allowed anywhere under the fixture)
    /// with `lsp` after writes.
    pub(crate) fn files(&self, cwd: &Path, lsp: Option<SessionDiagnostics>) -> Arc<FileSession> {
        let places = Places {
            user_home: self.home.clone(),
            workspace: cwd.to_path_buf(),
            scratch: Vec::new(),
            toolchain: Vec::new(),
        };
        let caps = BASELINE.iter().map(|c| (*c).to_owned()).chain([
            format!("fs.write:{}/**", self.base.display()),
            format!("fs.read:{}/**", self.base.display()),
        ]);
        let cwd = cwd.to_path_buf();
        let files = FileSession::new(FileSetup {
            backend: Arc::new(LocalBackend::new(
                CapabilitySet::parse(caps).unwrap(),
                places,
            )),
            cwd: Arc::new(move || cwd.clone()),
            user_home: Some(self.home.clone()),
            control: ControlPaths {
                prometheus_home: self.prometheus.clone(),
            },
            read_max_chars: pmt_tools::setting::fixed(100_000),
            linter: Linter::builtin(),
            documents: None,
            edits: None,
        });
        Arc::new(match lsp {
            Some(lsp) => files.with_post_write(Arc::new(lsp)),
            None => files,
        })
    }

    pub(crate) fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    /// How many times the stand-in server started.
    pub(crate) fn starts(&self) -> usize {
        self.log()
            .lines()
            .filter(|l| l.starts_with("start "))
            .count()
    }
}

/// An unconfined launcher that counts launches. The stand-in only logs its
/// start once Python is up, which on a slow runner can come after a short
/// warm-up timeout killed it; a launch is counted when it happens.
#[derive(Default)]
pub(crate) struct CountingLauncher {
    launches: AtomicUsize,
}

impl CountingLauncher {
    pub(crate) fn launches(&self) -> usize {
        self.launches.load(Ordering::SeqCst)
    }
}

impl Launcher for CountingLauncher {
    fn command(&self, spec: &LaunchSpec<'_>) -> Result<tokio::process::Command, String> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        Unconfined.command(spec)
    }
}

/// `write_file` off the runtime (it blocks while it waits for a verdict).
pub(crate) async fn write(files: &Arc<FileSession>, path: &Path, content: &str) -> Value {
    let files = Arc::clone(files);
    let (path, content) = (path.display().to_string(), content.to_owned());
    pmt_core::spawn::blocking(move || {
        let result = files.write_file(&path, &content);
        serde_json::from_str(&result.text_content()).unwrap()
    })
    .await
    .unwrap()
}

/// A `patch` replacing `old` with `new` in `path`, off the runtime.
pub(crate) async fn patch(files: &Arc<FileSession>, path: &Path, old: &str, new: &str) -> Value {
    let files = Arc::clone(files);
    let args = serde_json::json!({"path": path.display().to_string(), "old_string": old, "new_string": new});
    pmt_core::spawn::blocking(move || {
        let edit = pmt_tools::files::patch::PatchArgs::from_value(&args).unwrap();
        let result = files.patch(&edit);
        serde_json::from_str(&result.text_content()).unwrap()
    })
    .await
    .unwrap()
}

/// `read_file` off the runtime (marks a file read so it can be patched).
pub(crate) async fn read(files: &Arc<FileSession>, path: &Path) {
    let files = Arc::clone(files);
    let path = path.display().to_string();
    pmt_core::spawn::blocking(move || {
        let _ = files.read_file(&pmt_tools::files::ReadArgs::new(&path));
    })
    .await
    .unwrap();
}

/// The findings of a result, inline or (when the verdict missed the
/// write's budget) delivered before the turn ends.
pub(crate) async fn findings(result: &Value, lsp: &SessionDiagnostics) -> String {
    let mut lines: Vec<String> = result["lsp_diagnostics"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|i| i.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    lines.extend(lsp.settle().await);
    lines.join("\n")
}

/// Waits until `check` holds, at most 10 s.
pub(crate) async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !check() {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}
