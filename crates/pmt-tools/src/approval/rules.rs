//! Allow and deny rules: command globs where `*` matches any run of
//! characters, `?` one character and `\` makes the next character literal.
//!
//! A deny rule (`approvals.deny`) blocks when it matches the whole command
//! line or any one simple command in it, so `git push*` also blocks
//! `make && git push`. An allow rule (`[approvals].allow` in `grants.toml`
//! or a session approval) admits a command line only when it matches the
//! whole line or every simple command matches some allow rule, so an
//! allowed `npm test` never lets `npm test; rm -rf build` through.
//! Entries starting `plugin_rule:` approve plugin escalations by rule key,
//! never commands.

use super::shell::Script;

/// The prefix of allow entries that approve a plugin escalation.
pub const PLUGIN_RULE_PREFIX: &str = "plugin_rule:";

/// Whether `pattern` matches all of `text`.
#[must_use]
pub fn matches(pattern: &str, text: &str) -> bool {
    let pattern: Vec<Token> = tokens(pattern);
    let text: Vec<char> = text.chars().collect();
    glob(&pattern, &text)
}

/// `text` as a pattern matching exactly itself.
#[must_use]
pub fn literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '*' | '?' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The first deny rule matching the line or one of its commands.
#[must_use]
pub fn denied<'a>(rules: &'a [String], line: &str, script: &Script) -> Option<&'a str> {
    let line = line.trim();
    rules
        .iter()
        .find(|rule| {
            matches(rule, line) || script.segments.iter().any(|s| matches(rule, &s.text()))
        })
        .map(String::as_str)
}

/// The allow rule admitting the line: one matching it whole, or (when
/// every command is covered) the first rule covering a command.
#[must_use]
pub fn allowed<'a>(rules: &'a [String], line: &str, script: &Script) -> Option<&'a str> {
    let commands: Vec<&String> = rules
        .iter()
        .filter(|r| !r.starts_with(PLUGIN_RULE_PREFIX))
        .collect();
    let line = line.trim();
    if let Some(rule) = commands.iter().find(|rule| matches(rule, line)) {
        return Some(rule);
    }
    let mut first = None;
    for segment in &script.segments {
        let text = segment.text();
        let rule = commands.iter().find(|rule| matches(rule, &text))?;
        first.get_or_insert(rule.as_str());
    }
    first
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Token {
    Any,
    One,
    Char(char),
}

fn tokens(pattern: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        out.push(match c {
            '*' => Token::Any,
            '?' => Token::One,
            '\\' => Token::Char(chars.next().unwrap_or('\\')),
            c => Token::Char(c),
        });
    }
    out
}

/// Iterative wildcard matching with single-star backtracking.
fn glob(pattern: &[Token], text: &[char]) -> bool {
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        match pattern.get(p) {
            Some(Token::Any) => {
                star = Some((p, t));
                p += 1;
            }
            Some(Token::One) => {
                p += 1;
                t += 1;
            }
            Some(Token::Char(c)) if *c == text[t] => {
                p += 1;
                t += 1;
            }
            _ => match star {
                Some((sp, st)) => {
                    p = sp + 1;
                    t = st + 1;
                    star = Some((sp, st + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|t| *t == Token::Any)
}

#[cfg(test)]
mod tests {
    use super::{allowed, denied, literal, matches};
    use crate::approval::shell::parse;

    fn rules(list: &[&str]) -> Vec<String> {
        list.iter().map(|&r| r.to_owned()).collect()
    }

    #[test]
    fn globs_match_whole_text() {
        assert!(matches("npm run test*", "npm run test -- --watch"));
        assert!(matches("git push*", "git push"));
        assert!(!matches("git push", "git push --force"));
        assert!(matches("rm ?", "rm x"));
        assert!(matches(&literal("echo *?"), "echo *?"));
        assert!(!matches(&literal("echo *"), "echo hi"));
    }

    #[test]
    fn deny_matches_any_command_and_allow_needs_every_command() {
        let line = "make && git push --force";
        let script = parse(line).unwrap();
        assert_eq!(
            denied(&rules(&["git push*"]), line, &script),
            Some("git push*")
        );
        let line = "npm test; rm -rf build";
        let script = parse(line).unwrap();
        assert_eq!(allowed(&rules(&["npm test"]), line, &script), None);
        assert_eq!(
            allowed(&rules(&["npm test", "rm -rf build"]), line, &script),
            Some("npm test")
        );
        assert_eq!(
            allowed(&rules(&["plugin_rule:*"]), "x", &parse("x").unwrap()),
            None
        );
    }
}
