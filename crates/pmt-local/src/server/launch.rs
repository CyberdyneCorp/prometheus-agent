//! How the managed `llama-server` is started: router mode (no model on the
//! command line) over a preset file listing every library model, bound to
//! loopback. llama.cpp itself keeps at most `--models-max` models resident,
//! loads a model on its first request, and with `--sleep-idle-seconds`
//! unloads one that had no request for that long, reloading it on the next
//! request.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use pmt_config::daemon_local::{LocalBackend, LocalRuntimeConfig};
use pmt_core::error::Error;

use crate::download::io_error;
use crate::engine::Installed;
use crate::library::Model;

/// The preset file's name in `<home>/run/`.
pub const PRESET_FILE: &str = "llamacpp-models.ini";
/// The server log's name in `<home>/logs/`.
pub const LOG_FILE: &str = "llamacpp.log";

/// Everything one server start needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// `llama-server`.
    pub exe: PathBuf,
    /// Shared-library directories of the build.
    pub library_dirs: Vec<PathBuf>,
    /// The build directory (recorded).
    pub engine: PathBuf,
    /// The build's backend.
    pub backend: LocalBackend,
    /// The loopback port.
    pub port: u16,
    /// The preset file.
    pub preset: PathBuf,
    /// `local_runtime.models_max`.
    pub models_max: u32,
    /// `local_runtime.idle_unload_secs`.
    pub idle_unload_secs: u64,
    /// Where the server's output goes.
    pub log: PathBuf,
}

impl Launch {
    /// The launch of `engine` under `config` for `home` (`port` resolved:
    /// never 0).
    #[must_use]
    pub fn new(home: &Path, engine: &Installed, config: &LocalRuntimeConfig, port: u16) -> Self {
        Self {
            exe: engine.exe(),
            library_dirs: engine.library_dirs(),
            engine: engine.dir.clone(),
            backend: engine.record.backend,
            port,
            preset: preset_path(home),
            models_max: config.models_max,
            idle_unload_secs: config.idle_unload_secs,
            log: home.join("logs").join(LOG_FILE),
        }
    }

    /// The command (without output redirection).
    #[must_use]
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.exe);
        command
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(self.port.to_string())
            .arg("--models-preset")
            .arg(&self.preset)
            .arg("--models-max")
            .arg(self.models_max.to_string())
            .arg("--sleep-idle-seconds")
            .arg(self.idle_unload_secs.to_string())
            .stdin(std::process::Stdio::null());
        if let Some(dir) = self.exe.parent() {
            command.current_dir(dir);
        }
        if let Some((name, value)) = library_path(&self.library_dirs) {
            command.env(name, value);
        }
        command
    }
}

/// The dynamic loader's search path with the build's directories first.
fn library_path(dirs: &[PathBuf]) -> Option<(&'static str, std::ffi::OsString)> {
    let name = match std::env::consts::OS {
        "linux" | "android" => "LD_LIBRARY_PATH",
        "windows" => "PATH",
        _ => return None,
    };
    if dirs.is_empty() {
        return None;
    }
    let mut paths: Vec<PathBuf> = dirs.to_vec();
    // The server inherits the rest of the search path from this process.
    if let Some(existing) = std::env::var_os(name) {
        paths.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(paths).ok().map(|v| (name, v))
}

/// `<home>/run/llamacpp-models.ini`.
#[must_use]
pub fn preset_path(home: &Path) -> PathBuf {
    pmt_core::runtime::run_dir(home).join(PRESET_FILE)
}

/// The preset text for `models` on `backend`.
#[must_use]
pub fn preset(models: &[Model], backend: LocalBackend) -> String {
    let mut out = String::from("version = 1\n\n[*]\n");
    if backend == LocalBackend::Cpu {
        out.push_str("n-gpu-layers = 0\n");
    }
    for model in models {
        let Some(weights) = model.weights() else {
            continue;
        };
        let _ = write!(
            out,
            "\n[{}]\nmodel = {}\nc = {}\n",
            model.id,
            weights.display(),
            model.context
        );
        if let Some(projector) = model.projector() {
            let _ = writeln!(out, "mmproj = {}", projector.display());
        }
    }
    out
}

/// Writes the preset of `models` for `home`.
///
/// # Errors
///
/// `runtime.io`.
pub fn write_preset(
    home: &Path,
    models: &[Model],
    backend: LocalBackend,
) -> Result<PathBuf, Error> {
    let path = preset_path(home);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| io_error(dir, &e))?;
    }
    std::fs::write(&path, preset(models, backend)).map_err(|e| io_error(&path, &e))?;
    Ok(path)
}

/// A free loopback port (the OS picks it).
///
/// # Errors
///
/// `runtime.io` when no socket can be bound.
pub fn free_port() -> Result<u16, Error> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| io_error(Path::new("127.0.0.1:0"), &e))?;
    listener
        .local_addr()
        .map(|a| a.port())
        .map_err(|e| io_error(Path::new("127.0.0.1:0"), &e))
}

#[cfg(test)]
mod tests {
    use pmt_config::daemon_local::LocalBackend;

    use super::preset;
    use crate::library::{Model, ModelFile, Role, Source};

    #[test]
    fn the_preset_lists_each_model_with_its_window_and_projector() {
        let model = Model {
            id: "qwen-x".to_owned(),
            source: Source::Catalog,
            files: vec![
                ModelFile {
                    path: "/m/qwen-x/w.gguf".into(),
                    role: Role::Weights,
                    size: 1,
                },
                ModelFile {
                    path: "/m/qwen-x/mmproj.gguf".into(),
                    role: Role::Projector,
                    size: 1,
                },
            ],
            quant: None,
            context: 65_536,
            context_max: None,
            added_at: String::new(),
        };
        let text = preset(&[model], LocalBackend::Cpu);
        assert!(text.contains("n-gpu-layers = 0"), "{text}");
        assert!(
            text.contains(
                "[qwen-x]\nmodel = /m/qwen-x/w.gguf\nc = 65536\nmmproj = /m/qwen-x/mmproj.gguf\n"
            ),
            "{text}"
        );
    }
}
