//! `/queue` (alias `/q`): the session's FIFO of prompts that run after the
//! current turn (spec: cli, Queue management commands). Indexes are
//! 1-based; text typed while busy with `busy.input_mode = "queue"` joins
//! the same queue.

use std::collections::VecDeque;
use std::fmt::Write as _;

use pmt_core::error::{Error, codes};

/// What `/queue` changes or prints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// `/queue <prompt>` or `/queue add <prompt>`.
    Add(String),
    /// `/queue list` (or bare `/queue`).
    List,
    /// `/queue edit <n> <text>`.
    Edit(usize, String),
    /// `/queue rm <n>`.
    Remove(usize),
    /// `/queue move <a> <b>`.
    Move(usize, usize),
    /// `/queue clear`.
    Clear,
}

const USAGE: &str =
    "usage: /queue <prompt> | list | edit <n> <text> | rm <n> | move <a> <b> | clear | add <text>";

/// Parses `/queue` arguments.
///
/// # Errors
///
/// `cli.usage`.
pub fn parse(args: &str) -> Result<Action, Error> {
    let args = args.trim();
    let (verb, rest) = args
        .split_once(char::is_whitespace)
        .map_or((args, ""), |(v, r)| (v, r.trim()));
    let usage = || Error::new(&codes::CLI_USAGE, USAGE);
    let index = |word: &str| word.parse::<usize>().map_err(|_| usage());
    Ok(match verb {
        "" | "list" if rest.is_empty() => Action::List,
        "clear" if rest.is_empty() => Action::Clear,
        "add" if !rest.is_empty() => Action::Add(rest.to_owned()),
        "rm" => Action::Remove(index(rest)?),
        "edit" => {
            let (n, text) = rest.split_once(char::is_whitespace).ok_or_else(usage)?;
            Action::Edit(index(n)?, text.trim().to_owned())
        }
        "move" => {
            let (a, b) = rest.split_once(char::is_whitespace).ok_or_else(usage)?;
            Action::Move(index(a)?, index(b.trim())?)
        }
        _ => Action::Add(args.to_owned()),
    })
}

/// Applies `action` to `queue`; returns the reply.
///
/// # Errors
///
/// `cli.usage` with `no queued item <n>` for an index out of range.
pub fn apply(queue: &mut VecDeque<String>, action: Action) -> Result<String, Error> {
    let missing = |n: usize| Error::new(&codes::CLI_USAGE, format!("no queued item {n}"));
    let slot = |n: usize, len: usize| (1..=len).contains(&n).then(|| n - 1);
    Ok(match action {
        Action::Add(text) => {
            queue.push_back(text);
            format!("queued ({} in the queue)", queue.len())
        }
        Action::List => list(queue),
        Action::Edit(n, text) => {
            let at = slot(n, queue.len()).ok_or_else(|| missing(n))?;
            queue[at] = text;
            list(queue)
        }
        Action::Remove(n) => {
            let at = slot(n, queue.len()).ok_or_else(|| missing(n))?;
            queue.remove(at);
            list(queue)
        }
        Action::Move(a, b) => {
            let from = slot(a, queue.len()).ok_or_else(|| missing(a))?;
            let to = slot(b, queue.len()).ok_or_else(|| missing(b))?;
            if let Some(item) = queue.remove(from) {
                queue.insert(to, item);
            }
            list(queue)
        }
        Action::Clear => {
            let n = queue.len();
            queue.clear();
            format!("dropped {n} queued item(s)")
        }
    })
}

fn list(queue: &VecDeque<String>) -> String {
    if queue.is_empty() {
        return "the queue is empty".to_owned();
    }
    let mut text = String::from("queued:");
    for (n, item) in queue.iter().enumerate() {
        let first = item.lines().next().unwrap_or_default();
        let _ = write!(text, "\n  {}. {first}", n + 1);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue(items: &[&str]) -> VecDeque<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    /// Scenario: Reorder queue.
    #[test]
    fn move_reorders_and_list_shows_the_new_order() {
        let mut q = queue(&["a", "b", "c"]);
        apply(&mut q, parse("move 3 1").unwrap()).unwrap();
        assert_eq!(q, queue(&["c", "a", "b"]));
        assert_eq!(
            apply(&mut q, parse("list").unwrap()).unwrap(),
            "queued:\n  1. c\n  2. a\n  3. b"
        );
    }

    #[test]
    fn edit_remove_clear_and_out_of_range() {
        let mut q = queue(&["a", "b"]);
        apply(&mut q, parse("edit 2 B2").unwrap()).unwrap();
        apply(&mut q, parse("rm 1").unwrap()).unwrap();
        assert_eq!(q, queue(&["B2"]));
        let err = apply(&mut q, parse("rm 5").unwrap()).unwrap_err();
        assert_eq!(err.message(), "no queued item 5");
        assert_eq!(
            parse("fix the tests").unwrap(),
            Action::Add("fix the tests".into())
        );
        assert_eq!(parse("add list").unwrap(), Action::Add("list".into()));
        apply(&mut q, Action::Clear).unwrap();
        assert!(q.is_empty());
        assert!(parse("move 1").is_err());
    }
}
