//! The external URL command's policy (spec: desktop-app, External URL
//! scheme allowlist). Only `http:`, `https:` and `mailto:` reach the OS
//! handler; `file:` URLs and bare local paths go to the guarded file
//! opener, which refuses executables and anything outside the user's home
//! and the session workspace; every other scheme is invalid.

use std::path::{Path, PathBuf};

use serde::Serialize;
use url::Url;

/// Where an external link goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The OS URL handler (browser, mail client).
    Url(String),
    /// The guarded file opener.
    File(PathBuf),
    /// Refused.
    Invalid,
}

/// Sorts a link the user clicked; `user_home` expands `~/`.
#[must_use]
pub fn classify(raw: &str, user_home: &Path) -> Target {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix("~/") {
        return Target::File(user_home.join(rest));
    }
    if Path::new(raw).is_absolute() || has_drive_prefix(raw) {
        return Target::File(PathBuf::from(raw));
    }
    let Ok(url) = Url::parse(raw) else {
        return Target::Invalid;
    };
    match url.scheme() {
        "http" | "https" | "mailto" => Target::Url(url.to_string()),
        "file" => url.to_file_path().map_or(Target::Invalid, Target::File),
        _ => Target::Invalid,
    }
}

/// `C:\` or `C:/`: a Windows path, which `Url::parse` would read as a
/// one-letter scheme.
fn has_drive_prefix(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
}

/// The external URL command's answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Opened {
    /// Whether something was opened.
    pub ok: bool,
    /// Why not: `invalid`, `outside`, `executable`, `missing` or `failed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

impl Opened {
    /// Opened.
    pub const OK: Self = Self {
        ok: true,
        reason: None,
    };

    /// Refused for `reason`.
    #[must_use]
    pub const fn refused(reason: &'static str) -> Self {
        Self {
            ok: false,
            reason: Some(reason),
        }
    }
}

/// Extensions that run code when opened, on any OS.
const EXECUTABLE: &[&str] = &[
    "app", "appimage", "bat", "cmd", "com", "command", "cpl", "deb", "desktop", "dmg", "exe",
    "hta", "jar", "js", "jse", "lnk", "msc", "msi", "pkg", "ps1", "rpm", "scr", "scpt", "sh",
    "terminal", "url", "vbe", "vbs", "workflow", "wsf",
];

/// Checks a local path for the guarded opener: it must exist, resolve
/// inside `user_home` or `workspace`, and not be an executable.
///
/// # Errors
///
/// The refusal reason: `missing`, `outside` or `executable`.
pub fn guard_file(
    path: &Path,
    user_home: &Path,
    workspace: Option<&Path>,
) -> Result<PathBuf, &'static str> {
    let real = guard_inside(path, user_home, workspace)?;
    if is_executable(&real) {
        return Err("executable");
    }
    Ok(real)
}

/// The resolved `path` when it exists inside `user_home` or `workspace`
/// (revealing a file in its folder runs nothing, so executables pass).
///
/// # Errors
///
/// `missing` or `outside`.
pub fn guard_inside(
    path: &Path,
    user_home: &Path,
    workspace: Option<&Path>,
) -> Result<PathBuf, &'static str> {
    let real = path.canonicalize().map_err(|_| "missing")?;
    let inside = |root: &Path| root.canonicalize().is_ok_and(|root| real.starts_with(root));
    if inside(user_home) || workspace.is_some_and(inside) {
        Ok(real)
    } else {
        Err("outside")
    }
}

fn is_executable(path: &Path) -> bool {
    let by_extension = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXECUTABLE.contains(&e.to_ascii_lowercase().as_str()));
    by_extension || has_exec_bit(path)
}

#[cfg(unix)]
fn has_exec_bit(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn has_exec_bit(_path: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{Target, classify, guard_file};

    #[test]
    fn a_javascript_link_is_invalid() {
        let home = Path::new("/home/u");
        assert_eq!(classify("javascript:alert(1)", home), Target::Invalid);
        assert_eq!(classify("data:text/html,<b>x</b>", home), Target::Invalid);
        assert_eq!(classify("prometheus://new", home), Target::Invalid);
        assert_eq!(classify("not a link", home), Target::Invalid);
    }

    #[test]
    fn web_and_mail_links_go_to_the_os_and_files_to_the_guard() {
        let home = Path::new("/home/u");
        assert_eq!(
            classify("https://example.com/a", home),
            Target::Url("https://example.com/a".into())
        );
        assert_eq!(
            classify("mailto:a@example.com", home),
            Target::Url("mailto:a@example.com".into())
        );
        assert_eq!(
            classify("~/notes.md", home),
            Target::File(home.join("notes.md"))
        );
        assert_eq!(
            classify(r"C:\Users\u\a.txt", home),
            Target::File(PathBuf::from(r"C:\Users\u\a.txt"))
        );
    }

    #[test]
    fn the_guard_refuses_executables_and_paths_outside() {
        let home = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let note = home.path().join("note.md");
        std::fs::write(&note, "x").unwrap();
        let tool = workspace.path().join("run.sh");
        std::fs::write(&tool, "echo").unwrap();
        let elsewhere = outside.path().join("x.md");
        std::fs::write(&elsewhere, "x").unwrap();
        assert!(guard_file(&note, home.path(), None).is_ok());
        assert_eq!(
            guard_file(&tool, home.path(), Some(workspace.path())),
            Err("executable")
        );
        assert_eq!(guard_file(&elsewhere, home.path(), None), Err("outside"));
        assert_eq!(
            guard_file(&home.path().join("gone"), home.path(), None),
            Err("missing")
        );
    }
}
