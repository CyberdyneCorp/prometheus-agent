//! Every command quoted in a user guide (`docs/guide/*.md`) runs as
//! written (spec: configuration-and-profiles and the other guides' docs-
//! snippet tasks).
//!
//! In each ```` ```console ```` block, a line starting with `$ ` is a
//! command and the lines up to the next command are expected output: each
//! must equal (whitespace collapsed) some line of the command's stdout or
//! stderr, where `<...>`, `<home>` or any other `<word>` matches any text.
//! A command exits 0 unless it ends with `# exits N`. A guide's commands
//! run in order against one fresh home given as `PROMETHEUS_HOME`;
//! `echo -n "$VAR" | prometheus ...` pipes the value of a fixture variable.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

/// Variables a guide may pipe into a command.
const VARIABLES: &[(&str, &str)] = &[("TOKEN", "123456:AAH-docs-token-value")];

#[derive(Debug)]
struct Snippet {
    line: usize,
    command: String,
    exit: i32,
    expected: Vec<String>,
}

fn guides() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/guide");
    let mut guides: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "md"))
        .collect();
    guides.sort();
    guides
}

fn snippets(text: &str) -> Vec<Snippet> {
    let mut found: Vec<Snippet> = Vec::new();
    let mut in_console = false;
    for (index, line) in text.lines().enumerate() {
        if line.starts_with("```") {
            in_console = line.trim() == "```console";
            continue;
        }
        if !in_console {
            continue;
        }
        if let Some(command) = line.strip_prefix("$ ") {
            found.push(parse_command(index + 1, command));
        } else if let Some(last) = found.last_mut() {
            last.expected.push(normalize(line));
        }
    }
    found
}

fn parse_command(line: usize, text: &str) -> Snippet {
    let (command, exit) = match text.split_once("# exits ") {
        Some((command, code)) => (command, code.trim().parse().unwrap()),
        None => (text, 0),
    };
    Snippet {
        line,
        command: command.trim().to_owned(),
        exit,
        expected: Vec::new(),
    }
}

fn normalize(line: &str) -> String {
    line.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Shell-like words: whitespace-separated, with '...' and "..." quoting.
fn words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current: Option<String> = None;
    let mut quote = None;
    for c in text.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (None, '\'' | '"') => {
                quote = Some(c);
                current.get_or_insert_with(String::new);
            }
            (None, c) if c.is_whitespace() => words.extend(current.take()),
            (_, c) => current.get_or_insert_with(String::new).push(c),
        }
    }
    words.extend(current);
    words
}

fn variable(word: &str) -> String {
    let name = word.strip_prefix('$').expect("piped words are $VARIABLES");
    VARIABLES
        .iter()
        .find(|(n, _)| *n == name)
        .unwrap_or_else(|| panic!("unknown fixture variable ${name}"))
        .1
        .to_owned()
}

/// Splits `echo -n "$VAR" | prometheus ...` into stdin and the command.
fn stdin_and_args(command: &str) -> (Option<String>, Vec<String>) {
    let (stdin, command) = match command.split_once(" | ") {
        Some((producer, command)) => {
            let producer = words(producer);
            assert_eq!(
                producer[..2],
                ["echo", "-n"],
                "only `echo -n` pipes are supported"
            );
            (Some(variable(&producer[2])), command)
        }
        None => (None, command),
    };
    let mut args = words(command);
    assert_eq!(
        args.remove(0),
        "prometheus",
        "guide commands run prometheus"
    );
    (stdin, args)
}

/// `expected` with each `<word>` matching any text.
fn matches(expected: &str, actual: &str) -> bool {
    let mut parts = Vec::new();
    let mut rest = expected;
    while let Some(start) = rest.find('<') {
        let Some(len) = rest[start..].find('>') else {
            break;
        };
        parts.push(&rest[..start]);
        rest = &rest[start + len + 1..];
    }
    parts.push(rest);
    let (first, tail) = parts.split_first().unwrap();
    let Some(mut remaining) = actual.strip_prefix(first) else {
        return false;
    };
    for (i, part) in tail.iter().enumerate() {
        if i == tail.len() - 1 {
            return remaining.ends_with(part);
        }
        match remaining.find(part) {
            Some(at) => remaining = &remaining[at + part.len()..],
            None => return false,
        }
    }
    remaining.is_empty()
}

fn run(home: &Path, snippet: &Snippet, guide: &Path) {
    let (stdin, args) = stdin_and_args(&snippet.command);
    let mut cmd = Command::new(common::BIN);
    for (var, _) in std::env::vars_os() {
        let name = var.to_string_lossy().into_owned();
        if name.starts_with("PMT_") || name.starts_with("PROMETHEUS_") {
            cmd.env_remove(&var);
        }
    }
    cmd.env("PROMETHEUS_HOME", home).args(&args);
    let out = common::run_cmd_with_stdin(&mut cmd, stdin.as_deref().unwrap_or(""));
    let output = format!("{}{}", common::stdout(&out), common::stderr(&out));
    let place = format!(
        "{}:{}: `{}`",
        guide.display(),
        snippet.line,
        snippet.command
    );
    assert_eq!(out.status.code(), Some(snippet.exit), "{place}\n{output}");
    for expected in &snippet.expected {
        assert!(
            output
                .lines()
                .any(|line| matches(expected, &normalize(line))),
            "{place}: no output line matches {expected:?}\n{output}"
        );
    }
}

#[test]
fn every_guide_command_runs_as_documented() {
    let guides = guides();
    assert!(!guides.is_empty());
    for guide in guides {
        let snippets = snippets(&std::fs::read_to_string(&guide).unwrap());
        assert!(
            !snippets.is_empty(),
            "{} quotes no commands",
            guide.display()
        );
        let home = common::temp_home();
        for snippet in &snippets {
            run(home.path(), snippet, &guide);
        }
    }
}

#[test]
fn placeholder_matching() {
    assert!(matches(
        "source: config.toml:<...>",
        "source: config.toml:6 (x)"
    ));
    assert!(matches(
        "<home>/profiles/default/config.toml",
        "/t/h/profiles/default/config.toml"
    ));
    assert!(matches(
        "snap_<...> manual <...> known-good",
        "snap_01 manual 2026 known-good"
    ));
    assert!(!matches("snap_<...> manual", "snap_01 pre-update"));
    assert!(matches("200", "200"));
    assert!(!matches("200", "2000"));
    assert_eq!(words(r#"echo -n "$TOKEN""#), ["echo", "-n", "$TOKEN"]);
}
