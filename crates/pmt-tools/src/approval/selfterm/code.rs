//! Code an interpreter runs (`execute_code` bodies, `python3 -c '...'`):
//! its string literals are often command lines (`os.system("pkill -f
//! prometheus")`, `subprocess.run(["pkill", "-f", "prometheus"])`), and
//! its `kill` calls name pids directly (`os.kill(<daemon pid>, 9)`,
//! `os.kill(os.getppid(), 9)`) or ones read from the daemon's `run/`
//! records.
//!
//! This is a reading of text, not of any one language: each literal, and
//! each run of literals forming a list (an argv), is checked as a shell
//! command line, and any `kill(`/`killpg(` call is checked for a daemon
//! pid or the parent process. Over-reading only blocks.

use std::sync::LazyLock;

use regex::Regex;

use super::super::shell::parse;
use super::{Daemon, Guarded, structural};

/// How deep literals inside literals are followed.
const MAX_DEPTH: usize = 4;

/// `os.kill(`, `os.killpg(`, `process.kill(`, `Process.kill(`, `syscall.Kill(`.
static KILL_CALL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bkill(?:pg)?\s*\(").expect("kill call pattern"));

/// Why the code in `text` would stop the daemon, if it would.
pub(super) fn check(text: &str, guarded: Guarded<'_>) -> Option<String> {
    scan(text, guarded, 0)
}

fn scan(text: &str, guarded: Guarded<'_>, depth: usize) -> Option<String> {
    if let Some(reason) = kill_call(text, guarded).or_else(|| record_kill(text, guarded.daemon)) {
        return Some(reason);
    }
    if depth >= MAX_DEPTH {
        return None;
    }
    command_lines(text).iter().find_map(|line| {
        parse(line)
            .ok()
            .and_then(|script| structural(&script, guarded))
            .or_else(|| scan(line, guarded, depth + 1))
    })
}

/// A `kill(<pid>, ...)` call aimed at a daemon pid or the parent process.
fn kill_call(text: &str, guarded: Guarded<'_>) -> Option<String> {
    KILL_CALL.find_iter(text).find_map(|call| {
        let args = balanced(&text[call.end()..]);
        let what = if args.to_ascii_lowercase().contains("ppid") {
            "kills the parent process, the daemon hosting this session".to_owned()
        } else {
            let pid = args
                .split(',')
                .filter_map(|arg| {
                    let arg = arg.trim().trim_matches(['"', '\'']);
                    arg.trim_start_matches('-').parse::<u32>().ok()
                })
                .filter(|pid| !guarded.own.contains(pid))
                .find(|pid| guarded.daemon.pids.contains(pid))?;
            format!("kills pid {pid}, the daemon hosting this session")
        };
        Some(format!("`{}{args})` {what}", call.as_str()))
    })
}

/// Code that reads the daemon's `run/` record and calls `kill(`: the pid
/// it kills is the daemon's.
fn record_kill(text: &str, daemon: &Daemon) -> Option<String> {
    (daemon.in_run_dir(text) && KILL_CALL.is_match(text)).then(|| {
        "the code reads the pid record of the daemon hosting this session and kills a pid".into()
    })
}

/// The text up to the parenthesis closing the one just opened.
fn balanced(rest: &str) -> &str {
    let mut depth = 1;
    for (at, c) in rest.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return &rest[..at];
                }
            }
            _ => {}
        }
    }
    rest
}

/// A string literal and its byte span in the text.
struct Literal {
    start: usize,
    end: usize,
    text: String,
}

/// Every literal on its own, plus each run of two or more literals
/// separated only by commas, brackets and blanks (an argv list) joined as
/// one shell-quoted command line.
fn command_lines(text: &str) -> Vec<String> {
    let found = literals(text);
    let mut lines: Vec<String> = found.iter().map(|l| l.text.clone()).collect();
    let mut run: Vec<&Literal> = Vec::new();
    for literal in &found {
        let continues = run.last().is_some_and(|prev| {
            text[prev.end..literal.start]
                .chars()
                .all(|c| c.is_whitespace() || matches!(c, ',' | '[' | ']'))
        });
        if !continues {
            flush(&mut run, &mut lines);
        }
        run.push(literal);
    }
    flush(&mut run, &mut lines);
    lines
}

fn flush(run: &mut Vec<&Literal>, lines: &mut Vec<String>) {
    if run.len() > 1 {
        let words: Vec<String> = run
            .iter()
            .map(|l| format!("'{}'", l.text.replace('\'', r"'\''")))
            .collect();
        lines.push(words.join(" "));
    }
    run.clear();
}

/// The quoted strings of `text` (single, double or tripled quotes,
/// backslash escapes), skipping `#` comments. An unterminated quote is
/// taken as a lone character (an apostrophe in prose).
fn literals(text: &str) -> Vec<Literal> {
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(c) = text[at..].chars().next() {
        let comment = c == '#' && (at == 0 || text[..at].ends_with(char::is_whitespace));
        at = match c {
            '\\' => at + 1 + text[at + 1..].chars().next().map_or(0, char::len_utf8),
            _ if comment => text[at..].find('\n').map_or(text.len(), |n| at + n),
            '\'' | '"' => match quoted(text, at) {
                Some(literal) => {
                    let end = literal.end;
                    found.push(literal);
                    end
                }
                None => at + 1,
            },
            _ => at + c.len_utf8(),
        };
    }
    found
}

fn quoted(text: &str, start: usize) -> Option<Literal> {
    let quote = &text[start..=start];
    let tripled = quote.repeat(3);
    let delimiter = if text[start..].starts_with(&tripled) {
        tripled
    } else {
        quote.to_owned()
    };
    let open = start + delimiter.len();
    let mut body = String::new();
    let mut chars = text[open..].char_indices();
    while let Some((offset, c)) = chars.next() {
        let at = open + offset;
        if text[at..].starts_with(&delimiter) {
            return Some(Literal {
                start,
                end: at + delimiter.len(),
                text: body,
            });
        }
        if c == '\\' {
            let Some((_, escaped)) = chars.next() else {
                break;
            };
            if !matches!(escaped, '\'' | '"' | '\\') {
                body.push('\\');
            }
            body.push(escaped);
        } else {
            body.push(c);
        }
    }
    None
}
