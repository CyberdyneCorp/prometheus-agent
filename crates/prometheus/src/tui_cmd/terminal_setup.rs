//! `/terminal-setup` (spec: tui, Clipboard image attachments): the
//! integrated terminals of VS Code, Cursor and Windsurf keep `Ctrl+V` for
//! their own paste, so the TUI never sees it. This adds a keybinding to
//! each installed editor's `keybindings.json` that sends `Ctrl+V` to the
//! terminal instead, after showing the change and asking; the edited file
//! is first copied to `keybindings.json.bak`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pmt_core::env::LaunchEnv;

/// The binding added (VS Code's `sendSequence` with the `Ctrl+V` byte).
pub(super) const BINDING: &str = r#"{
    "key": "ctrl+v",
    "command": "workbench.action.terminal.sendSequence",
    "args": { "text": "\u0016" },
    "when": "terminalFocus"
}"#;

/// The editors whose terminals need it, by their settings folder name.
const EDITORS: &[(&str, &str)] = &[
    ("VS Code", "Code"),
    ("Cursor", "Cursor"),
    ("Windsurf", "Windsurf"),
];

/// One file to change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Target {
    /// The editor.
    pub(super) editor: &'static str,
    /// Its `keybindings.json`.
    pub(super) file: PathBuf,
}

/// The editors installed for this user (their user settings folder
/// exists), with the file that gets the binding.
pub(super) fn targets(launch: &LaunchEnv) -> Vec<Target> {
    let Some(base) = settings_base(launch) else {
        return Vec::new();
    };
    EDITORS
        .iter()
        .map(|(editor, folder)| Target {
            editor,
            file: base.join(folder).join("User").join("keybindings.json"),
        })
        .filter(|t| t.file.parent().is_some_and(Path::is_dir))
        .collect()
}

/// Where editors keep per-user settings on this OS.
fn settings_base(launch: &LaunchEnv) -> Option<PathBuf> {
    if cfg!(windows) {
        return launch.inherited("APPDATA").map(PathBuf::from);
    }
    let home = launch.user_home()?;
    Some(if cfg!(target_os = "macos") {
        home.join("Library/Application Support")
    } else {
        launch
            .inherited("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map_or_else(|| home.join(".config"), PathBuf::from)
    })
}

/// What the change does, for the confirmation.
pub(super) fn describe(targets: &[Target]) -> String {
    let mut text =
        String::from("/terminal-setup adds this keybinding so Ctrl+V reaches the TUI:\n");
    text.push_str(BINDING);
    for target in targets {
        let _ = write!(text, "\nto {} ({})", target.file.display(), target.editor);
    }
    text
}

/// Adds the binding to `target` (unless it is there), keeping a backup.
///
/// # Errors
///
/// The file cannot be read or written, or is not a JSON array.
pub(super) fn apply(target: &Target) -> Result<String, String> {
    let file = &target.file;
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("cannot read {}: {e}", file.display())),
    };
    if text.contains("workbench.action.terminal.sendSequence") && text.contains("\\u0016") {
        return Ok(format!("{}: already set up", target.editor));
    }
    let updated = with_binding(&text)
        .ok_or_else(|| format!("{} is not a JSON array of keybindings", file.display()))?;
    if !text.is_empty() {
        let backup = file.with_extension("json.bak");
        std::fs::copy(file, &backup)
            .map_err(|e| format!("cannot back up {}: {e}", file.display()))?;
    }
    std::fs::write(file, updated).map_err(|e| format!("cannot write {}: {e}", file.display()))?;
    Ok(format!(
        "{}: added Ctrl+V to {}",
        target.editor,
        file.display()
    ))
}

/// `text` (a JSON array that may carry comments, as VS Code allows) with
/// the binding appended before its closing bracket.
fn with_binding(text: &str) -> Option<String> {
    let indented = BINDING.replace('\n', "\n    ");
    if text.trim().is_empty() {
        return Some(format!("[\n    {indented}\n]\n"));
    }
    let close = text.rfind(']')?;
    let open = text.find('[')?;
    let body = text[open + 1..close].trim_end();
    let empty = strip_comments(body).trim().is_empty();
    let comma = if empty || body.ends_with(',') {
        ""
    } else {
        ","
    };
    Some(format!(
        "{}{body}{comma}\n    {indented}\n{}",
        &text[..=open],
        &text[close..]
    ))
}

/// `text` without `//` line comments (enough to tell an empty array).
fn strip_comments(text: &str) -> String {
    text.lines()
        .map(|line| line.split("//").next().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::{Target, apply, with_binding};

    #[test]
    fn the_binding_joins_existing_ones_and_comments_survive() {
        let empty = with_binding("// Place your key bindings in this file\n[\n]\n").unwrap();
        assert!(empty.starts_with("// Place your key bindings"), "{empty}");
        assert!(!empty.contains("[,"), "{empty}");
        let parsed: serde_json::Value =
            serde_json::from_str(&empty.lines().skip(1).collect::<Vec<_>>().join("\n")).unwrap();
        assert_eq!(parsed[0]["args"]["text"], "\u{16}");
        let one = with_binding("[\n  {\"key\": \"ctrl+k\", \"command\": \"x\"}\n]").unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&one).unwrap();
        assert_eq!(parsed.as_array().unwrap().len(), 2);
        assert_eq!(with_binding("{}"), None);
    }

    #[test]
    fn applying_keeps_a_backup_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("keybindings.json");
        std::fs::write(&file, "[]").unwrap();
        let target = Target {
            editor: "VS Code",
            file: file.clone(),
        };
        assert!(apply(&target).unwrap().contains("added Ctrl+V"));
        assert_eq!(
            std::fs::read_to_string(file.with_extension("json.bak")).unwrap(),
            "[]"
        );
        assert!(apply(&target).unwrap().contains("already set up"));
    }
}
