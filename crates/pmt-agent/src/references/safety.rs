//! What a context reference may attach (spec: agent-turn-loop, Context
//! reference safety and completion): paths inside the workspace only,
//! never credential files, never binaries.

use std::path::{Component, Path, PathBuf};

/// Refusal for a path that leaves the workspace.
pub const OUTSIDE: &str = "path is outside the allowed workspace";
/// Refusal for a credential file.
pub const SENSITIVE: &str = "path is a sensitive credential file";
/// Refusal for a binary file.
pub const BINARY: &str = "file is binary";

/// Directories under the user's home that hold credentials.
const SENSITIVE_DIRS: &[&str] = &[".ssh", ".aws", ".gnupg", ".kube"];

/// Files directly under the user's home that hold credentials or run code
/// at login (shell rc and profile files).
const SENSITIVE_FILES: &[&str] = &[
    ".netrc",
    ".pgpass",
    ".npmrc",
    ".pypirc",
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".bash_logout",
    ".profile",
    ".zshrc",
    ".zshenv",
    ".zprofile",
    ".zlogin",
    ".kshrc",
    ".cshrc",
    ".tcshrc",
];

/// Files under the Prometheus home that must never be attached.
const PROMETHEUS_FILES: &[&str] = &["secrets.toml", "grants.toml"];

/// Extensions read as text without sniffing.
const TEXT_EXTENSIONS: &[&str] = &[
    "rs",
    "py",
    "pyi",
    "js",
    "mjs",
    "cjs",
    "ts",
    "tsx",
    "jsx",
    "go",
    "c",
    "h",
    "cc",
    "cpp",
    "hpp",
    "java",
    "kt",
    "swift",
    "rb",
    "php",
    "cs",
    "sh",
    "bash",
    "zsh",
    "fish",
    "ps1",
    "md",
    "txt",
    "rst",
    "toml",
    "yaml",
    "yml",
    "json",
    "jsonl",
    "xml",
    "html",
    "css",
    "scss",
    "sql",
    "csv",
    "ini",
    "cfg",
    "conf",
    "lock",
    "sv",
    "svh",
    "v",
    "sol",
    "lua",
    "zig",
    "nix",
    "proto",
    "graphql",
    "dockerfile",
    "mk",
    "cmake",
    "gradle",
    "env",
    "gitignore",
    "log",
    "diff",
    "patch",
];

/// Extensions whose MIME type is binary.
const BINARY_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "tif", "tiff", "pdf", "zip", "gz", "tgz",
    "bz2", "xz", "zst", "7z", "rar", "tar", "jar", "class", "exe", "dll", "so", "dylib", "o", "a",
    "wasm", "bin", "dat", "db", "sqlite", "mp3", "mp4", "mov", "avi", "mkv", "wav", "flac", "ogg",
    "woff", "woff2", "ttf", "otf", "pyc", "docx", "xlsx", "pptx",
];

/// The places a reference is judged against.
#[derive(Debug, Clone)]
pub struct Places {
    /// The session's working directory (relative paths resolve here).
    pub cwd: PathBuf,
    /// The workspace root (references must stay below it).
    pub workspace: PathBuf,
    /// The user's home (`~`).
    pub user_home: Option<PathBuf>,
    /// The Prometheus home.
    pub prometheus_home: Option<PathBuf>,
}

impl Places {
    /// `raw` resolved against the cwd (`~` expanded), with `.` and `..`
    /// removed lexically and symlinks resolved when the path exists.
    #[must_use]
    pub fn resolve(&self, raw: &str) -> PathBuf {
        let expanded = match (
            raw.strip_prefix("~/").or(raw.strip_prefix("~\\")),
            &self.user_home,
        ) {
            (Some(rest), Some(home)) => home.join(rest),
            _ if raw == "~" => self.user_home.clone().unwrap_or_else(|| PathBuf::from(raw)),
            _ => PathBuf::from(raw),
        };
        let joined = if expanded.is_absolute() {
            expanded
        } else {
            self.cwd.join(expanded)
        };
        let lexical = normalize(&joined);
        std::fs::canonicalize(&lexical).unwrap_or(lexical)
    }

    /// The refusal for attaching `path`, if any.
    #[must_use]
    pub fn refusal(&self, path: &Path) -> Option<&'static str> {
        if self.is_sensitive(path) {
            return Some(SENSITIVE);
        }
        let workspace = std::fs::canonicalize(&self.workspace).unwrap_or(self.workspace.clone());
        (!path.starts_with(&workspace)).then_some(OUTSIDE)
    }

    fn is_sensitive(&self, path: &Path) -> bool {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if let Some(home) = self.user_home.as_ref().map(|h| canonical(h))
            && let Ok(rest) = path.strip_prefix(&home)
        {
            let first = rest
                .components()
                .next()
                .map(|c| c.as_os_str().to_string_lossy());
            let top = first.as_deref().unwrap_or("");
            if SENSITIVE_DIRS.contains(&top)
                || (rest.components().count() == 1 && SENSITIVE_FILES.contains(&top))
            {
                return true;
            }
        }
        let Some(prometheus) = self.prometheus_home.as_ref().map(|h| canonical(h)) else {
            return false;
        };
        let Ok(rest) = path.strip_prefix(&prometheus) else {
            return false;
        };
        PROMETHEUS_FILES.contains(&name) || is_skill_hub_cache(rest)
    }
}

/// `<home>/cache/skills/...` or `<home>/profiles/<p>/cache/skills/...`:
/// downloaded hub skills not yet reviewed.
fn is_skill_hub_cache(rest: &Path) -> bool {
    let parts: Vec<String> = rest
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    parts
        .windows(2)
        .any(|w| w[0] == "cache" && w[1] == "skills")
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// `path` without `.` and with `..` applied (never above the root).
#[must_use]
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether `path` with content `head` (its first bytes) is binary: known
/// text extensions are text, known binary extensions are binary, anything
/// else is binary when it holds a NUL byte.
#[must_use]
pub fn is_binary(path: &Path, head: &[u8]) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if TEXT_EXTENSIONS.contains(&ext.as_str()) {
        return false;
    }
    if BINARY_EXTENSIONS.contains(&ext.as_str()) {
        return true;
    }
    head.contains(&0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn places(root: &Path) -> Places {
        Places {
            cwd: root.join("work/app"),
            workspace: root.join("work/app"),
            user_home: Some(root.join("home")),
            prometheus_home: Some(root.join("home/.prometheus")),
        }
    }

    #[test]
    fn credential_files_and_traversal_are_refused() {
        let root = Path::new("/r");
        let p = places(root);
        let aws = p.resolve("~/.aws/credentials");
        assert_eq!(p.refusal(&aws), Some(SENSITIVE));
        assert_eq!(p.refusal(&p.resolve("~/.zshrc")), Some(SENSITIVE));
        assert_eq!(
            p.refusal(&p.resolve("~/.prometheus/profiles/default/secrets.toml")),
            Some(SENSITIVE)
        );
        assert_eq!(
            p.refusal(&p.resolve("~/.prometheus/cache/skills/x/SKILL.md")),
            Some(SENSITIVE)
        );
        assert_eq!(p.refusal(&p.resolve("../../etc/passwd")), Some(OUTSIDE));
        assert_eq!(p.refusal(&p.resolve("src/main.rs")), None);
        assert_eq!(p.refusal(&p.resolve("src/../../app/src/lib.rs")), None);
    }

    #[test]
    fn binaries_are_recognised() {
        assert!(is_binary(Path::new("a.png"), b"plain"));
        assert!(!is_binary(Path::new("a.rs"), b"\0weird"));
        assert!(is_binary(Path::new("blob"), b"ab\0c"));
        assert!(!is_binary(Path::new("README"), b"hello"));
    }
}
