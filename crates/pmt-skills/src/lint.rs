//! Skill authoring lint (spec: skills, Skill authoring lint; Description
//! budget and advisory lint on agent writes).
//!
//! Findings are advice: `skill_manage` returns the ones a write introduced
//! and never blocks on them; `prometheus skills lint --strict` fails on
//! any.

use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;

use crate::frontmatter::{self, MAX_SKILL_CHARS};
use crate::meta::Meta;

/// Words that describe nothing.
pub const BANNED_WORDS: [&str; 7] = [
    "powerful",
    "comprehensive",
    "seamless",
    "revolutionary",
    "cutting-edge",
    "state-of-the-art",
    "advanced",
];

static HOME_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(/Users/[^/\s]+/|/home/[^/\s]+/|[A-Za-z]:\\Users\\[^\\\s]+\\)")
        .expect("home path pattern")
});

/// One finding.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Finding {
    /// Rule id.
    pub rule: String,
    /// What to fix.
    pub message: String,
}

fn finding(rule: &str, message: impl Into<String>) -> Finding {
    Finding {
        rule: rule.to_owned(),
        message: message.into(),
    }
}

/// What a lint run needs besides the text.
#[derive(Clone, Copy)]
pub struct LintContext<'a> {
    /// The skill directory's name.
    pub dir_name: &'a str,
    /// `skills.index_description_chars`.
    pub description_chars: usize,
    /// Whether a skill name resolves (for `related_skills`).
    pub resolves: &'a dyn Fn(&str) -> bool,
}

/// Lints a `SKILL.md`.
#[must_use]
pub fn lint(text: &str, ctx: &LintContext<'_>) -> Vec<Finding> {
    let doc = match frontmatter::parse(text) {
        Ok(doc) => doc,
        Err(e) => {
            return vec![finding(
                "frontmatter",
                format!("line {}: {}", e.line, e.reason),
            )];
        }
    };
    let mut found = Vec::new();
    if doc.name != ctx.dir_name {
        found.push(finding(
            "name_matches_dir",
            format!(
                "name '{}' differs from the directory '{}'",
                doc.name, ctx.dir_name
            ),
        ));
    }
    let chars = doc.description.chars().count();
    if chars > ctx.description_chars {
        found.push(finding(
            "description_length",
            format!(
                "description has {chars} characters (budget {})",
                ctx.description_chars
            ),
        ));
    }
    if !doc.description.ends_with('.') {
        found.push(finding(
            "description_period",
            "description should end with a period",
        ));
    }
    let lower = doc.description.to_lowercase();
    for word in BANNED_WORDS {
        if lower
            .split(|c: char| !c.is_alphanumeric() && c != '-')
            .any(|w| w == word)
        {
            found.push(finding("banned_word", format!("description uses '{word}'")));
        }
    }
    for related in Meta::of(&doc.front).related_skills {
        if !(ctx.resolves)(&related) {
            found.push(finding(
                "related_skills",
                format!("related skill '{related}' does not resolve"),
            ));
        }
    }
    if HOME_PATH.is_match(text) {
        found.push(finding("home_path", "contains a machine-local home path"));
    }
    if text.chars().count() > MAX_SKILL_CHARS {
        found.push(finding(
            "size",
            format!("SKILL.md exceeds {MAX_SKILL_CHARS} characters"),
        ));
    }
    found.sort();
    found
}

/// Findings of `after` that `before` did not have.
#[must_use]
pub fn introduced(before: &[Finding], after: &[Finding]) -> Vec<Finding> {
    after
        .iter()
        .filter(|f| !before.contains(f))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(resolves: &dyn Fn(&str) -> bool) -> LintContext<'_> {
        LintContext {
            dir_name: "logs",
            description_chars: 60,
            resolves,
        }
    }

    #[test]
    fn marketing_words_are_found() {
        let none = |_: &str| false;
        let found = lint(
            "---\nname: logs\ndescription: Advanced log analysis.\n---\nbody\n",
            &ctx(&none),
        );
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].rule, "banned_word");
        assert!(found[0].message.contains("advanced"));
    }

    #[test]
    fn clean_skill_has_no_findings() {
        let all = |_: &str| true;
        let text =
            "---\nname: logs\ndescription: Read service logs.\nrelated_skills: [x]\n---\nbody\n";
        assert!(lint(text, &ctx(&all)).is_empty());
        let none = |_: &str| false;
        assert_eq!(lint(text, &ctx(&none))[0].rule, "related_skills");
    }
}
