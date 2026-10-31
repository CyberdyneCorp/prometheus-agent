//! Path guards of the file tools (spec: execution-environments, `read_file`
//! guards, Write guards, Protected secret files and device-namespace
//! paths).
//!
//! Each guard returns the refusal text the model sees, or `None`. They work
//! on the path text and on resolved paths only; none of them opens a file.

use std::path::{Component, Path, PathBuf};

/// Windows NT and device namespace prefixes (compared case-insensitively).
const DEVICE_PREFIXES: &[&str] = &[r"\??\", r"\\.\", r"\\?\UNC\", r"\\?\GLOBALROOT"];

/// System locations no agent write may touch.
const SYSTEM_DIRS: &[&str] = &[
    "/etc/",
    "/boot/",
    "/usr/lib/systemd/",
    "/private/etc/",
    "/private/var/db/",
];

/// Container runtime sockets: writing one is talking to a root daemon.
const RUNTIME_SOCKETS: &[&str] = &["docker.sock", "podman.sock", "containerd.sock"];

/// Per-profile files that steer Prometheus itself.
const CONTROL_FILES: &[&str] = &["config.toml", "secrets.toml", "grants.toml"];

/// `/proc` entries that expose process memory or environment secrets.
const PROC_SECRETS: &[&str] = &[
    "environ",
    "mem",
    "maps",
    "smaps",
    "smaps_rollup",
    "numa_maps",
    "pagemap",
    "auxv",
    "stack",
    "syscall",
];

/// Dotenv templates that hold no secrets.
const ENV_TEMPLATES: &[&str] = &[".env.example", ".env.sample", ".env.template"];

/// `Refusing device-namespace or UNC path '<path>'` for an NT/device
/// namespace path or a UNC host, checked on the raw text before any
/// filesystem access, so a crafted path cannot trigger outbound
/// authentication on any platform.
#[must_use]
pub fn device_namespace(raw: &str) -> Option<String> {
    let upper = raw.to_ascii_uppercase();
    let device = DEVICE_PREFIXES.iter().any(|p| upper.starts_with(p));
    let verbatim_drive = upper.starts_with(r"\\?\") && !device;
    let unc = !verbatim_drive && (raw.starts_with(r"\\") || (cfg!(windows) && is_slash_unc(raw)));
    (device || unc).then(|| format!("Refusing device-namespace or UNC path '{raw}'"))
}

/// Whether `path` is a device-namespace or UNC path: such a path must
/// never reach `canonicalize`/`metadata`, which on Windows open it (an SMB
/// connection that sends the user's NTLM credentials to the host).
#[must_use]
pub fn is_device_path(path: &Path) -> bool {
    device_namespace(&path.to_string_lossy()).is_some()
}

/// `//host/share` (Windows also reads forward slashes as UNC).
fn is_slash_unc(raw: &str) -> bool {
    raw.strip_prefix("//")
        .is_some_and(|rest| rest.split('/').next().is_some_and(|h| !h.is_empty()))
}

/// The read refusal for project dotenv files, which typically hold
/// secrets. Writing and creating them stays allowed.
#[must_use]
pub fn secret_file(path: &Path, shown: &str) -> Option<String> {
    let name = path.file_name()?.to_string_lossy().to_ascii_lowercase();
    let dotenv = name == ".env" || name == ".envrc" || name.starts_with(".env.");
    (dotenv && !ENV_TEMPLATES.contains(&name.as_str())).then(|| {
        format!(
            "Refusing to read '{shown}': it typically holds secrets; ask the user to share the specific value you need"
        )
    })
}

/// The read refusal for `/proc/<pid>/environ|mem|maps`-style files.
#[must_use]
pub fn proc_secret(path: &Path, shown: &str) -> Option<String> {
    let parts: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    let leaks = match parts.as_slice() {
        [proc, rest @ ..] if proc == "proc" => match rest {
            [kcore] => kcore == "kcore",
            [_, file] | [_, _, _, file] => PROC_SECRETS.contains(&file.as_str()),
            _ => false,
        },
        _ => false,
    };
    leaks.then(|| {
        format!("Cannot read '{shown}': it exposes process memory or environment secrets.")
    })
}

/// Where Prometheus keeps its own state, for the control-file guard.
#[derive(Debug, Clone)]
pub struct ControlPaths {
    /// `PROMETHEUS_HOME`, from the session's `ProfileScope`.
    pub prometheus_home: PathBuf,
}

impl ControlPaths {
    /// The write refusal for system locations, container runtime sockets
    /// and every profile's control files. `target` is the resolved path
    /// (links followed).
    #[must_use]
    pub fn write_refusal(&self, target: &Path) -> Option<String> {
        let text = target.to_string_lossy().replace('\\', "/");
        if SYSTEM_DIRS.iter().any(|dir| text.starts_with(dir)) {
            return Some(format!(
                "refusing to write system path: {}",
                target.display()
            ));
        }
        let name = target.file_name()?.to_string_lossy().to_ascii_lowercase();
        if RUNTIME_SOCKETS.contains(&name.as_str()) {
            return Some(format!(
                "refusing to write container runtime socket: {}",
                target.display()
            ));
        }
        self.control_file(target, &name)
    }

    /// `<home>/profiles/<name>/{config,secrets,grants}.toml`, compared
    /// case-insensitively (macOS and Windows filesystems are).
    fn control_file(&self, target: &Path, name: &str) -> Option<String> {
        if !CONTROL_FILES.contains(&name) {
            return None;
        }
        let home = resolve_existing(&self.prometheus_home);
        let profile_dir = target.parent()?;
        let profiles = profile_dir.parent()?;
        let same = |a: &Path, b: &Path| {
            a.to_string_lossy()
                .eq_ignore_ascii_case(&b.to_string_lossy())
        };
        let is_profiles = profiles
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case("profiles"))
            && profiles.parent().is_some_and(|p| same(p, &home));
        is_profiles.then(|| format!("refusing to write Prometheus control file: {name}"))
    }
}

/// `path` with links resolved as far as it exists: the canonical form of
/// its deepest existing ancestor plus the remaining components. A
/// device-namespace or UNC path is returned as is, never opened.
#[must_use]
pub fn resolve_existing(path: &Path) -> PathBuf {
    if is_device_path(path) {
        return path.to_path_buf();
    }
    let mut rest = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        if let Ok(canonical) = dunce::canonicalize(&current) {
            return rest
                .iter()
                .rev()
                .fold(canonical, |acc, part| acc.join(part));
        }
        match (current.file_name(), current.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name.to_os_string());
                current = parent.to_path_buf();
            }
            _ => return path.to_path_buf(),
        }
    }
}

/// `raw` resolved against `cwd` (`~` expanded with `user_home`), with `.`
/// and `..` folded lexically.
#[must_use]
pub fn absolute(raw: &str, cwd: &Path, user_home: Option<&Path>) -> PathBuf {
    let expanded = match (
        raw.strip_prefix("~/").or(raw.strip_prefix("~\\")),
        user_home,
    ) {
        (Some(rest), Some(home)) => home.join(rest),
        _ if raw == "~" && user_home.is_some() => {
            user_home.map(Path::to_path_buf).unwrap_or_default()
        }
        _ => PathBuf::from(raw),
    };
    let joined = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{ControlPaths, absolute, device_namespace, proc_secret, secret_file};

    #[test]
    fn device_and_unc_paths_are_refused_on_every_platform() {
        for raw in [
            r"\\attacker.example\share\x",
            r"\??\C:\Windows",
            r"\\.\PhysicalDrive0",
            r"\\?\UNC\host\share",
            r"\\?\GLOBALROOT\Device\X",
            r"\\?\unc\host\share",
        ] {
            assert_eq!(
                device_namespace(raw),
                Some(format!("Refusing device-namespace or UNC path '{raw}'")),
                "{raw}"
            );
        }
        for raw in [r"\\?\C:\work\a.rs", "src/a.rs", "/etc/hosts", r"C:\x"] {
            assert_eq!(device_namespace(raw), None, "{raw}");
        }
    }

    #[test]
    fn dotenv_files_are_secret_but_templates_are_not() {
        for name in [".env", ".env.local", ".ENV.production", ".envrc"] {
            assert!(secret_file(Path::new(name), name).is_some(), "{name}");
        }
        for name in [
            ".env.example",
            ".env.sample",
            ".env.template",
            "env.rs",
            ".environment.md",
        ] {
            assert!(secret_file(Path::new(name), name).is_none(), "{name}");
        }
    }

    #[test]
    fn proc_memory_and_environment_are_refused() {
        for path in [
            "/proc/self/environ",
            "/proc/1234/mem",
            "/proc/1/maps",
            "/proc/self/task/7/environ",
            "/proc/kcore",
        ] {
            assert!(proc_secret(Path::new(path), path).is_some(), "{path}");
        }
        for path in [
            "/proc/cpuinfo",
            "/proc/self/status",
            "/home/u/proc/1/environ",
        ] {
            assert!(proc_secret(Path::new(path), path).is_none(), "{path}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn system_paths_sockets_and_control_files_are_refused() {
        let guard = ControlPaths {
            prometheus_home: "/nonexistent-home/.prometheus".into(),
        };
        let refused = |p: &str| guard.write_refusal(Path::new(p));
        assert_eq!(
            refused("/nonexistent-home/.prometheus/profiles/work/grants.toml").as_deref(),
            Some("refusing to write Prometheus control file: grants.toml")
        );
        assert!(refused("/nonexistent-home/.prometheus/profiles/default/Config.TOML").is_some());
        assert!(refused("/etc/hosts").is_some());
        assert!(refused("/private/etc/hosts").is_some());
        assert!(refused("/var/run/docker.sock").is_some());
        assert!(refused("/nonexistent-home/work/config.toml").is_none());
        assert!(refused("/nonexistent-home/.prometheus/profiles/work/notes/grants.toml").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn relative_paths_resolve_against_the_cwd() {
        let cwd = Path::new("/w/proj");
        assert_eq!(
            absolute("src/../a.rs", cwd, None),
            Path::new("/w/proj/a.rs")
        );
        assert_eq!(
            absolute("~/.ssh/config", cwd, Some(Path::new("/home/u"))),
            Path::new("/home/u/.ssh/config")
        );
        assert_eq!(absolute("/etc/./hosts", cwd, None), Path::new("/etc/hosts"));
    }
}
