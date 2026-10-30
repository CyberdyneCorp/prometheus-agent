//! Dangerous-pattern detection: commands that run only after a decision
//! (a human answer, or the typed mode of an unattended context) unless a
//! bypass or allow rule applies first. Each finding names the pattern and
//! the simple command that matched it.

use std::sync::LazyLock;

use regex::Regex;

use super::Finding;
use super::Floors;
use super::paths::resolve;
use super::shell::{Script, Segment};

/// The detector name of these patterns in prompts and `approvals test`.
pub const DETECTOR: &str = "dangerous-pattern";

const INTERPRETERS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "dash",
    "ksh",
    "fish",
    "python",
    "python3",
    "perl",
    "ruby",
    "node",
    "pwsh",
    "powershell",
    "iex",
];

const DOWNLOADERS: &[&str] = &["curl", "wget", "fetch", "iwr", "irm", "invoke-webrequest"];

/// Shell start-up files whose overwrite changes every later shell.
const SHELL_FILES: &[&str] = &[
    ".bashrc",
    ".bash_profile",
    ".profile",
    ".zshrc",
    ".zprofile",
    ".zshenv",
];

static DESTRUCTIVE_SQL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(drop\s+(table|database|schema)|truncate\s+table)\b").expect("valid regex")
});

/// Every dangerous pattern in `script` (`line` is the raw command line).
#[must_use]
pub fn findings(line: &str, script: &Script, floors: &Floors) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut previous: Option<&Segment> = None;
    for segment in &script.segments {
        let reasons = segment_reasons(segment, previous, floors);
        out.extend(reasons.into_iter().map(|reason| Finding {
            detector: DETECTOR.to_owned(),
            reason: reason.to_owned(),
            fragment: segment.text(),
        }));
        previous = Some(segment);
    }
    if let Some(sql) = DESTRUCTIVE_SQL.find(line) {
        out.push(Finding {
            detector: DETECTOR.to_owned(),
            reason: "destructive SQL".to_owned(),
            fragment: sql.as_str().to_owned(),
        });
    }
    out
}

fn segment_reasons(
    segment: &Segment,
    previous: Option<&Segment>,
    floors: &Floors,
) -> Vec<&'static str> {
    let mut reasons = Vec::new();
    if segment.elevated() {
        reasons.push("privilege escalation (sudo)");
    }
    if segment
        .redirects
        .iter()
        .any(|r| r.writes() && sensitive_target(&r.target, floors))
    {
        reasons.push("overwrite of system or shell configuration");
    }
    let Some((program, args)) = segment.program() else {
        return reasons;
    };
    let program = program.to_ascii_lowercase();
    if segment.piped
        && INTERPRETERS.contains(&program.as_str())
        && previous
            .and_then(Segment::program)
            .is_some_and(|(p, _)| DOWNLOADERS.contains(&p.to_ascii_lowercase().as_str()))
    {
        reasons.push("download piped to an interpreter");
    }
    reasons.extend(program_reason(&program, args));
    reasons
}

fn has(args: &[String], flags: &[&str]) -> bool {
    args.iter().any(|a| flags.contains(&a.as_str()))
}

/// A short flag cluster (`-rf`) containing `letter`.
fn short_flag(args: &[String], letter: char) -> bool {
    args.iter()
        .any(|a| a.starts_with('-') && !a.starts_with("--") && a.contains(letter))
}

fn program_reason(program: &str, args: &[String]) -> Option<&'static str> {
    match program {
        "rm" if short_flag(args, 'r') || short_flag(args, 'R') || has(args, &["--recursive"]) => {
            Some("recursive delete")
        }
        "rmdir" | "rd" if has(args, &["/s", "/S"]) => Some("recursive delete"),
        "remove-item" | "ri" if args.iter().any(|a| a.eq_ignore_ascii_case("-recurse")) => {
            Some("recursive delete")
        }
        "find" if has(args, &["-delete"]) || exec_rm(args) => Some("find with delete"),
        "git" => git_reason(args),
        "chmod" if short_flag(args, 'R') || has(args, &["--recursive"]) => {
            Some("recursive permission change")
        }
        "chmod" if has(args, &["777", "666", "a+rwx", "o+w", "a+w"]) => {
            Some("world-writable permissions")
        }
        "chown" | "chgrp" if short_flag(args, 'R') || has(args, &["--recursive"]) => {
            Some("recursive ownership change")
        }
        "dd" => Some("raw copy (dd)"),
        "shred" | "wipefs" | "truncate" => Some("destructive overwrite"),
        "pkill" | "killall" | "taskkill" => Some("kill by name"),
        "su" | "pkexec" => Some("privilege escalation (su)"),
        "crontab" if has(args, &["-r"]) => Some("crontab removal"),
        "docker" | "podman" if docker_data_removal(args) => Some("container data removal"),
        "kubectl" if args.first().is_some_and(|a| a == "delete") => {
            Some("cluster resource deletion")
        }
        "terraform" | "tofu" if args.first().is_some_and(|a| a == "destroy") => {
            Some("infrastructure destroy")
        }
        _ => None,
    }
}

fn exec_rm(args: &[String]) -> bool {
    args.windows(2)
        .any(|w| matches!(w[0].as_str(), "-exec" | "-execdir") && w[1].ends_with("rm"))
}

fn git_reason(args: &[String]) -> Option<&'static str> {
    let verb = args.iter().find(|a| !a.starts_with('-'))?;
    match verb.as_str() {
        "reset" if has(args, &["--hard"]) => Some("git reset --hard (discards changes)"),
        "clean" if short_flag(args, 'f') || has(args, &["--force"]) => {
            Some("git clean (deletes untracked files)")
        }
        "push"
            if short_flag(args, 'f')
                || args
                    .iter()
                    .any(|a| a.starts_with("--force") || a.starts_with('+')) =>
        {
            Some("git force push")
        }
        "branch" if has(args, &["-D"]) => Some("git branch force delete"),
        _ => None,
    }
}

fn docker_data_removal(args: &[String]) -> bool {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    matches!(
        words.as_slice(),
        ["system" | "volume" | "image" | "container", "prune", ..] | ["volume", "rm", ..]
    )
}

/// `/etc/...`, the user's `~/.ssh/` or a shell start-up file.
fn sensitive_target(target: &str, floors: &Floors) -> bool {
    let path = resolve(target, floors);
    let in_home = |rest: &str| {
        floors.user_home.as_deref().is_some_and(|home| {
            path == format!("{home}/{rest}") || path.starts_with(&format!("{home}/{rest}/"))
        })
    };
    path.starts_with("/etc/")
        || path.starts_with("/private/etc/")
        || in_home(".ssh")
        || SHELL_FILES.iter().any(|f| in_home(f))
}
