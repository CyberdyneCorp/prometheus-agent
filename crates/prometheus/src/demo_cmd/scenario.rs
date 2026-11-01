//! The bundled demo scenario: a small ledger repository whose balance
//! script adds debits instead of subtracting them, and the mock script
//! that drives the agent through finding and fixing that bug.
//!
//! The script is POSIX `sh` (integer arithmetic only, so even Android's
//! shell runs it) and PowerShell on Windows, so the demo needs no runtime
//! the host may lack (spec: workspace-architecture, No mandatory external
//! runtime).

use std::path::Path;

use pmt_core::error::{Error, codes};

/// The answer the demo must end with.
pub const EXPECTED: &str = "Total: 85.15";

/// The user's request.
pub(super) const PROMPT: &str = "The balance that this ledger repository's script prints looks wrong. Find the bug, fix it and tell me the correct total.";

const LEDGER: &str = include_str!("scenario/ledger.txt");
const README: &str = include_str!("scenario/README.md");
const MOCK: &str = include_str!("scenario/mock.toml");

/// The platform's balance script.
struct Script {
    file: &'static str,
    text: &'static str,
    run: &'static str,
    bug: &'static str,
    fix: &'static str,
}

#[cfg(not(windows))]
const SCRIPT: Script = Script {
    file: "total.sh",
    text: include_str!("scenario/total.sh"),
    run: "sh total.sh",
    bug: "debit) total=$((total + cents))",
    fix: "debit) total=$((total - cents))",
};

/// Run in the terminal's own PowerShell (inside the Windows sandbox the
/// terminal always uses PowerShell): a nested Windows PowerShell would
/// spend ~20 s failing to auto-load its modules there.
#[cfg(windows)]
const SCRIPT: Script = Script {
    file: "total.ps1",
    text: include_str!("scenario/total.ps1"),
    run: "& .\\total.ps1",
    bug: "debit { $total += $cents }",
    fix: "debit { $total -= $cents }",
};

/// Writes the repository into `repository` and the mock script to
/// `mock_script`.
///
/// # Errors
///
/// `demo.io` (74).
pub(super) fn write(repository: &Path, mock_script: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(repository).map_err(|e| io_error(repository, &e))?;
    let files = [
        (repository.join("ledger.txt"), LEDGER.to_owned()),
        (repository.join("README.md"), fill(README)),
        (repository.join(SCRIPT.file), SCRIPT.text.to_owned()),
        (mock_script.to_path_buf(), fill(MOCK)),
    ];
    for (path, text) in files {
        std::fs::write(&path, text).map_err(|e| io_error(&path, &e))?;
    }
    Ok(())
}

/// Replaces the template's placeholders with this platform's script.
fn fill(template: &str) -> String {
    template
        .replace("@SCRIPT@", SCRIPT.file)
        .replace("@RUN@", SCRIPT.run)
        .replace("@BUG@", SCRIPT.bug)
        .replace("@FIX@", SCRIPT.fix)
}

fn io_error(path: &Path, err: &std::io::Error) -> Error {
    Error::new(
        &codes::DEMO_IO,
        format!("cannot write {}: {err}", path.display()),
    )
}

#[cfg(test)]
mod tests {
    use super::{MOCK, SCRIPT, fill};

    #[test]
    fn the_bundled_script_has_exactly_the_bug_the_mock_fixes() {
        assert_eq!(SCRIPT.text.matches(SCRIPT.bug).count(), 1);
        assert!(!SCRIPT.text.contains(SCRIPT.fix));
    }

    #[test]
    fn the_filled_mock_script_parses() {
        let filled = fill(MOCK);
        assert!(!filled.contains('@'), "{filled}");
        pmt_providers::mock::Script::parse(&filled, "demo").unwrap();
    }
}
