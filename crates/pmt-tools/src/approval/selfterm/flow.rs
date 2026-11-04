//! Daemon pids that reach `kill` indirectly: a substitution
//! (`kill $(pgrep prometheus)`, `kill $(cat <run>/gateway.json)`), a
//! pipeline into `xargs kill` (`pgrep prometheus | xargs kill`,
//! `xargs kill < <run>/...`), or a variable assigned from one of those
//! (`p=$(pgrep prometheus); kill $p`, `pgrep prometheus | while read p; do
//! kill $p; done`, `for p in $(pidof prometheus); do kill $p; done`).
//!
//! A source of daemon pids is a lookup by a name or pattern that reaches
//! the daemon (see [`super::Matching`]) or anything that reads the
//! daemon's `run/` directory. Variables are followed by name across the
//! whole script regardless of order: over-reading only blocks.

use super::super::shell::{Script, Segment, basename, parse};
use super::{Daemon, Guarded, Matching, kill_by_name};

/// What the reason says about a read of `run/`.
const RUN_READ: &str = "the pid record of the daemon hosting this session";

/// Builtins whose arguments are assignments.
const DECLARES: &[&str] = &["export", "local", "declare", "typeset", "readonly"];

/// Which segments read daemon pids on stdin and which variables hold them.
pub(super) struct Flow {
    /// Per segment: why its stdin carries daemon pids.
    fed: Vec<Option<String>>,
    /// Variables holding daemon pids, with why.
    tainted: Vec<(String, String)>,
    /// Other assigned values (a literal pid in a variable).
    values: Vec<(String, String)>,
}

impl Flow {
    pub(super) fn of(script: &Script, daemon: &Daemon) -> Self {
        let mut flow = Self {
            fed: fed(script, daemon),
            tainted: Vec::new(),
            values: Vec::new(),
        };
        for (at, segment) in script.segments.iter().enumerate() {
            flow.assign(at, segment, daemon);
        }
        flow
    }

    /// Why the `kill` in `segment` (the `at`-th) reaches the daemon
    /// through stdin or a variable.
    pub(super) fn reason(
        &self,
        at: usize,
        segment: &Segment,
        guarded: Guarded<'_>,
    ) -> Option<String> {
        let (program, args) = segment.program()?;
        if !matches!(program.to_ascii_lowercase().as_str(), "kill" | "taskkill") {
            return None;
        }
        let xargs = segment.words.iter().any(|w| basename(w) == "xargs");
        if let Some(why) = self.fed.get(at).cloned().flatten().filter(|_| xargs) {
            return Some(format!("`xargs {program}` reads pids from {why}"));
        }
        args.iter()
            .flat_map(|arg| variables(arg))
            .find_map(|name| self.variable_reason(name, guarded))
    }

    fn variable_reason(&self, name: &str, guarded: Guarded<'_>) -> Option<String> {
        if let Some((_, why)) = self.tainted.iter().find(|(n, _)| n == name) {
            return Some(format!("`${name}` holds pids from {why}"));
        }
        self.values
            .iter()
            .filter(|(n, _)| n == name)
            .filter_map(|(_, value)| value.trim_start_matches('-').parse::<u32>().ok())
            .filter(|pid| !guarded.own.contains(pid))
            .find(|pid| guarded.daemon.pids.contains(pid))
            .map(|pid| format!("`${name}` is pid {pid}, the daemon hosting this session"))
    }

    /// Records the variables `segment` assigns.
    fn assign(&mut self, at: usize, segment: &Segment, daemon: &Daemon) {
        for (name, value) in assignments(segment) {
            self.set(name, value, daemon);
        }
        let Some((program, args)) = segment.program() else {
            return;
        };
        match program {
            "read" | "mapfile" | "readarray" => {
                if let Some(why) = self.fed.get(at).cloned().flatten() {
                    for name in args.iter().filter(|a| !a.starts_with('-')) {
                        self.tainted.push((name.clone(), why.clone()));
                    }
                }
            }
            "for" => {
                if let [name, keyword, words @ ..] = args
                    && keyword == "in"
                {
                    for word in words {
                        self.set(name, word, daemon);
                    }
                }
            }
            _ => {}
        }
    }

    fn set(&mut self, name: &str, value: &str, daemon: &Daemon) {
        let why = value_reason(value, daemon).or_else(|| {
            variables(value).find_map(|other| {
                self.tainted
                    .iter()
                    .find(|(n, _)| n == other)
                    .map(|(_, why)| why.clone())
            })
        });
        match why {
            Some(why) => self.tainted.push((name.to_owned(), why)),
            None => self.values.push((name.to_owned(), value.to_owned())),
        }
    }
}

/// `kill $(pgrep <pattern>)` / `` kill `pidof prometheus` `` / `kill $(cat
/// <run>/gateway.json)`: the pids come from the substitution's source.
pub(super) fn substituted(args: &[String], daemon: &Daemon) -> Option<String> {
    args.iter().find_map(|arg| value_reason(arg, daemon))
}

/// Why a word's value holds daemon pids: it names `run/` or substitutes a
/// command that is a source of them.
fn value_reason(value: &str, daemon: &Daemon) -> Option<String> {
    if daemon.in_run_dir(value) {
        return Some(RUN_READ.to_owned());
    }
    let body = substitution_body(value)?;
    let script = parse(body).ok()?;
    script.segments.iter().find_map(|s| source(s, daemon))
}

/// Why `segment`'s output is daemon pids: a lookup by a name reaching the
/// daemon, or a read of `run/`.
fn source(segment: &Segment, daemon: &Daemon) -> Option<String> {
    let reads_run = segment.words.iter().any(|w| daemon.in_run_dir(w))
        || segment
            .redirects
            .iter()
            .any(|r| daemon.in_run_dir(&r.target));
    if reads_run {
        return Some(RUN_READ.to_owned());
    }
    let (program, args) = segment.program()?;
    let program = program.to_ascii_lowercase();
    Matching::of(&program, args)?;
    kill_by_name(&program, args, daemon).map(|reason| format!("a lookup ({reason})"))
}

/// Per segment, why its stdin carries daemon pids: a `<` redirection from
/// `run/`, or a pipe from a source (or from a segment fed by one).
fn fed(script: &Script, daemon: &Daemon) -> Vec<Option<String>> {
    let mut upstream: Option<String> = None;
    script
        .segments
        .iter()
        .map(|segment| {
            let stdin = segment
                .redirects
                .iter()
                .any(|r| r.op.starts_with('<') && daemon.in_run_dir(&r.target))
                .then(|| RUN_READ.to_owned());
            let here = stdin.or_else(|| upstream.clone().filter(|_| segment.piped));
            upstream = here.clone().or_else(|| source(segment, daemon));
            here
        })
        .collect()
}

/// `NAME=value` words before the program and the arguments of `export`
/// and its siblings.
fn assignments(segment: &Segment) -> impl Iterator<Item = (&str, &str)> {
    let declared = segment
        .program()
        .filter(|(program, _)| DECLARES.contains(program))
        .map(|(_, args)| args)
        .unwrap_or_default();
    segment
        .words
        .iter()
        .take_while(|w| assignment(w).is_some())
        .chain(declared)
        .filter_map(|w| assignment(w))
}

fn assignment(word: &str) -> Option<(&str, &str)> {
    let (name, value) = word.split_once('=')?;
    let valid = !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    valid.then_some((name, value))
}

/// The variable names `word` expands (`$p`, `${p}`, `${p[@]}`).
fn variables(word: &str) -> impl Iterator<Item = &str> {
    word.match_indices('$').filter_map(move |(at, _)| {
        let rest = &word[at + 1..];
        let rest = rest.strip_prefix('{').unwrap_or(rest);
        let end = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        let name = &rest[..end];
        (!name.is_empty() && !name.starts_with(|c: char| c.is_ascii_digit())).then_some(name)
    })
}

fn substitution_body(word: &str) -> Option<&str> {
    let start = word
        .find("$(")
        .map(|at| at + 2)
        .or_else(|| word.find('`').map(|at| at + 1))?;
    let rest = &word[start..];
    let end = rest.rfind([')', '`']).unwrap_or(rest.len());
    Some(&rest[..end])
}
