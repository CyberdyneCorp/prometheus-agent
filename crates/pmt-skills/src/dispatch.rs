//! A chat message that starts with skill or bundle commands (spec: skills,
//! Stacking skills in one message; Skill bundles; Skill slash commands).
//!
//! Resolution order: a built-in command (the caller's) always wins, then
//! a bundle slug, then skill slugs. Several leading skill slugs stack into
//! one user message, at most [`MAX_STACKED`]; parsing stops at the first
//! token that is not an enabled skill slug, so an argument that begins
//! with `/` (a file path) stays part of the instruction.

use crate::bundles;
use crate::session::SkillsSession;
use crate::slash::{self, Message, SKILL_COMMAND};

/// Most skills one message may stack.
pub const MAX_STACKED: usize = 5;
/// The notice when a sixth skill token is left as instruction text.
pub const STACK_NOTICE: &str = "at most 5 skills can be stacked; use a skill bundle for more";

/// A message rewritten by skill commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expanded {
    /// The user message to append.
    pub message: String,
    /// Notices for the user (not sent to the model).
    pub notices: Vec<String>,
}

/// The whitespace-separated tokens of `text` with their byte offsets.
fn tokens(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices() {
        match (c.is_whitespace(), start) {
            (true, Some(s)) => {
                out.push((s, &text[s..i]));
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push((s, &text[s..]));
    }
    out
}

/// What the leading commands of a message are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed<'a> {
    /// Not a skill or bundle command (a built-in, unknown, or plain text).
    None,
    /// `/skill <name> rest`.
    Named(&'a str, &'a str),
    /// A bundle and the rest.
    Bundle(String, &'a str),
    /// Stacked skill names, the rest, and whether the cap cut the stack.
    Stack(Vec<String>, &'a str, bool),
}

/// Parses the leading commands of `text`. `is_core` says whether a slug is
/// a built-in command or alias; `bundle` whether it is a bundle;
/// `skill` maps an enabled skill slug to its skill name.
pub fn parse<'a>(
    text: &'a str,
    is_core: &dyn Fn(&str) -> bool,
    bundle: &dyn Fn(&str) -> bool,
    skill: &dyn Fn(&str) -> Option<String>,
) -> Parsed<'a> {
    let tokens = tokens(text);
    let Some(first) = tokens
        .first()
        .and_then(|(_, t)| t.strip_prefix('/'))
        .filter(|s| !s.is_empty())
    else {
        return Parsed::None;
    };
    let rest_from = |index: usize| tokens.get(index).map_or("", |(at, _)| &text[*at..]);
    if is_core(first) {
        return Parsed::None;
    }
    if first == SKILL_COMMAND {
        return match tokens.get(1) {
            Some((_, name)) => Parsed::Named(name, rest_from(2)),
            None => Parsed::None,
        };
    }
    if bundle(first) {
        return Parsed::Bundle(first.to_owned(), rest_from(1));
    }
    let mut names = Vec::new();
    for (index, (_, token)) in tokens.iter().enumerate() {
        let Some(name) = token.strip_prefix('/').and_then(skill) else {
            return stack(names, rest_from(index), false);
        };
        if names.len() == MAX_STACKED {
            return stack(names, rest_from(index), true);
        }
        names.push(name);
    }
    stack(names, "", false)
}

fn stack(names: Vec<String>, rest: &str, capped: bool) -> Parsed<'_> {
    if names.is_empty() {
        Parsed::None
    } else {
        Parsed::Stack(names, rest, capped)
    }
}

/// Expands a message that starts with skill or bundle commands; `None`
/// when it does not (the caller handles built-ins and plain text). `core`
/// holds the built-in command names and aliases.
///
/// # Errors
///
/// The message to show the user (`/skill` of an unknown or hidden skill).
pub async fn expand(
    session: &SkillsSession,
    text: &str,
    core: &[&str],
) -> Option<Result<Expanded, String>> {
    let table = slash::table(session, core);
    let bundles = bundles::load(session.scope().dir());
    let core: Vec<&str> = core.iter().map(|c| c.trim_start_matches('/')).collect();
    let parsed = parse(
        text,
        &|slug| core.contains(&slug),
        &|slug| bundles.get(slug).is_some(),
        &|slug| table.commands.get(slug).cloned(),
    );
    let expanded = match parsed {
        Parsed::None => return None,
        Parsed::Named(name, rest) => slash::invoke(session, name, rest).await.map(plain),
        Parsed::Bundle(slug, rest) => {
            let bundle = bundles.get(&slug)?;
            Ok(plain(bundles::invoke(session, bundle, rest).await))
        }
        Parsed::Stack(names, rest, capped) => Ok(stacked(session, &names, rest, capped).await),
    };
    Some(expanded)
}

fn plain(message: String) -> Expanded {
    Expanded {
        message,
        notices: Vec::new(),
    }
}

async fn stacked(session: &SkillsSession, names: &[String], rest: &str, capped: bool) -> Expanded {
    let (catalog, config) = session.catalog();
    let mut message = Message {
        instruction: rest.to_owned(),
        ..Message::default()
    };
    for name in names {
        match slash::resolve(session, &catalog, &config, name) {
            Ok(skill) => message
                .blocks
                .push(slash::block(session, &skill, &config).await),
            Err(_) => message.skipped.push(name.clone()),
        }
    }
    Expanded {
        message: message.render(),
        notices: if capped {
            vec![STACK_NOTICE.to_owned()]
        } else {
            Vec::new()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str) -> Parsed<'_> {
        let skills = [
            "pdf",
            "github-pr-workflow",
            "test-driven-development",
            "a",
            "b",
            "c",
            "d",
            "e",
            "f",
        ];
        parse(text, &|s| s == "help", &|s| s == "backend-dev", &|s| {
            skills.contains(&s).then(|| s.to_owned())
        })
    }

    #[test]
    fn two_skills_and_an_instruction() {
        assert_eq!(
            run("/github-pr-workflow /test-driven-development fix issue 123"),
            Parsed::Stack(
                vec![
                    "github-pr-workflow".into(),
                    "test-driven-development".into()
                ],
                "fix issue 123",
                false
            )
        );
    }

    #[test]
    fn path_argument_is_not_swallowed() {
        assert_eq!(
            run("/pdf /tmp/scan.pdf extract tables"),
            Parsed::Stack(vec!["pdf".into()], "/tmp/scan.pdf extract tables", false)
        );
    }

    #[test]
    fn sixth_skill_is_instruction_text() {
        let Parsed::Stack(names, rest, capped) = run("/a /b /c /d /e /f go") else {
            panic!("not a stack");
        };
        assert_eq!(names.len(), 5);
        assert_eq!(rest, "/f go");
        assert!(capped);
    }

    #[test]
    fn built_ins_win_then_bundles() {
        assert_eq!(run("/help pdf"), Parsed::None);
        assert_eq!(
            run("/backend-dev refactor auth"),
            Parsed::Bundle("backend-dev".into(), "refactor auth")
        );
        assert_eq!(run("/skill pdf x y"), Parsed::Named("pdf", "x y"));
        assert_eq!(run("plain /pdf"), Parsed::None);
        assert_eq!(run("/unknown x"), Parsed::None);
    }
}
