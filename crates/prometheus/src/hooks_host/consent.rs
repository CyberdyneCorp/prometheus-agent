//! First-use consent of shell hooks and gateway hook directories (spec:
//! plugin-system, Shell hooks with first-use consent; Gateway hook
//! directories).
//!
//! A shell hook is approved as `(event, command, sha256 of its program)`
//! and a hook directory as `(name, sha256 of its handler)`, recorded in the
//! profile's `grants.toml`. Editing the program or handler changes the
//! digest, so the hook is skipped (with a warning naming the profile) until
//! it is approved again. Only a foreground interactive chat asks; every
//! other context runs approved hooks only, unless `hooks.auto_accept` (or
//! `--accept-hooks`) accepts the rest.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pmt_config::ProfileScope;
use pmt_config::grants::{Grants, GrantsFile, HookConsent, HookKind};
use pmt_config::hooks_check::shell_words;
use pmt_config::schema::ShellHook;
use pmt_core::error::Error;

/// One `[[hooks.<event>]]` entry as found on disk.
#[derive(Debug, Clone)]
pub struct ShellEntry {
    /// The event.
    pub event: String,
    /// The entry.
    pub hook: ShellHook,
    /// The command split into words.
    pub words: Vec<String>,
    /// The program the first word resolves to.
    pub program: Option<PathBuf>,
    /// `sha256:<hex>` of the program file.
    pub digest: Option<String>,
}

/// Where a hook stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Consent {
    /// Approved at its current digest.
    Approved,
    /// Never approved.
    Pending,
    /// Approved at another digest: the program changed.
    Changed,
    /// Its program cannot be found, so nothing can be approved.
    Missing,
}

impl Consent {
    /// The word `plugins hooks list` shows.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Pending => "pending approval",
            Self::Changed => "changed since approval",
            Self::Missing => "program not found",
        }
    }
}

/// `sha256:<hex>` of a file.
#[must_use]
pub fn file_digest(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(pmt_plugins::digest::of_bytes(&[&bytes]))
}

/// The program a command's first word names: a path (relative to the
/// profile home when relative) or a name looked up on `PATH`.
#[must_use]
pub fn program(scope: &ProfileScope, first: &str) -> Option<PathBuf> {
    let path = Path::new(first);
    if path.components().count() > 1 || path.is_absolute() {
        let full = if path.is_absolute() {
            path.to_path_buf()
        } else {
            scope.dir().join(path)
        };
        return full.is_file().then_some(full);
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(first))
            .find(|candidate| candidate.is_file())
    })
}

/// Every `[[hooks.<event>]]` entry of the profile, in config order.
#[must_use]
pub fn shell_entries(scope: &ProfileScope) -> Vec<ShellEntry> {
    let mut entries = Vec::new();
    for (event, hooks) in &scope.config().hooks.events {
        for hook in hooks {
            let words = shell_words(&hook.command).unwrap_or_default();
            let program = words.first().and_then(|first| program(scope, first));
            let digest = program.as_deref().and_then(file_digest);
            entries.push(ShellEntry {
                event: event.clone(),
                hook: hook.clone(),
                words,
                program,
                digest,
            });
        }
    }
    entries
}

/// Where `entry` stands in `grants`.
#[must_use]
pub fn shell_status(grants: &Grants, entry: &ShellEntry) -> Consent {
    let Some(digest) = &entry.digest else {
        return Consent::Missing;
    };
    status(
        grants,
        HookKind::Shell,
        Some(&entry.event),
        &entry.hook.command,
        digest,
    )
}

/// Where a hook of `kind` named `subject` (a command, or a directory name)
/// at `digest` stands.
#[must_use]
pub fn status(
    grants: &Grants,
    kind: HookKind,
    event: Option<&str>,
    subject: &str,
    digest: &str,
) -> Consent {
    let same =
        |c: &&HookConsent| c.kind == kind && c.event.as_deref() == event && c.subject == subject;
    let mut matching = grants.hooks.iter().filter(same).peekable();
    if matching.peek().is_none() {
        return Consent::Pending;
    }
    if matching.any(|c| c.digest == digest) {
        Consent::Approved
    } else {
        Consent::Changed
    }
}

/// Records the approval of `entry` at its current digest.
///
/// # Errors
///
/// `grants.store`; `config.invalid` when the program cannot be found.
pub fn approve_shell(file: &GrantsFile, entry: &ShellEntry) -> Result<(), Error> {
    let digest = entry.digest.clone().ok_or_else(|| {
        Error::new(
            &pmt_core::error::codes::CONFIG_INVALID,
            format!("hook {}: its program cannot be found", entry.hook.command),
        )
    })?;
    file.consent(HookConsent {
        kind: HookKind::Shell,
        event: Some(entry.event.clone()),
        subject: entry.hook.command.clone(),
        digest,
        granted_at: jiff::Timestamp::now(),
    })
}

/// Asks, on the terminal of a foreground interactive chat, about every
/// shell hook that is not approved, and records each yes. Nothing is asked
/// when `hooks.auto_accept` accepts them all anyway.
///
/// # Errors
///
/// `grants.store`; a failed terminal read.
pub fn ask_pending(scope: &ProfileScope) -> Result<(), Error> {
    if scope.config().hooks.auto_accept || scope.config().plugins.safe_mode {
        return Ok(());
    }
    let file = GrantsFile::of(scope.profile());
    let grants = file.load()?;
    for entry in shell_entries(scope) {
        let consent = shell_status(&grants, &entry);
        if !matches!(consent, Consent::Pending | Consent::Changed) {
            continue;
        }
        let mut question = format!("Shell hook on {}: `{}`", entry.event, entry.hook.command);
        if let Some(program) = &entry.program {
            let _ = write!(question, "\n  program {}", program.display());
        }
        if consent == Consent::Changed {
            question.push_str("\n  (its program changed since you approved it)");
        }
        question.push_str("\nRun it in this profile's sessions?");
        if crate::prompt::confirm(&question)? {
            approve_shell(&file, &entry)?;
        }
    }
    Ok(())
}
