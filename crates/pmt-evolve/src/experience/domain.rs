//! Domain names (spec: experience-library, Domain names; Domain
//! resolution for a job). A domain is a directory name, so user input is
//! normalised rather than refused: lower-cased, every run of other
//! characters (`/`, `\`, whitespace, punctuation) turned into one `-`,
//! `-` trimmed; an empty result is `general`. Only a name that is still
//! invalid (too long) is refused, with exit 2.

use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

/// The domain of work that names none.
pub const GENERAL: &str = "general";

/// The longest domain name.
pub const MAX_LEN: usize = 48;

/// Lower-cases `input` and collapses runs of characters outside
/// `[a-z0-9]` into `-`, trimming `-`; empty becomes `general`.
#[must_use]
pub fn normalize(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            out.push(c);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        GENERAL.to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Whether `name` matches `[a-z0-9][a-z0-9-]{0,47}`.
#[must_use]
pub fn is_valid(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.len() <= MAX_LEN
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The normalised domain of user input.
///
/// # Errors
///
/// `experience.invalid_domain` (2) when the normalised name is still
/// invalid.
pub fn parse(input: &str) -> Result<String, Error> {
    let name = normalize(input);
    if is_valid(&name) {
        Ok(name)
    } else {
        Err(Error::new(
            &codes::EXPERIENCE_INVALID_DOMAIN,
            format!(
                "domain '{name}' is not valid: at most {MAX_LEN} lower-case letters, digits or '-'"
            ),
        ))
    }
}

/// Where a job's domain came from (`task.toml` `domain_source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// `--domain`.
    Flag,
    /// The triggering scheduled job's `domain`.
    Cron,
    /// The triggering kanban task's `domain`.
    Kanban,
    /// The source session's domain tag.
    Session,
    /// The auxiliary classification call at job start.
    #[default]
    Classifier,
}

impl Source {
    /// The spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Flag => "flag",
            Self::Cron => "cron",
            Self::Kanban => "kanban",
            Self::Session => "session",
            Self::Classifier => "classifier",
        }
    }
}

/// The domain the classifier's `reply` names, when it is one of `known`
/// (or `general`); otherwise `general`, with the WARN the spec asks for.
#[must_use]
pub fn classified(reply: &str, known: &[String]) -> String {
    let answer = reply
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .trim_matches(|c: char| c == '`' || c == '"' || c == '\'' || c == '.');
    let name = normalize(answer);
    if name == GENERAL || known.contains(&name) {
        return name;
    }
    tracing::warn!("classifier returned unknown domain '{name}'");
    GENERAL.to_owned()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn logged<T>(f: impl FnOnce() -> T) -> (T, String) {
        let buffer = Buffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        let out = tracing::subscriber::with_default(subscriber, f);
        let text = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        (out, text)
    }

    /// Scenario: Slash in domain.
    #[test]
    fn slashes_spaces_and_case_are_normalised() {
        assert_eq!(
            normalize("Business / Financial Ops"),
            "business-financial-ops"
        );
        assert_eq!(normalize(r"a\b  c"), "a-b-c");
        assert_eq!(normalize("--Rust_Refactors--"), "rust-refactors");
        assert_eq!(normalize(" / "), GENERAL);
        assert_eq!(normalize(""), GENERAL);
    }

    #[test]
    fn a_name_too_long_after_normalising_is_a_usage_error() {
        assert_eq!(parse("DevOps").unwrap(), "devops");
        let err = parse(&"a".repeat(49)).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(is_valid(&"a".repeat(48)));
    }

    /// Scenario: Classifier limited to known domains.
    #[test]
    fn the_classifier_never_invents_a_domain() {
        let known = vec!["accounting".to_owned(), "devops".to_owned()];
        let (domain, log) = logged(|| classified("marketing", &known));
        assert_eq!(domain, GENERAL);
        assert!(
            log.contains("WARN") && log.contains("classifier returned unknown domain 'marketing'"),
            "{log}"
        );
        assert_eq!(classified("`devops`\nbecause...", &known), "devops");
        assert_eq!(classified("General", &known), GENERAL);
    }
}
