//! The self-termination guard (spec: tool-system, Self-termination
//! guard): inside a session hosted by the gateway daemon, commands that
//! would stop or restart that daemon are blocked under every approval
//! mode, yolo included, and every backend. The model asking the daemon to
//! kill itself would end every session it hosts, the one asking included.
//!
//! What counts: `prometheus daemon|gateway stop|restart`, `systemctl stop|
//! restart|kill` of the Prometheus unit, `launchctl bootout|kill` of its
//! label, `schtasks /End` of its task, and `kill`, `pkill`, `killall` or
//! `taskkill` aimed at the Prometheus executable by name or at the
//! daemon's pid. Kills of the session's own background processes and of
//! other explicit pids are left to the rest of the gate.
//!
//! Name patterns are read the way the tools read them: `pkill` and
//! `pgrep` take a regular expression over process names or, with `-f`,
//! over whole command lines; `killall -r`/`-m` takes a regular expression
//! too. Over-reading is harmless (a false match only blocks), so a
//! pattern that mentions an executable name is blocked even when no
//! command line is known to match it.
//!
//! Pids also reach `kill` indirectly: through a substitution, a variable
//! or `xargs` fed by a lookup by name (`pgrep`, `pidof`, `grep` over `ps`)
//! or by a read of the daemon's `run/` records ([`flow`]), and from code
//! an interpreter runs, whose string literals are command lines and whose
//! `os.kill`-style calls name pids ([`code`]). The guard looks at the raw
//! text as well as the parsed shell, so `python3 -c '...'` and
//! `execute_code` bodies are read the same way.

mod code;
mod flow;

use regex::RegexBuilder;

use super::shell::{Script, Segment, basename};

/// What identifies the hosting daemon.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Daemon {
    /// The daemon's pids: this process and the one its `run/` record names.
    pub pids: Vec<u32>,
    /// Executable base names without extension, lower case (`prometheus`).
    pub executables: Vec<String>,
    /// Full command lines the daemon runs under (what `pkill -f` matches),
    /// e.g. `/usr/local/bin/prometheus daemon run`.
    pub command_lines: Vec<String>,
    /// Service unit, launchd label and scheduled task names.
    pub services: Vec<String>,
    /// Spellings of the daemon's `run/` directory, which holds its pid
    /// record: the absolute path and home-relative tails such as
    /// `.prometheus/run` (matched as substrings, so `~/`, `$HOME/` and
    /// absolute spellings all hit).
    pub run_dirs: Vec<String>,
}

impl Daemon {
    /// Whether `word` names something inside the daemon's `run/` directory.
    #[must_use]
    pub fn in_run_dir(&self, word: &str) -> bool {
        let word = word.replace('\\', "/");
        self.run_dirs.iter().any(|dir| {
            let dir = dir.replace('\\', "/");
            let dir = dir.trim_end_matches('/');
            !dir.is_empty() && (word.contains(&format!("{dir}/")) || word.ends_with(dir))
        })
    }
}

/// The daemon and the session's own background process ids.
#[derive(Debug, Clone, Copy)]
pub struct Guarded<'a> {
    /// The hosting daemon.
    pub daemon: &'a Daemon,
    /// Pids of background processes the session started.
    pub own: &'a [u32],
}

/// Why `line` (read as `script`) would stop the daemon, if it would.
#[must_use]
pub fn check(line: &str, script: &Script, guarded: Guarded<'_>) -> Option<String> {
    structural(script, guarded).or_else(|| code::check(line, guarded))
}

/// What the parsed shell commands do, indirections included.
fn structural(script: &Script, guarded: Guarded<'_>) -> Option<String> {
    let flow = flow::Flow::of(script, guarded.daemon);
    script
        .segments
        .iter()
        .enumerate()
        .find_map(|(at, segment)| {
            segment_reason(segment, guarded).or_else(|| flow.reason(at, segment, guarded))
        })
}

fn segment_reason(segment: &Segment, guarded: Guarded<'_>) -> Option<String> {
    let (program, args) = segment.program()?;
    let program = program.to_ascii_lowercase();
    let daemon = guarded.daemon;
    if matches!(program.as_str(), "kill" | "pkill" | "killall" | "taskkill")
        && let Some(arg) = args.iter().find(|a| daemon.in_run_dir(a))
    {
        return Some(format!(
            "`{program} {arg}` reads the pid record of the daemon hosting this session"
        ));
    }
    match program.as_str() {
        "systemctl" => service_stop(args, &["stop", "restart", "kill"], daemon),
        "launchctl" => service_stop(args, &["bootout", "kill"], daemon),
        "schtasks" => scheduled_task_end(args, daemon),
        "kill" => kill_pids(args, guarded).or_else(|| flow::substituted(args, daemon)),
        "pkill" | "killall" => kill_by_name(&program, args, daemon),
        "taskkill" => taskkill(args, guarded),
        name if daemon.executables.iter().any(|e| e == name) => own_cli(&program, args),
        _ => None,
    }
}

/// `prometheus [flags] daemon|gateway stop|restart`.
fn own_cli(program: &str, args: &[String]) -> Option<String> {
    args.windows(2).find_map(|pair| {
        let (noun, verb) = (pair[0].as_str(), pair[1].as_str());
        (matches!(noun, "daemon" | "gateway") && matches!(verb, "stop" | "restart"))
            .then(|| format!("`{program} {noun} {verb}` stops the daemon hosting this session"))
    })
}

/// `systemctl stop <unit>` / `launchctl bootout <target>`: a `verb`
/// followed by an argument naming a Prometheus service.
fn service_stop(args: &[String], verbs: &[&str], daemon: &Daemon) -> Option<String> {
    let verb = args.iter().position(|a| verbs.contains(&a.as_str()))?;
    let target = args[verb + 1..]
        .iter()
        .find(|a| !a.starts_with('-') && names_service(a, daemon))?;
    Some(format!(
        "`{} {target}` stops the daemon hosting this session",
        args[verb]
    ))
}

fn names_service(arg: &str, daemon: &Daemon) -> bool {
    let arg = arg.to_ascii_lowercase();
    let unit = arg.strip_suffix(".service").unwrap_or(&arg);
    daemon.services.iter().any(|service| {
        let service = service.to_ascii_lowercase();
        let bare = service.strip_suffix(".service").unwrap_or(&service);
        // launchctl targets look like `gui/501/ai.prometheus.daemon`.
        unit == bare || unit.ends_with(&format!("/{bare}"))
    })
}

/// `schtasks /End /TN "<task>"`.
fn scheduled_task_end(args: &[String], daemon: &Daemon) -> Option<String> {
    let ends = args.iter().any(|a| a.eq_ignore_ascii_case("/end"));
    let task = option_value(args, &["/tn"])?;
    let named = daemon
        .services
        .iter()
        .any(|s| s.eq_ignore_ascii_case(task.trim_start_matches('\\')));
    (ends && named)
        .then(|| format!("`schtasks /End /TN {task}` stops the daemon hosting this session"))
}

/// `kill [-SIG] <pid>...`: the daemon's pid (or its process group).
fn kill_pids(args: &[String], guarded: Guarded<'_>) -> Option<String> {
    args.iter()
        .filter_map(|a| a.strip_prefix('-').unwrap_or(a).parse::<u32>().ok())
        .filter(|pid| !guarded.own.contains(pid))
        .find(|pid| guarded.daemon.pids.contains(pid))
        .map(|pid| format!("pid {pid} is the daemon hosting this session"))
}

/// `pkill [-f] <pattern>` (a regular expression over process names or,
/// with `-f`, command lines) and `killall [-r|-m] <name>...`.
fn kill_by_name(program: &str, args: &[String], daemon: &Daemon) -> Option<String> {
    let how = Matching::of(program, args)?;
    let (pattern, target) = args
        .iter()
        .filter(|a| !a.starts_with('-'))
        .find_map(|pattern| Some((pattern, how.target(pattern, daemon)?)))?;
    Some(format!(
        "`{program} {pattern}` matches {target} of the daemon hosting this session"
    ))
}

/// How a process-by-name tool reads its pattern.
struct Matching {
    /// A regular expression rather than an exact name.
    regex: bool,
    /// Against whole command lines rather than executable names.
    full: bool,
}

impl Matching {
    fn of(program: &str, args: &[String]) -> Option<Self> {
        let short = |flag: char| {
            args.iter()
                .any(|a| !a.starts_with("--") && a.starts_with('-') && a.contains(flag))
        };
        let long = |flag: &str| args.iter().any(|a| a == flag);
        match program {
            "pkill" | "pgrep" => Some(Self {
                regex: true,
                full: short('f') || long("--full"),
            }),
            "killall" => Some(Self {
                // psmisc `-r`/`--regexp`, BSD/macOS `-m`.
                regex: args
                    .iter()
                    .any(|a| matches!(a.as_str(), "-r" | "-m" | "--regexp")),
                full: false,
            }),
            "pidof" => Some(Self {
                regex: false,
                full: false,
            }),
            // Over `ps` output, whose lines carry whole command lines.
            "grep" | "egrep" => Some(Self {
                regex: true,
                full: true,
            }),
            _ => None,
        }
    }

    /// What of the daemon `pattern` reaches, described for the reason.
    fn target(&self, pattern: &str, daemon: &Daemon) -> Option<String> {
        if !self.regex {
            let name = basename(pattern).to_ascii_lowercase();
            let name = name.strip_suffix(".exe").unwrap_or(&name);
            return daemon
                .executables
                .iter()
                .find(|e| *e == name)
                .map(|e| format!("the {e} executable"));
        }
        let lower = pattern.to_ascii_lowercase();
        let hits = |text: &str| {
            RegexBuilder::new(pattern)
                .case_insensitive(true)
                .build()
                .map_or_else(
                    |_| text.to_ascii_lowercase().contains(&lower),
                    |re| re.is_match(text),
                )
        };
        if let Some(e) = daemon
            .executables
            .iter()
            .find(|e| hits(e) || lower.contains(e.as_str()))
        {
            return Some(format!("the {e} executable"));
        }
        self.full
            .then(|| daemon.command_lines.iter().find(|line| hits(line)))
            .flatten()
            .map(|line| format!("the command line `{line}`"))
    }
}

/// `taskkill /IM prometheus.exe` or `taskkill /PID <daemon pid>`.
fn taskkill(args: &[String], guarded: Guarded<'_>) -> Option<String> {
    if let Some(image) = option_value(args, &["/im"]) {
        let name = basename(image).to_ascii_lowercase();
        let wildcard = image.contains('*');
        if wildcard || guarded.daemon.executables.contains(&name) {
            return Some(format!(
                "`taskkill /IM {image}` matches the executable of the daemon hosting this session"
            ));
        }
    }
    let pid = option_value(args, &["/pid"])?.parse::<u32>().ok()?;
    (!guarded.own.contains(&pid) && guarded.daemon.pids.contains(&pid))
        .then(|| format!("pid {pid} is the daemon hosting this session"))
}

/// The value after a Windows-style option (`/TN name`), case-insensitive.
fn option_value<'a>(args: &'a [String], names: &[&str]) -> Option<&'a str> {
    let at = args
        .iter()
        .position(|a| names.iter().any(|n| a.eq_ignore_ascii_case(n)))?;
    args.get(at + 1).map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::{Daemon, Guarded, check};
    use crate::approval::shell::parse;

    fn daemon() -> Daemon {
        Daemon {
            pids: vec![777],
            executables: vec!["prometheus".into()],
            command_lines: vec!["/opt/pm/bin/pmt-host daemon run --profile work".into()],
            services: vec![
                "prometheus-daemon.service".into(),
                "ai.prometheus.daemon".into(),
                "Prometheus Daemon".into(),
            ],
            run_dirs: vec!["/home/u/.prometheus/run".into(), ".prometheus/run".into()],
        }
    }

    fn blocked(line: &str, own: &[u32]) -> Option<String> {
        let daemon = daemon();
        check(
            line,
            &parse(line).unwrap(),
            Guarded {
                daemon: &daemon,
                own,
            },
        )
    }

    #[test]
    fn commands_that_stop_the_daemon_are_named() {
        for line in [
            "pkill -f prometheus",
            "pkill prom.*",
            "killall prometheus",
            "sudo killall -9 /usr/local/bin/prometheus",
            "kill 777",
            "kill -9 777",
            "kill -TERM -777",
            "prometheus daemon stop",
            "prometheus --profile work gateway restart",
            "/opt/bin/prometheus daemon restart",
            "systemctl --user stop prometheus-daemon",
            "systemctl kill prometheus-daemon.service",
            "launchctl bootout gui/501/ai.prometheus.daemon",
            "launchctl kill SIGTERM gui/501/ai.prometheus.daemon",
            r#"schtasks /End /TN "Prometheus Daemon""#,
            "taskkill /F /IM prometheus.exe",
            "taskkill /PID 777",
            "echo ok && bash -c 'pkill -f prometheus'",
        ] {
            assert!(blocked(line, &[]).is_some(), "{line} should be blocked");
        }
    }

    /// `pkill -f` reads whole command lines and `killall -r`/`-m` reads a
    /// regular expression: a pattern reaching the daemon either way stops it.
    #[test]
    fn name_patterns_are_read_as_the_tools_read_them() {
        for line in [
            "pkill -f 'prometheus daemon'",
            "pkill -f bin/prometheus",
            "pkill --full 'PROMETHEUS.*run'",
            "pkill -f 'daemon run'",
            "pkill -9 -f pmt-host",
            "pkill -f 'bin/pmt.*work'",
            "killall -r prom",
            "killall -m 'pro.*eus'",
            "killall --regexp '^prom'",
            "kill $(pgrep -f 'daemon run')",
            "kill -9 `pidof prometheus`",
            "kill $(pgrep prom)",
        ] {
            assert!(blocked(line, &[]).is_some(), "{line} should be blocked");
        }
    }

    /// Pids reach `kill` through pipelines, variables and reads of the
    /// daemon's `run/` record.
    #[test]
    fn daemon_pids_reaching_kill_indirectly_are_blocked() {
        for line in [
            "pgrep prometheus | xargs kill",
            "pgrep -f 'daemon run' | head -1 | xargs -n1 kill -9",
            "pidof prometheus | xargs kill",
            "ps aux | grep prometheus | awk '{print $2}' | xargs kill",
            "xargs kill < ~/.prometheus/run/daemon.pid",
            "cat /home/u/.prometheus/run/gateway.json | jq .pid | xargs kill",
            "kill $(cat ~/.prometheus/run/daemon.pid)",
            "kill -9 $(jq .pid \"$HOME/.prometheus/run/gateway.json\")",
            "kill $(< /home/u/.prometheus/run/gateway.json)",
            "pkill -F /home/u/.prometheus/run/gateway.json",
            "p=$(pgrep prometheus); kill $p",
            "export P=`pidof prometheus` && kill -TERM \"${P}\"",
            "p=$(cat ~/.prometheus/run/gateway.json | jq .pid); q=$p; kill $q",
            "p=777; kill -9 $p",
            "pgrep prometheus | while read p; do kill $p; done",
            "read p < ~/.prometheus/run/daemon.pid; kill $p",
            "for p in $(pgrep -f prometheus); do kill $p; done",
        ] {
            assert!(blocked(line, &[]).is_some(), "{line} should be blocked");
        }
    }

    /// Code an interpreter runs: literals are command lines, `kill` calls
    /// name pids.
    #[test]
    fn code_execution_bodies_are_read() {
        for code in [
            r#"import os; os.system("pkill -f prometheus")"#,
            r#"import subprocess; subprocess.run(["pkill", "-f", "prometheus"])"#,
            "import subprocess\nsubprocess.Popen(['killall', 'prometheus'])",
            r#"require("child_process").execSync("kill $(pgrep prometheus)")"#,
            r#"require("child_process").spawn("pkill", ["-f", "prometheus"])"#,
            "import os, signal\nos.kill(777, signal.SIGTERM)",
            "import os\nos.kill(os.getppid(), 9)",
            "import os\nos.killpg(os.getpgid(os.getppid()), 15)",
            "process.kill(process.ppid)",
            "import json, os\npid = json.load(open(os.path.expanduser('~/.prometheus/run/gateway.json')))['pid']\nos.kill(pid, 15)",
            "# don't do this\nimport os; os.system('pkill -f prometheus')",
            r"python3 -c 'import os; os.kill(os.getppid(), 9)'",
            r#"bash -c "python3 -c 'import os; os.system(\"pkill prometheus\")'""#,
        ] {
            assert!(blocked(code, &[]).is_some(), "{code} should be blocked");
        }
    }

    #[test]
    fn unrelated_code_and_pipelines_pass() {
        for line in [
            "pgrep node | xargs kill",
            "cat ~/.prometheus/run/gateway.json | jq .pid",
            "p=$(pgrep node); kill $p",
            "p=4242; kill $p",
            "for p in $(pgrep webpack); do kill $p; done",
            r#"import os; os.system("pkill -f webpack")"#,
            r#"import subprocess; subprocess.run(["kill", "4242"])"#,
            "import os\nos.kill(4242, 9)",
            "import os\nos.kill(child.pid, 9)",
            r#"print("prometheus")"#,
        ] {
            assert_eq!(blocked(line, &[4242]), None, "{line} should pass");
        }
    }

    #[test]
    fn unrelated_and_own_processes_pass() {
        for line in [
            "kill 4242",
            "kill -9 4242",
            "pkill -f node",
            "killall python3",
            "prometheus daemon status",
            "prometheus sessions list",
            "systemctl --user stop nginx",
            "systemctl status prometheus-daemon",
            "launchctl list",
            "taskkill /IM node.exe",
            "echo prometheus daemon stop is dangerous",
            "pkill -f webpack",
            "pkill -f 'node server.js'",
            "killall -r '^node'",
            "killall prom",
            "kill $(pgrep node)",
            "pgrep -f prometheus",
        ] {
            assert_eq!(blocked(line, &[4242]), None, "{line} should pass");
        }
    }
}
