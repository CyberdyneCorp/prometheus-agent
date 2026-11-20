//! The driver's identity on macOS (spec: computer-use, Driver runtime
//! management and identity): the Accessibility and Screen Recording grants
//! belong to `CuaDriver.app` (`com.trycua.driver`), so a private runtime
//! is launched only through the bundle that carries the resolved binary,
//! and only when `codesign` reports exactly the official identifier and
//! one of the official teams. An ad-hoc or unsigned build passes only with
//! `computer_use.allow_unsigned_driver = true`.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// The bundle identifier of the official driver.
pub const BUNDLE_ID: &str = "com.trycua.driver";
/// The teams that sign official releases.
pub const TEAMS: &[&str] = &["4YEC26S9KF", "YCK386LBJ7"];

/// The `.app` bundle carrying `driver` (`<x>.app/Contents/MacOS/<bin>`).
#[must_use]
pub fn bundle(driver: &Path) -> Option<PathBuf> {
    let real = std::fs::canonicalize(driver).unwrap_or_else(|_| driver.to_path_buf());
    let macos = real.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    let is_bundle = macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && app.extension().is_some_and(|e| e == "app");
    is_bundle.then(|| app.to_path_buf())
}

/// Checks `app`'s code signature with `codesign` (found on `path_var`).
///
/// # Errors
///
/// What the signature check found: unsigned, another identifier or team,
/// or `codesign` unavailable.
pub fn verify(app: &Path, allow_unsigned: bool, codesign: &Path) -> Result<(), String> {
    let mut command = Command::new(codesign);
    command
        .args(["-dv"])
        .arg(app)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let output =
        pmt_sandbox::container::run_bounded(command, Duration::from_secs(15)).map_err(|e| {
            format!(
                "the code signature check of {} could not run: {}",
                app.display(),
                e
            )
        })?;
    let report = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        if allow_unsigned {
            return Ok(());
        }
        return Err(format!(
            "the code signature check refused {}: it is not signed ({}); set computer_use.allow_unsigned_driver = true only for a local driver build",
            app.display(),
            report.lines().last().unwrap_or("codesign failed").trim()
        ));
    }
    judge(&report, allow_unsigned)
        .map_err(|why| format!("the code signature check refused {}: {why}", app.display()))
}

/// The verdict on `codesign -dv` output (it reports on stderr; the first
/// occurrence of a key wins).
fn judge(report: &str, allow_unsigned: bool) -> Result<(), String> {
    let field = |key: &str| {
        report
            .lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .map(str::trim)
            .unwrap_or_default()
    };
    let identifier = field("Identifier");
    if identifier != BUNDLE_ID {
        return Err(format!(
            "its identifier is '{identifier}', expected '{BUNDLE_ID}'"
        ));
    }
    let team = field("TeamIdentifier");
    if TEAMS.contains(&team) || (allow_unsigned && matches!(team, "" | "not set")) {
        return Ok(());
    }
    Err(format!(
        "it is signed by team '{team}', expected one of {}; set computer_use.allow_unsigned_driver = true only for a local driver build",
        TEAMS.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::{bundle, judge};

    #[test]
    fn only_the_official_identity_passes() {
        let official = "Executable=/x\nIdentifier=com.trycua.driver\nTeamIdentifier=4YEC26S9KF\n";
        assert_eq!(judge(official, false), Ok(()));
        let adhoc = "Identifier=com.trycua.driver\nTeamIdentifier=not set\n";
        assert!(judge(adhoc, false).unwrap_err().contains("team 'not set'"));
        assert_eq!(judge(adhoc, true), Ok(()));
        let impostor = "Identifier=com.trycua.driver.evil\nTeamIdentifier=4YEC26S9KF\n";
        assert!(judge(impostor, true).unwrap_err().contains("identifier"));
    }

    #[test]
    fn a_bundle_is_found_only_around_its_binary() {
        let temp = tempfile::tempdir().unwrap();
        let exe = temp.path().join("CuaDriver.app/Contents/MacOS/cua-driver");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, "").unwrap();
        let found = bundle(&exe).unwrap();
        assert!(found.ends_with("CuaDriver.app"));
        let loose = temp.path().join("cua-driver");
        std::fs::write(&loose, "").unwrap();
        assert_eq!(bundle(&loose), None);
    }

    /// Scenario: Unsigned bundle (the real `codesign`).
    #[cfg(target_os = "macos")]
    #[test]
    fn an_unsigned_bundle_fails_the_signature_check() {
        let temp = tempfile::tempdir().unwrap();
        let app = temp.path().join("CuaDriver.app");
        let exe = app.join("Contents/MacOS/cua-driver");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, "#!/bin/sh\nexit 0\n").unwrap();
        let why =
            super::verify(&app, false, std::path::Path::new("/usr/bin/codesign")).unwrap_err();
        assert!(why.contains("code signature check refused"), "{why}");
    }
}
