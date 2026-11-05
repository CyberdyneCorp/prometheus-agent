//! Sudo password handling (spec: execution-environments, Sudo password
//! handling).
//!
//! A command whose executable, after optional `env` options and variable
//! assignments, is literally `sudo` (bare, absolute or quoted path) gets
//! the password from, in order, the session's cached password, the secret
//! `SUDO_PASSWORD` (when the session holds `secrets.read:SUDO_PASSWORD`)
//! or a masked prompt on the session's interactive surface, whose answer
//! is cached in memory for the session only. Shell payloads (`bash -c`,
//! `env -S`) and dynamic paths (`$SUDO`) are never rewritten.
//!
//! The password reaches sudo only through the command's environment
//! ([`PASSWORD_VAR`], a hidden secret variable) and only for a credential
//! check: the command first runs `sudo -S -p '' -v` with the password on
//! its stdin, drops the variable, then runs the original command with
//! `-S` added and its usual (closed) stdin. A command that needs no
//! password therefore never sees the password line on its stdin. Known
//! limit: with sudo's credential cache off (`timestamp_timeout=0`) the
//! second `sudo` asks again and fails.
//!
//! Subagents, cron, single-query runs and surfaces without a secure prompt
//! never prompt; a failed sudo command there explains why, and a messaging
//! session points at `prometheus secrets set SUDO_PASSWORD`. The password
//! never appears in a result: the output is scrubbed of it.

use std::sync::{Arc, Mutex, PoisonError};

use pmt_core::secret::SecretString;

use super::result::{Status, TerminalResult};

/// The child variable carrying the password to the credential check.
pub const PASSWORD_VAR: &str = "PMT_SUDO_PASSWORD";

/// The secret a session may be granted (`secrets.read:SUDO_PASSWORD`).
pub const SECRET_NAME: &str = "SUDO_PASSWORD";

/// Asks the person for the sudo password (a hidden line, or `sudo.request`
/// to a daemon session's clients). Called on a blocking thread.
pub trait SudoPrompt: Send + Sync {
    /// The password for `command`; `None` when declined or unanswered.
    fn ask(&self, command: &str) -> Option<SecretString>;
}

/// Who runs the session's commands, which decides whether a prompt is
/// possible and what a failure says.
#[derive(Clone)]
pub enum Surface {
    /// A person at a secure prompt.
    Interactive(Arc<dyn SudoPrompt>),
    /// A messaging conversation (no secure prompt there).
    Messaging,
    /// A delegated subagent.
    Subagent,
    /// Cron, single-query and other runs nobody watches.
    Unattended,
}

/// Reads the `SUDO_PASSWORD` secret of a session that holds its grant.
pub type SecretLookup = Arc<dyn Fn() -> Option<SecretString> + Send + Sync>;

/// One session's sudo handling.
pub struct Sudo {
    surface: Surface,
    secret: Option<SecretLookup>,
    cached: Mutex<Option<SecretString>>,
}

/// What a command needs from sudo handling.
pub enum Elevation {
    /// Not a literal sudo command: runs as written.
    NotSudo,
    /// A sudo command without a password: runs as written; a failure
    /// explains why nobody was asked.
    NoPassword,
    /// The rewritten command and the password its environment carries.
    Elevated {
        /// The command to run.
        command: String,
        /// The value of [`PASSWORD_VAR`].
        password: SecretString,
    },
}

impl Sudo {
    /// Sudo handling on `surface`; `secret` reads `SUDO_PASSWORD` and is
    /// given only to a session holding `secrets.read:SUDO_PASSWORD`.
    #[must_use]
    pub fn new(surface: Surface, secret: Option<SecretLookup>) -> Self {
        Self {
            surface,
            secret,
            cached: Mutex::new(None),
        }
    }

    /// What `command` needs; may prompt (blocking), so call it on a
    /// blocking thread.
    #[must_use]
    pub fn elevate(&self, command: &str) -> Elevation {
        let Some(rewritten) = rewrite(command) else {
            return Elevation::NotSudo;
        };
        match self.password(command) {
            Some(password) => Elevation::Elevated {
                command: rewritten,
                password,
            },
            None => Elevation::NoPassword,
        }
    }

    fn password(&self, command: &str) -> Option<SecretString> {
        let mut cached = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(password) = cached.as_ref() {
            return Some(password.clone());
        }
        if let Some(password) = self.secret.as_ref().and_then(|lookup| lookup()) {
            return Some(password);
        }
        let Surface::Interactive(prompt) = &self.surface else {
            return None;
        };
        let answer = prompt.ask(command).filter(|p| !p.expose().is_empty())?;
        *cached = Some(answer.clone());
        Some(answer)
    }

    /// Finishes `result` of a command run with `elevation`: the password
    /// is scrubbed from it, a rejected password is forgotten, and a failed
    /// sudo command that had no password says why.
    pub fn finish(&self, elevation: &Elevation, result: &mut TerminalResult) {
        let failed = matches!(result.status, Status::Error);
        match elevation {
            Elevation::Elevated { password, .. } => {
                scrub(result, password.expose());
                if failed && rejected(&result.output) {
                    self.forget(password);
                    add_hint(
                        result,
                        "sudo rejected the password; the next sudo command asks again",
                    );
                }
            }
            Elevation::NoPassword if failed => add_hint(result, &self.no_password_hint()),
            Elevation::NoPassword | Elevation::NotSudo => {}
        }
    }

    /// Drops `password` from the cache (a wrong cached or typed one).
    fn forget(&self, password: &SecretString) {
        let mut cached = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        if cached
            .as_ref()
            .is_some_and(|c| c.expose() == password.expose())
        {
            *cached = None;
        }
    }

    fn no_password_hint(&self) -> String {
        let store = format!(
            "store {SECRET_NAME} with `prometheus secrets set {SECRET_NAME}` and grant secrets.read:{SECRET_NAME}"
        );
        match self.surface {
            Surface::Subagent => format!(
                "subagents cannot prompt for sudo passwords; {store}, or run the command from the parent session"
            ),
            Surface::Messaging => format!("sudo needs a password: {store}"),
            Surface::Unattended => {
                format!("this session cannot prompt for sudo passwords; {store}")
            }
            Surface::Interactive(_) => "no sudo password was given".to_owned(),
        }
    }
}

/// Whether sudo's output says the password was wrong.
fn rejected(output: &str) -> bool {
    [
        "Sorry, try again",
        "incorrect password",
        "no password was provided",
    ]
    .iter()
    .any(|marker| output.contains(marker))
}

fn scrub(result: &mut TerminalResult, password: &str) {
    if password.is_empty() {
        return;
    }
    for text in [Some(&mut result.output), result.hint.as_mut()]
        .into_iter()
        .flatten()
    {
        if text.contains(password) {
            *text = text.replace(password, "[REDACTED]");
        }
    }
}

fn add_hint(result: &mut TerminalResult, hint: &str) {
    result.hint = Some(match result.hint.take() {
        Some(other) => format!("{hint}. {other}"),
        None => hint.to_owned(),
    });
}

/// `command` rewritten to check sudo's credentials with the password from
/// [`PASSWORD_VAR`] first, or `None` when its executable is not literally
/// `sudo`.
#[must_use]
pub fn rewrite(command: &str) -> Option<String> {
    let sudo = find_sudo(command)?;
    let raw = &command[sudo.start..sudo.end];
    Some(format!(
        "printf '%s\\n' \"${PASSWORD_VAR}\" | {raw} -S -p '' -v && unset {PASSWORD_VAR} && {{\n{} -S -p ''{}\n}}",
        &command[..sudo.end],
        &command[sudo.end..]
    ))
}

/// Where the `sudo` word of a command is.
struct Span {
    start: usize,
    end: usize,
}

/// One shell word at the start of `text[at..]`.
struct Word {
    start: usize,
    end: usize,
    value: String,
    /// It holds an unquoted or double-quoted expansion (`$`, backquote).
    dynamic: bool,
}

/// The `sudo` word of the command's first simple command, after optional
/// assignments and an `env` with its options and assignments.
fn find_sudo(command: &str) -> Option<Span> {
    let mut word = next_word(command, 0)?;
    word = skip_assignments(command, word)?;
    if !word.dynamic && is_program(&word.value, "env") {
        word = skip_env_options(command, &word)?;
        word = skip_assignments(command, word)?;
    }
    (!word.dynamic && is_program(&word.value, "sudo")).then_some(Span {
        start: word.start,
        end: word.end,
    })
}

/// `name` bare or as an absolute path.
fn is_program(value: &str, name: &str) -> bool {
    value == name || value.starts_with('/') && value.rsplit('/').next() == Some(name)
}

fn skip_assignments(command: &str, mut word: Word) -> Option<Word> {
    while is_assignment(&command[word.start..word.end]) {
        word = next_word(command, word.end)?;
    }
    Some(word)
}

/// Whether the raw word is `NAME=value` with an unquoted name.
fn is_assignment(raw: &str) -> bool {
    let Some((name, _)) = raw.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Skips `env`'s options; `None` for `-S`/`--split-string`, whose payload
/// is a shell-like string that is never rewritten.
fn skip_env_options(command: &str, env: &Word) -> Option<Word> {
    let mut word = next_word(command, env.end)?;
    loop {
        let value = word.value.as_str();
        if value == "--" {
            return next_word(command, word.end);
        }
        if !value.starts_with('-') || value == "-" {
            return Some(word);
        }
        if value.starts_with("-S") || value.starts_with("--split-string") {
            return None;
        }
        let takes_value = matches!(value, "-u" | "-C" | "--unset" | "--chdir");
        word = next_word(command, word.end)?;
        if takes_value {
            word = next_word(command, word.end)?;
        }
    }
}

/// The word starting after blanks at `from`; `None` at the end of the
/// first simple command (an operator, a newline or the end).
fn next_word(text: &str, from: usize) -> Option<Word> {
    let bytes = text.as_bytes();
    let mut at = from;
    while at < bytes.len() && matches!(bytes[at], b' ' | b'\t') {
        at += 1;
    }
    let start = at;
    let mut word = Word {
        start,
        end: start,
        value: String::new(),
        dynamic: false,
    };
    while at < bytes.len() {
        match bytes[at] {
            b' ' | b'\t' | b'\n' | b';' | b'&' | b'|' | b'<' | b'>' | b'(' | b')' => break,
            b'\'' => {
                let close = text[at + 1..].find('\'')? + at + 1;
                word.value.push_str(&text[at + 1..close]);
                at = close + 1;
            }
            b'"' => at = double_quoted(text, at, &mut word)?,
            b'\\' => {
                let next = text[at + 1..].chars().next()?;
                word.value.push(next);
                at += 1 + next.len_utf8();
            }
            b'$' | b'`' => {
                word.dynamic = true;
                at += 1;
            }
            _ => {
                let c = text[at..].chars().next()?;
                word.value.push(c);
                at += c.len_utf8();
            }
        }
    }
    word.end = at;
    (at > start).then_some(word)
}

/// Reads the double-quoted part opening at `open`; returns the index after
/// its closing quote.
fn double_quoted(text: &str, open: usize, word: &mut Word) -> Option<usize> {
    let mut at = open + 1;
    loop {
        let c = text[at..].chars().next()?;
        match c {
            '"' => return Some(at + 1),
            '$' | '`' => word.dynamic = true,
            '\\' => {
                let next = text[at + 1..].chars().next()?;
                word.value.push(next);
                at += 1 + next.len_utf8();
                continue;
            }
            _ => word.value.push(c),
        }
        at += c.len_utf8();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sudo_word(command: &str) -> Option<&str> {
        find_sudo(command).map(|span| &command[span.start..span.end])
    }

    #[test]
    fn literal_sudo_after_env_and_assignments_is_found() {
        for (command, word) in [
            ("sudo apt-get update", "sudo"),
            ("/usr/bin/sudo id", "/usr/bin/sudo"),
            ("'sudo' id", "'sudo'"),
            ("\"/usr/bin/sudo\" -u root id", "\"/usr/bin/sudo\""),
            ("FOO=1 sudo id", "sudo"),
            ("env -i PATH=/bin sudo id", "sudo"),
            ("env -u HOME -- sudo id", "sudo"),
            ("/usr/bin/env FOO=\"a b\" sudo id && echo ok", "sudo"),
        ] {
            assert_eq!(sudo_word(command), Some(word), "{command}");
        }
    }

    #[test]
    fn payloads_and_dynamic_paths_are_left_alone() {
        for command in [
            "bash -c 'sudo id'",
            "env -S 'sudo id'",
            "env --split-string='sudo id'",
            "$SUDO id",
            "\"$HOME/bin/sudo\" id",
            "echo sudo",
            "sudoedit /etc/hosts",
            "bin/sudo id",
            "",
        ] {
            assert_eq!(sudo_word(command), None, "{command}");
        }
    }

    #[test]
    fn the_rewrite_checks_credentials_then_runs_the_command_unchanged() {
        assert_eq!(
            rewrite("FOO=1 sudo apt-get update # refresh").unwrap(),
            "printf '%s\\n' \"$PMT_SUDO_PASSWORD\" | sudo -S -p '' -v && unset PMT_SUDO_PASSWORD && {\nFOO=1 sudo -S -p '' apt-get update # refresh\n}"
        );
    }
}
