//! Where a session starts (spec: execution-environments, Working directory
//! resolution).

use std::path::{Component, Path, PathBuf};

/// Where the selected backend runs commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// The local host.
    Local,
    /// An SSH host (starts in `~`).
    Ssh,
    /// A container; `/workspace` when the workspace is mounted.
    Container {
        /// Whether the session workspace is mounted at `/workspace`.
        workspace_mounted: bool,
        /// The backend descriptor's `default_cwd`.
        default_cwd: String,
    },
    /// A plugin (remote) backend with its descriptor's `default_cwd`.
    Remote {
        /// The descriptor's `default_cwd`.
        default_cwd: String,
    },
}

/// The inputs of [`resolve`].
#[derive(Debug, Clone)]
pub struct StartCwd<'a> {
    /// Where commands run.
    pub place: Place,
    /// `terminal.cwd`.
    pub configured: Option<&'a str>,
    /// The directory `prometheus` was launched from.
    pub launch_dir: &'a Path,
    /// The session's workspace for gateway sessions.
    pub gateway_workspace: Option<&'a Path>,
    /// The local user's home (for `~` expansion and host-path detection).
    pub user_home: Option<&'a Path>,
}

/// The start directory and, when a configured path was ignored, the
/// one-time notice naming it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The directory, as the backend understands it.
    pub cwd: String,
    /// `terminal.cwd '<path>' is a host path; using <default> on the <x>
    /// backend`.
    pub notice: Option<String>,
}

/// The session start directory.
#[must_use]
pub fn resolve(input: &StartCwd<'_>) -> Resolved {
    if input.place == Place::Local {
        let cwd = match (input.configured, input.gateway_workspace) {
            (Some(configured), _) => expand_home(configured, input.user_home),
            (None, Some(workspace)) => workspace.to_owned(),
            (None, None) => input.launch_dir.to_owned(),
        };
        return Resolved {
            cwd: cwd.to_string_lossy().into_owned(),
            notice: None,
        };
    }
    let default = backend_default(&input.place);
    match input.configured {
        Some(path) if looks_like_host_path(path, input.user_home) => Resolved {
            notice: Some(format!(
                "terminal.cwd '{path}' is a path on this host; the {} backend starts in {default} instead",
                place_name(&input.place)
            )),
            cwd: default,
        },
        Some(path) => Resolved {
            cwd: path.to_owned(),
            notice: None,
        },
        None => Resolved {
            cwd: default,
            notice: None,
        },
    }
}

fn backend_default(place: &Place) -> String {
    match place {
        Place::Local => String::new(),
        Place::Ssh => "~".to_owned(),
        Place::Container {
            workspace_mounted: true,
            ..
        } => "/workspace".to_owned(),
        Place::Container { default_cwd, .. } | Place::Remote { default_cwd } => default_cwd.clone(),
    }
}

fn place_name(place: &Place) -> &'static str {
    match place {
        Place::Local => "local",
        Place::Ssh => "ssh",
        Place::Container { .. } => "container",
        Place::Remote { .. } => "remote",
    }
}

/// `~` and `~/x` against the local home; other paths unchanged.
#[must_use]
pub fn expand_home(path: &str, user_home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix('~'), user_home) {
        (Some(""), Some(home)) => home.to_owned(),
        (Some(rest), Some(home)) if rest.starts_with(['/', '\\']) => home.join(&rest[1..]),
        _ => PathBuf::from(path),
    }
}

/// Whether `path` names something on this host rather than on a remote
/// machine: a Windows drive path, macOS `/Users/...`, Linux `/home/...`, or
/// a path inside the local user's home.
fn looks_like_host_path(path: &str, user_home: Option<&Path>) -> bool {
    let bytes = path.as_bytes();
    let drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    let user_dirs = ["/Users/", "/home/"].iter().any(|p| path.starts_with(p));
    let in_home = user_home.is_some_and(|home| {
        let home: Vec<Component<'_>> = home.components().collect();
        home.len() > 1
            && Path::new(path)
                .components()
                .take(home.len())
                .eq(home.iter().copied())
    });
    drive || user_dirs || in_home
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{Place, StartCwd, resolve};

    fn input(place: Place, configured: Option<&str>) -> StartCwd<'_> {
        StartCwd {
            place,
            configured,
            launch_dir: Path::new("/launch"),
            gateway_workspace: None,
            user_home: Some(Path::new("/home/me")),
        }
    }

    #[test]
    fn local_uses_config_then_gateway_workspace_then_launch_dir() {
        let launch = resolve(&input(Place::Local, None));
        assert_eq!(launch.cwd, "/launch");
        let configured = resolve(&input(Place::Local, Some("~/proj")));
        assert_eq!(Path::new(&configured.cwd), Path::new("/home/me/proj"));
        let gateway = resolve(&StartCwd {
            gateway_workspace: Some(Path::new("/ws")),
            ..input(Place::Local, None)
        });
        assert_eq!(gateway.cwd, "/ws");
    }

    #[test]
    fn host_path_on_a_remote_backend_uses_the_default_with_a_notice() {
        let remote = Place::Remote {
            default_cwd: "/root".to_owned(),
        };
        let resolved = resolve(&input(remote.clone(), Some("/Users/me/project")));
        assert_eq!(resolved.cwd, "/root");
        assert!(resolved.notice.unwrap().contains("'/Users/me/project'"));
        let remote_path = resolve(&input(remote, Some("/srv/app")));
        assert_eq!(remote_path.cwd, "/srv/app");
        assert!(remote_path.notice.is_none());
    }

    #[test]
    fn backend_defaults() {
        assert_eq!(resolve(&input(Place::Ssh, None)).cwd, "~");
        let mounted = Place::Container {
            workspace_mounted: true,
            default_cwd: "/".to_owned(),
        };
        assert_eq!(resolve(&input(mounted, Some("C:\\work"))).cwd, "/workspace");
        let unmounted = Place::Container {
            workspace_mounted: false,
            default_cwd: "/root".to_owned(),
        };
        assert_eq!(resolve(&input(unmounted, None)).cwd, "/root");
    }
}
