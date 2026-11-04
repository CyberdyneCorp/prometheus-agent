//! First-use consent of shell hooks and gateway hook directories (spec:
//! plugin-system, Shell hooks with first-use consent; Gateway hook
//! directories).
//!
//! A shell hook is approved as `(event, command, sha256 of the files it
//! runs)` and a hook directory as `(name, sha256 of its handler)`, recorded
//! in the profile's `grants.toml`. The files a shell hook runs are its
//! program and every argument naming a file (`python3 hooks/guard.py`
//! digests the interpreter and `guard.py`), so editing the script changes
//! the digest and the hook is skipped (with a warning naming the profile)
//! until it is approved again. Only a foreground interactive chat asks; every
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
    /// The command split into words, with every argument that names a
    /// script made absolute (so the hook runs the files approved, whatever
    /// the session's working directory).
    pub words: Vec<String>,
    /// The program the first word resolves to.
    pub program: Option<PathBuf>,
    /// The files the arguments name, in argument order.
    pub scripts: Vec<PathBuf>,
    /// `sha256:<hex>` over the program and the scripts, in that order.
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

/// The file an argument names: absolute, or relative to the profile home
/// (like a relative program). Options never name files.
fn script(scope: &ProfileScope, word: &str) -> Option<PathBuf> {
    if word.starts_with('-') {
        return None;
    }
    let path = Path::new(word);
    let full = if path.is_absolute() {
        path.to_path_buf()
    } else {
        scope.dir().join(path)
    };
    full.is_file().then_some(full)
}

/// `sha256:<hex>` over the bytes of `files`, in order.
fn files_digest(files: &[&Path]) -> Option<String> {
    let contents: Vec<Vec<u8>> = files
        .iter()
        .map(|f| std::fs::read(f).ok())
        .collect::<Option<_>>()?;
    let parts: Vec<&[u8]> = contents.iter().map(Vec::as_slice).collect();
    Some(pmt_plugins::digest::of_bytes(&parts))
}

/// One `[[hooks.<event>]]` entry resolved against the profile.
fn shell_entry(scope: &ProfileScope, event: &str, hook: &ShellHook) -> ShellEntry {
    let mut words = shell_words(&hook.command).unwrap_or_default();
    let program = words.first().and_then(|first| program(scope, first));
    let mut scripts = Vec::new();
    for word in words.iter_mut().skip(1) {
        if let Some(file) = script(scope, word) {
            *word = file.display().to_string();
            scripts.push(file);
        }
    }
    let digest = program.as_deref().and_then(|program| {
        let files: Vec<&Path> = std::iter::once(program)
            .chain(scripts.iter().map(PathBuf::as_path))
            .collect();
        files_digest(&files)
    });
    ShellEntry {
        event: event.to_owned(),
        hook: hook.clone(),
        words,
        program,
        scripts,
        digest,
    }
}

/// Every `[[hooks.<event>]]` entry of the profile, in config order.
#[must_use]
pub fn shell_entries(scope: &ProfileScope) -> Vec<ShellEntry> {
    let config = scope.config();
    config
        .hooks
        .events
        .iter()
        .flat_map(|(event, hooks)| hooks.iter().map(|hook| shell_entry(scope, event, hook)))
        .collect()
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
        for script in &entry.scripts {
            let _ = write!(question, "\n  script {}", script.display());
        }
        if consent == Consent::Changed {
            question.push_str("\n  (its program or script changed since you approved it)");
        }
        question.push_str("\nRun it in this profile's sessions?");
        if crate::prompt::confirm(&question)? {
            approve_shell(&file, &entry)?;
        }
    }
    Ok(())
}
