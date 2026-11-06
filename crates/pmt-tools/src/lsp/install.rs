//! Finding and installing server binaries (spec: lsp-diagnostics, Server
//! installation).
//!
//! `npm`, `go` and `pip` servers install into
//! `<PROMETHEUS_HOME>/runtimes/lsp/<id>/`: npm packages with the managed
//! Node (or the configured `pnpm`/`yarn`, never another manager), Go
//! modules with `go install`, pip packages into a private virtualenv of
//! the managed CPython. Package scripts never run (`--ignore-scripts`).
//! `manual` servers and custom servers are never installed.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use pmt_config::schema::{LspInstallStrategy, LspPackageManager};

use super::registry::{Install, Server};

/// Written into an install directory once its install succeeded.
const COMPLETE: &str = ".prometheus-complete";

/// Where a server's binary came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `lsp.servers.<id>.command`.
    Command,
    /// `<PROMETHEUS_HOME>/runtimes/lsp/<id>/`.
    Managed,
    /// `PATH`.
    Path,
}

impl Source {
    /// `command`, `managed` or `path`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Command => "command",
            Self::Managed => "managed",
            Self::Path => "path",
        }
    }
}

/// A server binary found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The executable.
    pub program: PathBuf,
    /// The arguments after it.
    pub args: Vec<String>,
    /// Where it came from.
    pub source: Source,
}

/// The install directories of a home and where binaries are looked up.
#[derive(Debug, Clone)]
pub struct Installs {
    /// `<PROMETHEUS_HOME>/runtimes/lsp`.
    pub root: PathBuf,
    /// The `PATH` binaries are searched on.
    pub path_var: Option<OsString>,
}

impl Installs {
    /// The installs of the home at `home`.
    #[must_use]
    pub fn of_home(home: &Path, path_var: Option<OsString>) -> Self {
        Self {
            root: home.join("runtimes").join("lsp"),
            path_var,
        }
    }

    /// The install directory of `id`.
    #[must_use]
    pub fn dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    /// The binary of `server` in its install directory, once installed.
    #[must_use]
    pub fn managed(&self, server: &Server) -> Option<PathBuf> {
        let dir = self.dir(&server.id);
        if !dir.join(COMPLETE).is_file() {
            return None;
        }
        let program = server.program();
        let candidate = match server.install {
            Install::Npm { .. } => dir.join("node_modules").join(".bin").join(npm_bin(program)),
            Install::Go { .. } => dir.join("bin").join(exe(program)),
            Install::Pip { .. } => venv_bin(&dir.join("venv")).join(exe(program)),
            Install::Manual { .. } => return None,
        };
        candidate.is_file().then_some(candidate)
    }

    /// `name` on `PATH`.
    #[must_use]
    pub fn on_path(&self, name: &str) -> Option<PathBuf> {
        let path_var = self.path_var.as_deref()?;
        let pathext = std::env::var_os("PATHEXT");
        crate::terminal::shell::find_executable(name, path_var, pathext.as_deref())
    }

    /// The binary `server` runs: its configured command, else (with
    /// `auto`) its managed install, else `PATH`.
    #[must_use]
    pub fn resolve(&self, server: &Server, strategy: LspInstallStrategy) -> Option<Resolved> {
        let args = server.command.iter().skip(1).cloned().collect();
        let program = server.program();
        if server.command_overridden {
            let path = Path::new(program);
            let found = if path.components().count() > 1 {
                path.is_file().then(|| path.to_path_buf())
            } else {
                self.on_path(program)
            };
            return found.map(|program| Resolved {
                program,
                args,
                source: Source::Command,
            });
        }
        let managed = (strategy == LspInstallStrategy::Auto)
            .then(|| self.managed(server))
            .flatten()
            .map(|program| (program, Source::Managed));
        managed
            .or_else(|| self.on_path(program).map(|p| (p, Source::Path)))
            .map(|(program, source)| Resolved {
                program,
                args,
                source,
            })
    }
}

/// What an install runs with.
pub struct Tools<'a> {
    /// `lsp.package_manager`.
    pub manager: LspPackageManager,
    /// The managed Node (npm installs, and running npm servers).
    pub node: Option<&'a Path>,
    /// The managed CPython (pip installs).
    pub python: Option<&'a Path>,
}

impl Installs {
    /// Installs `server` and returns its binary.
    ///
    /// # Errors
    ///
    /// Why it was not installed: a `manual` or custom server, a missing
    /// installer (the configured package manager, `go`, Python), or the
    /// installer's failure.
    pub async fn install(&self, server: &Server, tools: &Tools<'_>) -> Result<PathBuf, String> {
        if server.custom {
            return Err(format!(
                "{} is a custom server: install it yourself",
                server.id
            ));
        }
        let dir = self.dir(&server.id);
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let _ = std::fs::remove_file(dir.join(COMPLETE));
        match server.install {
            Install::Npm { package, extra } => self.npm(&dir, package, extra, tools).await?,
            Install::Go { module } => self.go(&dir, module).await?,
            Install::Pip { package } => self.pip(&dir, package, tools).await?,
            Install::Manual { hint } => {
                return Err(format!("{} is installed manually: {hint}", server.id));
            }
        }
        std::fs::write(dir.join(COMPLETE), b"").map_err(|e| e.to_string())?;
        self.managed(server).ok_or_else(|| {
            format!(
                "the install of {} did not provide {}",
                server.id,
                server.program()
            )
        })
    }

    async fn npm(
        &self,
        dir: &Path,
        package: &str,
        extra: &[&str],
        tools: &Tools<'_>,
    ) -> Result<(), String> {
        std::fs::write(dir.join("package.json"), "{\"private\": true}\n")
            .map_err(|e| e.to_string())?;
        let packages = std::iter::once(package).chain(extra.iter().copied());
        let mut command = match tools.manager {
            LspPackageManager::Npm => self.npm_command(tools.node)?,
            other => {
                let name = other.as_str();
                let program = self.on_path(name).ok_or_else(|| {
                    format!(
                        "skipped: lsp.package_manager = \"{name}\" but {name} is not installed; install {name} or set lsp.package_manager = \"npm\""
                    )
                })?;
                tokio::process::Command::new(program)
            }
        };
        match tools.manager {
            LspPackageManager::Npm => command.arg("install").arg("--prefix").arg(dir).args([
                "--no-audit",
                "--no-fund",
                "--ignore-scripts",
            ]),
            LspPackageManager::Pnpm => command
                .arg("add")
                .arg("--dir")
                .arg(dir)
                .arg("--ignore-scripts"),
            LspPackageManager::Yarn => command
                .arg("add")
                .arg("--cwd")
                .arg(dir)
                .arg("--ignore-scripts"),
        };
        command.args(packages).current_dir(dir);
        command.env("PATH", self.path_with(tools.node.and_then(Path::parent)));
        run(command, tools.manager.as_str()).await
    }

    /// npm itself: the managed Node's bundled npm, else `npm` on `PATH`.
    fn npm_command(&self, node: Option<&Path>) -> Result<tokio::process::Command, String> {
        if let Some((node, cli)) = node.and_then(|node| npm_cli(node).map(|cli| (node, cli))) {
            let mut command = tokio::process::Command::new(node);
            command.arg(cli);
            return Ok(command);
        }
        self.on_path("npm")
            .map(tokio::process::Command::new)
            .ok_or_else(|| {
                "skipped: npm is not installed and the managed Node is unavailable".to_owned()
            })
    }

    async fn go(&self, dir: &Path, module: &str) -> Result<(), String> {
        let go = self
            .on_path("go")
            .ok_or("skipped: go is not installed; gopls installs with `go install`")?;
        let mut command = tokio::process::Command::new(go);
        command
            .args(["install", module])
            .env("GOBIN", dir.join("bin"))
            .current_dir(dir);
        run(command, "go install").await
    }

    async fn pip(&self, dir: &Path, package: &str, tools: &Tools<'_>) -> Result<(), String> {
        let python = tools
            .python
            .map(Path::to_path_buf)
            .or_else(|| self.on_path("python3"))
            .ok_or("skipped: no Python to create the server's virtualenv")?;
        let venv = dir.join("venv");
        let mut create = tokio::process::Command::new(python);
        create.args(["-m", "venv"]).arg(&venv);
        run(create, "python -m venv").await?;
        let mut pip = tokio::process::Command::new(venv_bin(&venv).join(exe("python")));
        pip.args([
            "-m",
            "pip",
            "install",
            "--disable-pip-version-check",
            package,
        ]);
        run(pip, "pip install").await
    }

    /// `PATH` with `first` in front.
    fn path_with(&self, first: Option<&Path>) -> OsString {
        let rest = self.path_var.clone().unwrap_or_default();
        let Some(first) = first else { return rest };
        let dirs = std::iter::once(first.to_path_buf()).chain(std::env::split_paths(&rest));
        std::env::join_paths(dirs).unwrap_or(rest)
    }
}

/// The npm CLI shipped with a Node runtime whose executable is `node`.
fn npm_cli(node: &Path) -> Option<PathBuf> {
    let bin = node.parent()?;
    [
        bin.join("../lib/node_modules/npm/bin/npm-cli.js"),
        bin.join("node_modules/npm/bin/npm-cli.js"),
    ]
    .into_iter()
    .find(|p| p.is_file())
}

/// Runs an installer, reporting the tail of its output on failure.
async fn run(mut command: tokio::process::Command, what: &str) -> Result<(), String> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = command
        .output()
        .await
        .map_err(|e| format!("cannot run {what}: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let mut text = String::from_utf8_lossy(&output.stderr).into_owned();
    if text.trim().is_empty() {
        text = String::from_utf8_lossy(&output.stdout).into_owned();
    }
    let tail: Vec<&str> = text.lines().rev().take(5).collect();
    let tail: Vec<&str> = tail.into_iter().rev().collect();
    Err(format!(
        "{what} failed ({}): {}",
        output.status,
        tail.join("\n")
    ))
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    }
}

fn npm_bin(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.cmd")
    } else {
        name.to_owned()
    }
}

fn venv_bin(venv: &Path) -> PathBuf {
    if cfg!(windows) {
        venv.join("Scripts")
    } else {
        venv.join("bin")
    }
}

/// `PATH` for running an npm-installed server: the Node beside it first.
#[must_use]
pub fn node_path(node: Option<&Path>, path_var: Option<&OsStr>) -> Option<OsString> {
    let rest = path_var.map(OsStr::to_os_string).unwrap_or_default();
    let dir = node?.parent()?.to_path_buf();
    let dirs = std::iter::once(dir).chain(std::env::split_paths(&rest));
    std::env::join_paths(dirs).ok()
}
