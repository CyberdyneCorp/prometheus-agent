//! Turning `config.toml` text into a validated [`Config`], with every error
//! located as `path:line:column`.

use std::path::Path;

use pmt_core::error::{Error, codes};

use crate::schema::Config;
use crate::validate::{self, Issue, Node};

/// A parsed document that keeps source spans (for line numbers).
pub(crate) type SpannedDocument = toml_edit::Document<String>;

/// Parses TOML; a syntax error is `config.parse` with its position.
pub(crate) fn document(path: &Path, text: &str) -> Result<SpannedDocument, Error> {
    text.parse::<SpannedDocument>().map_err(|e| {
        let position = e
            .span()
            .map(|span| {
                let (line, column) = line_col(text, span.start);
                format!(":{line}:{column}")
            })
            .unwrap_or_default();
        let detail = e.message().trim_end();
        Error::new(
            &codes::CONFIG_PARSE,
            format!("{}{position}: {detail}", path.display()),
        )
        .with_hint("fix the file with: prometheus config edit")
    })
}

/// Parses, validates and deserializes `text`. Every problem is reported in
/// one error, one per line; the code is the first problem's.
///
/// # Errors
///
/// `config.parse`, `config.unknown_key` or `config.invalid` (exit 78).
pub(crate) fn config(path: &Path, text: &str) -> Result<Config, Error> {
    let doc = document(path, text)?;
    let issues = validate::document(&Node::from_item(doc.as_item()));
    if !issues.is_empty() {
        return Err(issues_error(path, text, &issues));
    }
    toml::from_str(text).map_err(|e| {
        // The validator mirrors the schema, so this only fires on a gap
        // between the two; still report it with its position.
        let position = e
            .span()
            .map(|s| {
                let (line, column) = line_col(text, s.start);
                format!(":{line}:{column}")
            })
            .unwrap_or_default();
        Error::new(
            &codes::CONFIG_INVALID,
            format!("{}{position}: {}", path.display(), e.message()),
        )
    })
}

/// One error listing every issue as `path:line:column: message`.
pub(crate) fn issues_error(path: &Path, text: &str, issues: &[Issue]) -> Error {
    let lines: Vec<String> = issues
        .iter()
        .map(|issue| {
            let position = issue
                .span
                .as_ref()
                .map(|s| {
                    let (line, column) = line_col(text, s.start);
                    format!(":{line}:{column}")
                })
                .unwrap_or_default();
            format!("{}{position}: {}", path.display(), issue.message)
        })
        .collect();
    let code = issues.first().map_or(&codes::CONFIG_INVALID, |i| i.code);
    Error::new(code, lines.join("\n")).with_hint(
        "fix the file with: prometheus config edit (keys: docs/reference/configuration.md)",
    )
}

/// 1-based line and column (in characters) of a byte offset.
pub(crate) fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    (line, before[line_start..].chars().count() + 1)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{config, line_col};

    #[test]
    fn positions_are_one_based() {
        assert_eq!(line_col("a\nbc", 0), (1, 1));
        assert_eq!(line_col("a\nbc", 3), (2, 2));
    }

    #[test]
    fn unknown_enum_names_key_line_and_allowed_values() {
        let err = config(Path::new("config.toml"), "[approvals]\nmode = \"of\"\n").unwrap_err();
        assert_eq!(err.exit_code(), 78);
        assert_eq!(
            err.message(),
            "config.toml:2:8: approvals.mode: unknown value \"of\" (allowed: \"manual\", \"smart\", \"off\")"
        );
    }

    #[test]
    fn yaml_style_off_does_not_parse() {
        let err = config(Path::new("config.toml"), "[approvals]\nmode = off\n").unwrap_err();
        assert_eq!(err.code().code, "config.parse");
        assert_eq!(err.exit_code(), 78);
        assert!(err.message().starts_with("config.toml:2:8: "), "{err}");
    }

    #[test]
    fn quoted_off_is_the_off_mode() {
        let parsed = config(Path::new("c"), "[approvals]\nmode = \"off\"\n").unwrap();
        assert_eq!(parsed.approvals.mode, crate::schema::ApprovalMode::Off);
    }

    #[test]
    fn every_error_is_printed() {
        let err = config(
            Path::new("c"),
            "[agent]\nmax_iteration = 40\n[compression]\nmax_attempts = 50\n",
        )
        .unwrap_err();
        assert_eq!(err.code().code, "config.unknown_key");
        assert_eq!(
            err.message(),
            "c:2:1: unknown key 'agent.max_iteration' (did you mean 'agent.max_iterations'?)\n\
             c:4:16: compression.max_attempts: must be between 1 and 10"
        );
    }
}
