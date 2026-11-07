//! Validation of `terminal.container.volumes` and `extra_args` (spec:
//! execution-environments, Container backend options).
//!
//! A volume may not expose `PROMETHEUS_HOME`, a credential store the OS
//! sandbox denies, or the runtime socket. An extra argument may only add
//! restrictions: anything that weakens hardening is refused unless
//! `security.sandbox.allow_degraded` covers it.

use std::path::{Path, PathBuf};

use pmt_config::schema::DegradedFeature;
use pmt_core::error::{Error, codes};

use crate::policy::Paths;

/// Socket file names whose mount hands the container the runtime itself.
const RUNTIME_SOCKETS: &[&str] = &["docker.sock", "podman.sock", "containerd.sock"];

/// Namespace flags whose `host` value joins a host namespace.
const NAMESPACE_FLAGS: &[&str] = &[
    "--pid",
    "--network",
    "--net",
    "--userns",
    "--ipc",
    "--uts",
    "--cgroupns",
];

/// One parsed `-v` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Volume {
    /// The host path.
    pub host: PathBuf,
    /// The path in the container.
    pub container: String,
    /// Mounted read-only.
    pub read_only: bool,
}

impl Volume {
    /// The `--volume` argument.
    #[must_use]
    pub fn arg(&self) -> String {
        let ro = if self.read_only { ":ro" } else { "" };
        format!("--volume={}:{}{ro}", self.host.display(), self.container)
    }
}

/// Parses and checks `terminal.container.volumes`.
///
/// # Errors
///
/// `terminal.container_hardening` for a malformed entry, or a host path
/// that is, contains or lies inside `PROMETHEUS_HOME`, a credential store or
/// a runtime socket.
pub fn volumes(
    entries: &[String],
    user_home: &Path,
    prometheus_home: &Path,
) -> Result<Vec<Volume>, Error> {
    let paths = Paths {
        user_home: user_home.to_path_buf(),
        prometheus_home: prometheus_home.to_path_buf(),
        workspace: PathBuf::new(),
        scratch: Vec::new(),
    };
    let mut protected = crate::locations::credential_stores(&paths);
    protected.push(prometheus_home.to_path_buf());
    let protected: Vec<PathBuf> = protected.iter().map(|p| resolved(p)).collect();
    entries
        .iter()
        .map(|entry| {
            let volume = parse_volume(entry)?;
            let host = resolved(&volume.host);
            let socket = host
                .file_name()
                .is_some_and(|n| RUNTIME_SOCKETS.iter().any(|s| n == *s));
            let exposes = |p: &PathBuf| host.starts_with(p) || p.starts_with(&host);
            if socket || protected.iter().any(exposes) {
                return Err(Error::new(
                    &codes::TERMINAL_CONTAINER_HARDENING,
                    format!(
                        "terminal.container.volumes: '{entry}' would expose a protected path ({})",
                        volume.host.display()
                    ),
                ));
            }
            Ok(volume)
        })
        .collect()
}

fn parse_volume(entry: &str) -> Result<Volume, Error> {
    let invalid = || {
        Error::new(
            &codes::TERMINAL_CONTAINER_HARDENING,
            format!("terminal.container.volumes: '{entry}' is not host:container[:ro]"),
        )
    };
    // Parsed from the right: a Windows host path starts with a drive
    // letter and its colon (`C:\src:/src:ro`).
    let (rest, read_only) = match entry.rsplit_once(':') {
        Some((rest, "ro")) => (rest, true),
        Some((rest, "rw")) => (rest, false),
        _ => (entry, false),
    };
    let Some((host, container)) = rest.rsplit_once(':') else {
        return Err(invalid());
    };
    let drive = cfg!(windows) && host.as_bytes().get(1) == Some(&b':');
    let colon_in_host = host[if drive { 2 } else { 0 }..].contains(':');
    if colon_in_host || !Path::new(host).is_absolute() || !container.starts_with('/') {
        return Err(invalid());
    }
    Ok(Volume {
        host: PathBuf::from(host),
        container: container.to_owned(),
        read_only,
    })
}

/// The path with symbolic links resolved when it exists.
fn resolved(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Checks `terminal.container.extra_args`.
///
/// # Errors
///
/// `terminal.container_hardening` naming the first argument that weakens
/// hardening and that `allowed` does not cover.
pub fn extra_args(args: &[String], allowed: &[DegradedFeature]) -> Result<(), Error> {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if arg.starts_with("--") => (flag, Some(value)),
            _ => (arg, None),
        };
        let takes_value = matches!(
            flag,
            "--security-opt" | "-v" | "--volume" | "--mount" | "--read-only"
        ) || NAMESPACE_FLAGS.contains(&flag);
        let value = match inline {
            Some(value) => Some(value),
            None if takes_value && flag != "--read-only" => {
                i += 1;
                args.get(i).map(String::as_str)
            }
            None => None,
        };
        if let Some(feature) = weakens(flag, value)
            && !allowed.contains(&feature)
            && !allowed.contains(&DegradedFeature::All)
        {
            return Err(Error::new(
                &codes::TERMINAL_CONTAINER_HARDENING,
                format!("terminal.container.extra_args: '{arg}' would weaken container hardening"),
            )
            .with_hint(format!(
                "remove it, or set security.sandbox.allow_degraded = [\"{}\"]",
                feature.as_str()
            )));
        }
        i += 1;
    }
    Ok(())
}

/// The feature `flag value` weakens, if any.
fn weakens(flag: &str, value: Option<&str>) -> Option<DegradedFeature> {
    let value = value.unwrap_or("");
    match flag {
        "--privileged" if value != "false" => Some(DegradedFeature::All),
        "--cap-add" => Some(DegradedFeature::CapDrop),
        "--read-only" if value == "false" => Some(DegradedFeature::ReadOnlyRootfs),
        "--security-opt" => security_opt(value),
        "-v" | "--volume" | "--mount" if RUNTIME_SOCKETS.iter().any(|s| value.contains(s)) => {
            Some(DegradedFeature::All)
        }
        f if NAMESPACE_FLAGS.contains(&f) && value == "host" => Some(DegradedFeature::All),
        _ => None,
    }
}

/// `--security-opt` values that only add restrictions pass; the rest weaken.
fn security_opt(value: &str) -> Option<DegradedFeature> {
    let normalized = value.replace(':', "=");
    let (key, setting) = normalized.split_once('=').unwrap_or((&normalized, ""));
    match key {
        "no-new-privileges" if matches!(setting, "" | "true") => None,
        "no-new-privileges" => Some(DegradedFeature::NoNewPrivileges),
        "seccomp" if setting == "unconfined" => Some(DegradedFeature::Seccomp),
        "seccomp" | "mask" => None,
        "apparmor" if setting != "unconfined" => None,
        "label" if setting != "disable" => None,
        _ => Some(DegradedFeature::All),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privileged_and_host_namespaces_are_refused_naming_the_argument() {
        for args in [
            vec!["--privileged".to_owned()],
            vec!["--pid=host".to_owned()],
            vec!["--network".to_owned(), "host".to_owned()],
            vec!["--userns=host".to_owned()],
            vec![
                "-v".to_owned(),
                "/var/run/docker.sock:/var/run/docker.sock".to_owned(),
            ],
            vec!["--security-opt".to_owned(), "seccomp=unconfined".to_owned()],
            vec!["--security-opt=no-new-privileges=false".to_owned()],
            vec!["--cap-add=SYS_ADMIN".to_owned()],
        ] {
            let err = extra_args(&args, &[]).unwrap_err();
            assert!(err.message().contains(&args[0]), "{}", err.message());
        }
    }

    #[test]
    fn restricting_arguments_pass_and_allow_degraded_waives_the_rest() {
        let ok = [
            "--security-opt=no-new-privileges".to_owned(),
            "--security-opt".to_owned(),
            "apparmor=docker-default".to_owned(),
            "--network=none".to_owned(),
            "--env=A=b".to_owned(),
        ];
        extra_args(&ok, &[]).unwrap();
        extra_args(
            &["--cap-add=NET_RAW".to_owned()],
            &[DegradedFeature::CapDrop],
        )
        .unwrap();
        extra_args(&["--privileged".to_owned()], &[DegradedFeature::All]).unwrap();
        assert!(extra_args(&["--privileged".to_owned()], &[DegradedFeature::CapDrop]).is_err());
    }

    #[test]
    fn volumes_may_not_expose_the_home_or_credential_stores() {
        let root = std::env::temp_dir();
        let home = root.join("pmt-user-home");
        let prometheus = home.join(".prometheus");
        let refused = [
            format!("{}:/data", prometheus.display()),
            format!("{}:/data:ro", prometheus.join("profiles").display()),
            format!("{}:/h", home.display()),
            format!("{}:/k", home.join(".ssh").display()),
            "/var/run/docker.sock:/s".to_owned(),
            "relative:/x".to_owned(),
            format!("{}:/a:/b", home.join("code").display()),
            format!("{}:/a:zz", home.join("code").display()),
            format!("{}", home.join("code").display()),
        ];
        for entry in refused {
            assert!(
                volumes(std::slice::from_ref(&entry), &home, &prometheus).is_err(),
                "{entry}"
            );
        }
        let ok = volumes(
            &[format!("{}:/src:ro", home.join("code").display())],
            &home,
            &prometheus,
        )
        .unwrap();
        assert!(ok[0].read_only);
        assert_eq!(ok[0].container, "/src");
    }
}
