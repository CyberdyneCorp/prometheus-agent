//! Files no tool may hand out by path alone (spec: execution-environments,
//! Protected secret files and device-namespace paths): project dotenv files
//! and `/proc` entries that expose process memory or environment. One
//! definition for every surface that returns or sends file content: the
//! file tools (`pmt_tools::files::guards`) and the gateway's outbound
//! attachments.

use std::path::{Component, Path};

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

/// Whether `path` names a project dotenv file (`.env`, `.env.*`,
/// `.envrc`; templates excepted), which typically holds secrets.
#[must_use]
pub fn is_dotenv(path: &Path) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    let name = name.to_string_lossy().to_ascii_lowercase();
    let dotenv = name == ".env" || name == ".envrc" || name.starts_with(".env.");
    dotenv && !ENV_TEMPLATES.contains(&name.as_str())
}

/// Whether `path` is a `/proc/<pid>/environ|mem|maps`-style file (or
/// `/proc/kcore`).
#[must_use]
pub fn is_proc_secret(path: &Path) -> bool {
    let parts: Vec<String> = path
        .components()
        .filter_map(|c| match c {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    match parts.as_slice() {
        [proc, rest @ ..] if proc == "proc" => match rest {
            [kcore] => kcore == "kcore",
            [_, file] | [_, _, _, file] => PROC_SECRETS.contains(&file.as_str()),
            _ => false,
        },
        _ => false,
    }
}

/// Whether `path` may never be handed out by path ([`is_dotenv`] or
/// [`is_proc_secret`]). Callers check the path as written AND where it
/// leads, so a link `notes.txt -> .env` is caught too.
#[must_use]
pub fn is_protected(path: &Path) -> bool {
    is_dotenv(path) || is_proc_secret(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotenv_files_but_not_templates() {
        for name in [".env", ".env.local", ".ENVRC", "/w/app/.env.production"] {
            assert!(is_protected(Path::new(name)), "{name}");
        }
        for name in [".env.example", "env", "notes.env", "/w/a.txt"] {
            assert!(!is_protected(Path::new(name)), "{name}");
        }
    }

    #[test]
    fn proc_memory_and_environment() {
        assert!(is_protected(Path::new("/proc/1/environ")));
        assert!(is_protected(Path::new("/proc/self/task/2/mem")));
        assert!(is_protected(Path::new("/proc/kcore")));
        assert!(!is_protected(Path::new("/proc/1/status")));
    }
}
