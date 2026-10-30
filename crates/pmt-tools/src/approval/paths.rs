//! Lexical path handling for command targets. Nothing touches the disk:
//! the command may run in a container or over SSH, so targets are compared
//! as text after expanding the home, joining the working directory and
//! folding `.` and `..`. Paths use `/` separators throughout.

use std::path::Path;

use super::Floors;

/// `path` as text with `/` separators.
#[must_use]
pub fn text(path: &Path) -> String {
    normalize(&path.to_string_lossy())
}

/// The absolute, normalized form of the command argument `arg`: `~`,
/// `$HOME`, `${HOME}`, `$env:USERPROFILE` and `%USERPROFILE%` become the
/// user home, a relative path is joined to the working directory, and a
/// trailing `/*` or `/.` names the directory itself (deleting everything
/// in a directory is deleting the directory for these rules).
#[must_use]
pub fn resolve(arg: &str, floors: &Floors) -> String {
    let arg = arg.replace('\\', "/");
    let home = floors.user_home.as_deref().unwrap_or("~");
    let mut expanded = arg.clone();
    for prefix in ["~", "$HOME", "${HOME}", "$env:USERPROFILE", "%USERPROFILE%"] {
        if let Some(rest) = arg.strip_prefix(prefix)
            && (rest.is_empty() || rest.starts_with('/'))
        {
            expanded = format!("{home}{rest}");
            break;
        }
    }
    let absolute = expanded.starts_with('/') || has_drive(&expanded);
    let joined = if absolute {
        expanded
    } else {
        format!("{}/{expanded}", floors.cwd)
    };
    let mut path = normalize(&joined);
    while let Some(dir) = path.strip_suffix("/*") {
        path = if dir.is_empty() {
            "/".to_owned()
        } else {
            dir.to_owned()
        };
    }
    path
}

fn has_drive(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Folds `.`, `..` and repeated or trailing separators.
fn normalize(path: &str) -> String {
    let path = path.replace('\\', "/");
    let (prefix, rest) = if has_drive(&path) {
        (path[..2].to_ascii_lowercase(), &path[2..])
    } else {
        (String::new(), path.as_str())
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in rest.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    format!("{prefix}/{}", parts.join("/"))
}

/// Whether `ancestor` is `path` or one of its parent directories.
#[must_use]
pub fn is_ancestor_or_same(ancestor: &str, path: &str) -> bool {
    ancestor == "/"
        || path == ancestor
        || path
            .strip_prefix(ancestor)
            .is_some_and(|rest| rest.starts_with('/'))
}

#[cfg(test)]
mod tests {
    use super::{is_ancestor_or_same, resolve};
    use crate::approval::Floors;

    fn floors() -> Floors {
        Floors {
            cwd: "/work/repo".into(),
            user_home: Some("/home/u".into()),
            ..Floors::default()
        }
    }

    #[test]
    fn targets_resolve_lexically() {
        let f = floors();
        assert_eq!(resolve("/", &f), "/");
        assert_eq!(resolve("/*", &f), "/");
        assert_eq!(resolve("~", &f), "/home/u");
        assert_eq!(resolve("$HOME/", &f), "/home/u");
        assert_eq!(resolve("~/*", &f), "/home/u");
        assert_eq!(resolve("../..", &f), "/");
        assert_eq!(resolve("build/*", &f), "/work/repo/build");
        assert_eq!(resolve("/etc/.", &f), "/etc");
        assert_eq!(resolve(r"C:\Windows", &f), "c:/Windows");
        assert_eq!(resolve("~user", &f), "/work/repo/~user");
    }

    #[test]
    fn ancestry_is_by_whole_components() {
        assert!(is_ancestor_or_same("/home/u", "/home/u/.prometheus"));
        assert!(!is_ancestor_or_same("/home/u/.prom", "/home/u/.prometheus"));
        assert!(is_ancestor_or_same("/", "/x"));
    }
}
