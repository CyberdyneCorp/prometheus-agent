//! The session each working directory last showed in the TUI (spec: tui,
//! Auto-resume of the last TUI session): `<profile>/tui/recent.json` maps a
//! directory to a session id, written whenever the TUI shows a session, so
//! `ui.tui.auto_resume = "recent"` can re-attach to it after a dropped
//! connection or a closed terminal.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The file in a profile directory.
#[must_use]
pub(super) fn path_in(profile_dir: &Path) -> PathBuf {
    profile_dir.join("tui").join("recent.json")
}

fn load(file: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// The session `cwd` last showed.
#[must_use]
pub(super) fn lookup(file: &Path, cwd: &Path) -> Option<String> {
    load(file).remove(&cwd.to_string_lossy().into_owned())
}

/// Records that `cwd` shows `session` (best effort: a failed write only
/// costs the next auto-resume).
pub(super) fn record(file: &Path, cwd: &Path, session: &str) {
    let mut map = load(file);
    let key = cwd.to_string_lossy().into_owned();
    if map.get(&key).map(String::as_str) == Some(session) {
        return;
    }
    map.insert(key, session.to_owned());
    let Ok(text) = serde_json::to_string_pretty(&map) else {
        return;
    };
    let written = file
        .parent()
        .map_or(Ok(()), pmt_core::private_fs::create_dir)
        .and_then(|()| pmt_core::private_fs::write_atomic(file, text.as_bytes()));
    if let Err(err) = written {
        tracing::warn!("cannot record the TUI's recent session: {err}");
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{lookup, record};

    #[test]
    fn each_directory_remembers_its_last_session() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tui/recent.json");
        assert_eq!(lookup(&file, Path::new("/src/api")), None);
        record(&file, Path::new("/src/api"), "ses_A");
        record(&file, Path::new("/src/web"), "ses_B");
        record(&file, Path::new("/src/api"), "ses_C");
        assert_eq!(
            lookup(&file, Path::new("/src/api")).as_deref(),
            Some("ses_C")
        );
        assert_eq!(
            lookup(&file, Path::new("/src/web")).as_deref(),
            Some("ses_B")
        );
    }
}
