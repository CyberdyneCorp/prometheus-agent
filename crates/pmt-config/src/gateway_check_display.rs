//! Checks of `[gateway.display]` beyond its schema (spec: messaging-
//! gateway, Display policy and per-platform overrides): a status phrase
//! file lies under the profile directory and holds at most 80 phrases of
//! at most 160 characters.

use std::path::{Component, Path};

use pmt_core::error::{Error, codes};

use crate::Config;
use crate::schema::StatusPhrases;

/// Phrases a status phrase file may hold, and their length.
const MAX_PHRASES: usize = 80;
const MAX_PHRASE_CHARS: usize = 160;

/// Refuses a status phrase file outside the profile or over its limits.
///
/// # Errors
///
/// `config.invalid` naming the key.
pub(crate) fn check(config: &Config, profile_dir: &Path) -> Result<(), Error> {
    let display = &config.gateway.display;
    let all = display
        .status_phrases
        .as_ref()
        .map(|p| ("gateway.display.status_phrases".to_owned(), p));
    let own = display.platforms.iter().filter_map(|(name, table)| {
        table.status_phrases.as_ref().map(|p| {
            (
                format!("gateway.display.platforms.{name}.status_phrases"),
                p,
            )
        })
    });
    for (key, phrases) in all.into_iter().chain(own) {
        check_phrases(&key, phrases, profile_dir)?;
    }
    Ok(())
}

fn check_phrases(key: &str, phrases: &StatusPhrases, profile_dir: &Path) -> Result<(), Error> {
    let invalid = |why: String| Error::new(&codes::CONFIG_INVALID, format!("{key}.path: {why}"));
    let path = Path::new(&phrases.path);
    let inside = path
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if !inside {
        return Err(invalid(format!(
            "'{}' must be a relative path inside the profile directory",
            phrases.path
        )));
    }
    let Ok(text) = std::fs::read_to_string(profile_dir.join(path)) else {
        // A file not written yet: the built-in phrases apply until it is.
        return Ok(());
    };
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.len() > MAX_PHRASES {
        return Err(invalid(format!(
            "{} phrases; at most {MAX_PHRASES}",
            lines.len()
        )));
    }
    if let Some(long) = lines.iter().find(|l| l.chars().count() > MAX_PHRASE_CHARS) {
        let start: String = long.chars().take(30).collect();
        return Err(invalid(format!(
            "the phrase starting '{start}' is over {MAX_PHRASE_CHARS} characters"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(toml: &str) -> Config {
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn a_phrase_file_outside_the_profile_or_too_long_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = config("[gateway.display]\nstatus_phrases = { path = \"../x.txt\" }\n");
        let err = check(&outside, dir.path()).unwrap_err();
        assert!(
            err.message()
                .contains("gateway.display.status_phrases.path")
        );
        std::fs::write(dir.path().join("p.txt"), "ok\n".repeat(81)).unwrap();
        let many = config(
            "[gateway.display.platforms.slack]\nstatus_phrases = { path = \"p.txt\", mode = \"replace\" }\n",
        );
        let err = check(&many, dir.path()).unwrap_err();
        assert!(err.message().contains("81 phrases; at most 80"), "{err}");
        std::fs::write(dir.path().join("p.txt"), "Pondering…\nBrewing…\n").unwrap();
        check(&many, dir.path()).unwrap();
    }
}
