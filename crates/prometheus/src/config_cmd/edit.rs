//! `prometheus config edit` (spec: configuration-and-profiles, Config CLI):
//! the user edits a private draft copy; a valid draft replaces
//! `config.toml` through the writer, an invalid one prints every error and
//! offers to re-open the editor, and aborting leaves the file untouched.

use std::io::{self, BufRead as _, Write as _};
use std::path::Path;
use std::process::Command;

use pmt_config::ProfileDir;
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_core::private_fs;

use crate::output::print_error;

/// The editor when neither `VISUAL` nor `EDITOR` is set.
const DEFAULT_EDITOR: &str = if cfg!(windows) { "notepad" } else { "vi" };

/// Runs the edit loop for `profile`.
pub(super) fn run(profile: &ProfileDir, launch: &LaunchEnv) -> Result<u8, Error> {
    let file = profile.config_file();
    // A broken file is still editable (this is how it gets fixed); a valid
    // older one is migrated first so the user edits the current format.
    let original = match file.read() {
        Ok((text, _)) => text,
        Err(err) if err.code().code == codes::CONFIG_PARSE.code => file.raw()?.unwrap_or_default(),
        Err(err) => return Err(err),
    };
    let draft = profile.path().join("config.edit.toml");
    private_fs::write_atomic(&draft, original.as_bytes()).map_err(|e| draft_error(&draft, &e))?;
    let result = edit_until_valid(
        &file,
        &draft,
        &original,
        launch.editor().unwrap_or(DEFAULT_EDITOR),
    );
    let _ = std::fs::remove_file(&draft);
    result
}

fn edit_until_valid(
    file: &pmt_config::ConfigFile,
    draft: &Path,
    original: &str,
    editor: &str,
) -> Result<u8, Error> {
    loop {
        launch_editor(editor, draft)?;
        let text = std::fs::read_to_string(draft).map_err(|e| draft_error(draft, &e))?;
        if text == original {
            eprintln!("no changes");
            return Ok(0);
        }
        match file.replace(&text) {
            Ok(()) => {
                eprintln!("saved {}", file.path().display());
                return Ok(0);
            }
            Err(err) => {
                print_error(&err, false);
                if !ask_reopen()? {
                    eprintln!("aborted; {} is unchanged", file.path().display());
                    return Err(err);
                }
            }
        }
    }
}

/// Runs the editor command (which may carry arguments) on `path`.
fn launch_editor(editor: &str, path: &Path) -> Result<(), Error> {
    let mut words = editor.split_whitespace();
    let program = words.next().unwrap_or(DEFAULT_EDITOR);
    let status = Command::new(program)
        .args(words)
        .arg(path)
        .status()
        .map_err(|e| {
            Error::new(
                &codes::CONFIG_EDITOR,
                format!("cannot start editor '{program}': {e}"),
            )
            .with_hint("set VISUAL or EDITOR to your editor command")
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::new(
            &codes::CONFIG_EDITOR,
            format!("editor '{program}' exited with {status}; config.toml is unchanged"),
        ))
    }
}

/// Asks whether to re-open the editor; yes by default, no at end of input.
fn ask_reopen() -> Result<bool, Error> {
    eprint!("Re-open the editor to fix it? [Y/n] ");
    let _ = io::stderr().flush();
    let mut answer = String::new();
    let read = io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|e| Error::new(&codes::OUTPUT_IO, format!("cannot read the answer: {e}")))?;
    let answer = answer.trim().to_ascii_lowercase();
    Ok(read > 0 && (answer.is_empty() || answer == "y" || answer == "yes"))
}

fn draft_error(path: &Path, e: &io::Error) -> Error {
    Error::new(&codes::HOME_IO, format!("{}: {e}", path.display()))
}
