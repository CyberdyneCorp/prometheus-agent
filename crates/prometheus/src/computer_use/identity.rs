//! The driver's identity on macOS (spec: computer-use, Driver runtime
//! management and identity): the Accessibility and Screen Recording grants
//! belong to `CuaDriver.app` (`com.trycua.driver`), so a private runtime
//! is launched only through the bundle that carries the resolved binary,
//! and only when `codesign --verify` validates its signature (code and
//! sealed resources unmodified) against a requirement naming exactly the
//! official identifier and one of the official teams. An unsigned or
//! ad-hoc build passes only with `computer_use.allow_unsigned_driver =
//! true`, and an ad-hoc signature must still be intact.
//!
//! `codesign -dv` only prints the embedded signature without validating
//! it, so it is read solely to word a refusal. The tool is always the
//! system's `/usr/bin/codesign`, never one found on `PATH`.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

/// The bundle identifier of the official driver.
pub const BUNDLE_ID: &str = "com.trycua.driver";
/// The teams that sign official releases.
pub const TEAMS: &[&str] = &["4YEC26S9KF", "YCK386LBJ7"];
/// The signature tool.
pub const CODESIGN: &str = "/usr/bin/codesign";

/// The bundle carrying `driver` (`<x>.app/Contents/MacOS/<bin>`).
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

/// The requirement an official build satisfies: an Apple-issued
/// certificate chain, the official identifier and an official team.
#[must_use]
pub fn requirement() -> String {
    let teams = TEAMS
        .iter()
        .map(|team| format!("certificate leaf[subject.OU] = \"{team}\""))
        .collect::<Vec<_>>()
        .join(" or ");
    format!("anchor apple generic and identifier \"{BUNDLE_ID}\" and ({teams})")
}

/// Checks `app`'s code signature.
///
/// # Errors
///
/// Why it was refused: unsigned, modified, another identifier or team, or
/// `codesign` unavailable.
pub fn verify(app: &Path, allow_unsigned: bool) -> Result<(), String> {
    let refused =
        |why: String| format!("the code signature check refused {}: {why}", app.display());
    let required = format!("-R={}", requirement());
    let official = codesign(app, &["--verify", "--strict", "--deep", &required])?;
    if official.status.success() {
        return Ok(());
    }
    let described = codesign(app, &["-dv"])?;
    let report = String::from_utf8_lossy(&described.stderr);
    if !described.status.success() {
        if allow_unsigned {
            return Ok(());
        }
        return Err(refused(format!(
            "it is not signed ({}); set computer_use.allow_unsigned_driver = true only for a local driver build",
            last_line(&report)
        )));
    }
    judge(&report, allow_unsigned).map_err(refused)?;
    // The identity reads as acceptable, so either an official signature is
    // broken or an allowed ad-hoc one must still be intact.
    let intact = codesign(app, &["--verify", "--strict", "--deep"])?;
    if intact.status.success() && allow_unsigned && is_adhoc(&report) {
        return Ok(());
    }
    let failed = if intact.status.success() {
        &official
    } else {
        &intact
    };
    Err(refused(format!(
        "its signature does not verify ({})",
        first_line(&String::from_utf8_lossy(&failed.stderr))
    )))
}

/// Runs `/usr/bin/codesign <args> <app>`, bounded.
fn codesign(app: &Path, args: &[&str]) -> Result<Output, String> {
    let mut command = Command::new(CODESIGN);
    command
        .args(args)
        .arg(app)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    pmt_sandbox::container::run_bounded(command, Duration::from_secs(15)).map_err(|e| {
        format!(
            "the code signature check of {} could not run: {e}",
            app.display()
        )
    })
}

fn last_line(text: &str) -> &str {
    text.lines().last().unwrap_or("codesign failed").trim()
}

fn first_line(text: &str) -> &str {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("codesign failed")
        .trim()
}

/// A `codesign -dv` field (it reports on stderr; the first occurrence of a
/// key wins).
fn field<'a>(report: &'a str, key: &str) -> &'a str {
    report
        .lines()
        .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
        .map(str::trim)
        .unwrap_or_default()
}

fn is_adhoc(report: &str) -> bool {
    field(report, "Signature") == "adhoc"
        || matches!(field(report, "TeamIdentifier"), "" | "not set")
}

/// The verdict on the identity `codesign -dv` reports.
fn judge(report: &str, allow_unsigned: bool) -> Result<(), String> {
    let identifier = field(report, "Identifier");
    if identifier != BUNDLE_ID {
        return Err(format!(
            "its identifier is '{identifier}', expected '{BUNDLE_ID}'"
        ));
    }
    let team = field(report, "TeamIdentifier");
    if TEAMS.contains(&team) || (allow_unsigned && is_adhoc(report)) {
        return Ok(());
    }
    Err(format!(
        "it is signed by team '{team}', expected one of {}; set computer_use.allow_unsigned_driver = true only for a local driver build",
        TEAMS.join(", ")
    ))
}

#[cfg(test)]
mod tests;
