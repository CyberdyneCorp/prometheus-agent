//! `prometheus verify`: detect a project's run recipe and run it (spec:
//! execution-environments, Project run-recipe verification).
//!
//! The recipe comes from `<project>/.prometheus/environment.json` or from
//! the manifests (Cargo, package.json with its lockfile's package manager,
//! pyproject, go.mod, Makefile). Phases run in order under the OS sandbox
//! (workspace writable, network allowed) with a per-phase timeout; `start`
//! runs in the background until its port answers, then is stopped.

use std::fmt::Write as _;
use std::io::Read as _;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use clap::Args;
use pmt_core::error::{Error, codes};
use serde::{Deserialize, Serialize};

use crate::context::Context;

/// Characters of output shown per phase.
const TAIL_CHARS: usize = 2_000;
/// The phases, in run order.
pub const PHASES: [&str; 4] = ["bootstrap", "build", "test", "start"];

/// `prometheus verify` arguments.
#[derive(Debug, Args)]
pub struct VerifyArgs {
    /// The project directory (default: the current directory).
    pub path: Option<PathBuf>,
    /// Print the detected recipe as JSON and run nothing.
    #[arg(long)]
    pub detect_only: bool,
    /// Write the detected recipe to `.prometheus/environment.json`.
    #[arg(long)]
    pub save: bool,
    /// Run only these phases (repeatable).
    #[arg(long, value_parser = PHASES)]
    pub phase: Vec<String>,
    /// Skip the start phase.
    #[arg(long)]
    pub skip_start: bool,
    /// The port the start phase listens on.
    #[arg(long)]
    pub port: Option<u16>,
    /// Seconds each phase may run.
    #[arg(long, default_value_t = 600)]
    pub timeout: u64,
    /// Seconds the start phase may take to listen.
    #[arg(long, default_value_t = 60)]
    pub ready_timeout: u64,
}

/// A project's run recipe.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    /// What it was detected from.
    #[serde(default)]
    pub detected_from: String,
    /// Phase name to shell command.
    pub phases: Vec<Phase>,
    /// The port `start` listens on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

/// One phase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase {
    /// `bootstrap`, `build`, `test` or `start`.
    pub name: String,
    /// The shell command.
    pub command: String,
}

fn phase(name: &str, command: &str) -> Phase {
    Phase {
        name: name.to_owned(),
        command: command.to_owned(),
    }
}

/// The recipe of the project at `dir`, if one can be detected.
///
/// # Errors
///
/// An unreadable or invalid `environment.json`.
pub fn detect(dir: &Path) -> Result<Option<Recipe>, Error> {
    let saved = dir.join(".prometheus/environment.json");
    if saved.is_file() {
        let text =
            std::fs::read_to_string(&saved).map_err(|e| Error::io(&codes::HOME_IO, &saved, &e))?;
        let mut recipe: Recipe = serde_json::from_str(&text)
            .map_err(|e| Error::new(&codes::CLI_USAGE, format!("{}: {e}", saved.display())))?;
        ".prometheus/environment.json".clone_into(&mut recipe.detected_from);
        return Ok(Some(recipe));
    }
    let detectors: [fn(&Path) -> Option<Recipe>; 5] = [cargo, node, python, go, make];
    Ok(detectors.iter().find_map(|detect| detect(dir)))
}

fn cargo(dir: &Path) -> Option<Recipe> {
    dir.join("Cargo.toml").is_file().then(|| Recipe {
        detected_from: "Cargo.toml".to_owned(),
        phases: vec![phase("build", "cargo build"), phase("test", "cargo test")],
        port: None,
    })
}

fn node(dir: &Path) -> Option<Recipe> {
    let text = std::fs::read_to_string(dir.join("package.json")).ok()?;
    let manifest: serde_json::Value = serde_json::from_str(&text).ok()?;
    let pm = [
        ("pnpm-lock.yaml", "pnpm"),
        ("yarn.lock", "yarn"),
        ("bun.lockb", "bun"),
        ("bun.lock", "bun"),
    ]
    .iter()
    .find(|(lock, _)| dir.join(lock).is_file())
    .map_or("npm", |(_, pm)| pm);
    let mut phases = vec![phase("bootstrap", &format!("{pm} install"))];
    let scripts = manifest.get("scripts");
    for (name, command) in [
        ("build", format!("{pm} run build")),
        ("test", format!("{pm} test")),
        ("start", format!("{pm} start")),
    ] {
        if scripts.and_then(|s| s.get(name)).is_some() {
            phases.push(phase(name, &command));
        }
    }
    Some(Recipe {
        detected_from: "package.json".to_owned(),
        phases,
        port: None,
    })
}

fn python(dir: &Path) -> Option<Recipe> {
    if !dir.join("pyproject.toml").is_file() {
        return None;
    }
    let (bootstrap, test) = if dir.join("uv.lock").is_file() {
        ("uv sync", "uv run pytest")
    } else if dir.join("poetry.lock").is_file() {
        ("poetry install", "poetry run pytest")
    } else {
        ("python -m pip install -e .", "python -m pytest")
    };
    Some(Recipe {
        detected_from: "pyproject.toml".to_owned(),
        phases: vec![phase("bootstrap", bootstrap), phase("test", test)],
        port: None,
    })
}

fn go(dir: &Path) -> Option<Recipe> {
    dir.join("go.mod").is_file().then(|| Recipe {
        detected_from: "go.mod".to_owned(),
        phases: vec![
            phase("build", "go build ./..."),
            phase("test", "go test ./..."),
        ],
        port: None,
    })
}

fn make(dir: &Path) -> Option<Recipe> {
    let text = std::fs::read_to_string(dir.join("Makefile")).ok()?;
    let mut phases = vec![phase("build", "make")];
    if text.lines().any(|l| l.starts_with("test:")) {
        phases.push(phase("test", "make test"));
    }
    Some(Recipe {
        detected_from: "Makefile".to_owned(),
        phases,
        port: None,
    })
}

/// How one phase went.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Outcome {
    /// The phase.
    pub phase: String,
    /// The command.
    pub command: String,
    /// It passed.
    pub passed: bool,
    /// Its exit code (none when stopped or never started).
    pub exit_code: Option<i32>,
    /// Wall time.
    pub duration_ms: u64,
    /// Why it failed, when it did not just exit non-zero.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The end of its output.
    pub output_tail: String,
}

/// Runs `prometheus verify`.
///
/// # Errors
///
/// No recipe could be detected, the sandbox cannot be set up.
pub fn run(args: &VerifyArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let dir = match &args.path {
        Some(path) => path.clone(),
        None => {
            std::env::current_dir().map_err(|e| Error::io(&codes::HOME_IO, Path::new("."), &e))?
        }
    };
    let dir = std::path::absolute(&dir).map_err(|e| Error::io(&codes::HOME_IO, &dir, &e))?;
    let mut recipe = detect(&dir)?.ok_or_else(|| {
        Error::new(
            &codes::CLI_USAGE,
            format!("no run recipe found in {}: add Cargo.toml, package.json, pyproject.toml, go.mod, a Makefile or .prometheus/environment.json", dir.display()),
        )
    })?;
    if args.port.is_some() {
        recipe.port = args.port;
    }
    if args.save {
        save(&dir, &recipe)?;
    }
    if args.detect_only {
        println!(
            "{}",
            serde_json::to_string_pretty(&recipe).unwrap_or_default()
        );
        return Ok(pmt_core::exit::SUCCESS);
    }
    let scope = ctx.scope()?;
    let runner = Runner::new(&scope, ctx.launch, &dir, args)?;
    let outcomes: Vec<Outcome> = recipe
        .phases
        .iter()
        .filter(|p| args.phase.is_empty() || args.phase.contains(&p.name))
        .filter(|p| !(args.skip_start && p.name == "start"))
        .map(|p| runner.phase(p, recipe.port))
        .collect();
    let passed = outcomes.iter().all(|o| o.passed);
    record_evidence(&scope, ctx.launch, passed, &outcomes);
    if ctx.json {
        let report = serde_json::json!({"passed": passed, "recipe": recipe, "phases": outcomes});
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
    } else {
        print!("{}", render(&outcomes));
    }
    Ok(if passed {
        pmt_core::exit::SUCCESS
    } else {
        pmt_core::exit::FAILURE
    })
}

fn save(dir: &Path, recipe: &Recipe) -> Result<(), Error> {
    let path = dir.join(".prometheus/environment.json");
    let saved = Recipe {
        detected_from: String::new(),
        ..recipe.clone()
    };
    std::fs::create_dir_all(dir.join(".prometheus"))
        .and_then(|()| {
            std::fs::write(
                &path,
                serde_json::to_string_pretty(&saved).unwrap_or_default(),
            )
        })
        .map_err(|e| Error::io(&codes::HOME_IO, &path, &e))?;
    eprintln!("saved {}", path.display());
    Ok(())
}

/// Per phase: status, duration and output tail.
#[must_use]
pub fn render(outcomes: &[Outcome]) -> String {
    let mut text = String::new();
    for outcome in outcomes {
        let status = if outcome.passed { "passed" } else { "FAILED" };
        let _ = writeln!(
            text,
            "{}: {status} in {:.1}s ({})",
            outcome.phase,
            Duration::from_millis(outcome.duration_ms).as_secs_f64(),
            outcome.command
        );
        if let Some(reason) = &outcome.reason {
            let _ = writeln!(text, "  {reason}");
        }
        if !outcome.passed && !outcome.output_tail.is_empty() {
            for line in outcome.output_tail.lines() {
                let _ = writeln!(text, "  | {line}");
            }
        }
    }
    let passed = outcomes.iter().all(|o| o.passed);
    let _ = writeln!(
        text,
        "{}",
        if passed {
            "verify: passed"
        } else {
            "verify: failed"
        }
    );
    text
}

/// Runs phases under the OS sandbox in the project directory.
struct Runner {
    sandbox: pmt_sandbox::Sandbox,
    shell: PathBuf,
    env: pmt_tools::terminal::env::ChildEnv,
    dir: PathBuf,
    timeout: Duration,
    ready_timeout: Duration,
}

impl Runner {
    fn new(
        scope: &pmt_config::ProfileScope,
        launch: &pmt_core::env::LaunchEnv,
        dir: &Path,
        args: &VerifyArgs,
    ) -> Result<Self, Error> {
        let home = launch.user_home().cloned().unwrap_or_default();
        let scratch = scope.path("cache/verify");
        std::fs::create_dir_all(&scratch).map_err(|e| Error::io(&codes::HOME_IO, &scratch, &e))?;
        let paths = pmt_sandbox::Paths {
            user_home: home,
            prometheus_home: scope.home().to_path_buf(),
            workspace: dir.to_path_buf(),
            scratch: vec![scratch.clone()],
        };
        let policy =
            pmt_sandbox::Policy::from_capabilities(pmt_core::capability::BASELINE, &paths)?;
        let sandbox = pmt_sandbox::Sandbox::new(policy, &scope.config().security.sandbox)?;
        let mut env = pmt_tools::terminal::env::ChildEnv::build(
            launch,
            pmt_tools::terminal::env::EnvRequest {
                passthrough: &scope.config().terminal.env_passthrough,
                ..pmt_tools::terminal::env::EnvRequest::default()
            },
        );
        env.set_temp_dir(&scratch);
        let shell =
            pmt_tools::terminal::doctor::local_shell(&scope.config().terminal, launch, &sandbox)?
                .path;
        Ok(Self {
            sandbox,
            shell,
            env,
            dir: dir.to_path_buf(),
            timeout: Duration::from_secs(args.timeout),
            ready_timeout: Duration::from_secs(args.ready_timeout),
        })
    }

    fn spawn(&self, command: &str) -> Result<Child, String> {
        let mut cmd = self
            .sandbox
            .command(&self.shell)
            .map_err(|e| e.message().to_owned())?;
        self.env.apply(&mut cmd);
        cmd.arg("-c")
            .arg(command)
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.spawn().map_err(|e| format!("cannot start: {e}"))
    }

    fn phase(&self, phase: &Phase, port: Option<u16>) -> Outcome {
        let started = Instant::now();
        let mut outcome = Outcome {
            phase: phase.name.clone(),
            command: phase.command.clone(),
            passed: false,
            exit_code: None,
            duration_ms: 0,
            reason: None,
            output_tail: String::new(),
        };
        match self.spawn(&phase.command) {
            Err(why) => outcome.reason = Some(why),
            Ok(child) if phase.name == "start" => self.start(child, port, &mut outcome),
            Ok(child) => self.wait(child, &mut outcome),
        }
        outcome.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        outcome
    }

    fn wait(&self, mut child: Child, outcome: &mut Outcome) {
        let output = collect(&mut child);
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50));
                }
                _ => break None,
            }
        };
        if status.is_none() {
            let _ = child.kill();
            let _ = child.wait();
            outcome.reason = Some(format!("timed out after {} s", self.timeout.as_secs()));
        }
        outcome.output_tail = tail(&output.join());
        outcome.exit_code = status.and_then(|s| s.code());
        outcome.passed = status.is_some_and(|s| s.success());
    }

    fn start(&self, mut child: Child, port: Option<u16>, outcome: &mut Outcome) {
        let output = collect(&mut child);
        let deadline = Instant::now() + self.ready_timeout;
        let ready = loop {
            if let Ok(Some(status)) = child.try_wait() {
                outcome.exit_code = status.code();
                outcome.reason = Some(format!("exited before it was ready ({status})"));
                break false;
            }
            let listening = port.is_none_or(|port| {
                TcpStream::connect_timeout(
                    &([127, 0, 0, 1], port).into(),
                    Duration::from_millis(200),
                )
                .is_ok()
            });
            if listening && (port.is_some() || Instant::now() >= deadline) {
                break true;
            }
            if Instant::now() >= deadline {
                outcome.reason = Some(format!(
                    "not listening after {} s",
                    self.ready_timeout.as_secs()
                ));
                break false;
            }
            std::thread::sleep(Duration::from_millis(200));
        };
        let _ = child.kill();
        let _ = child.wait();
        outcome.passed = ready;
        outcome.output_tail = tail(&output.join());
    }
}

/// Readers of a child's stdout and stderr.
struct Collected(Vec<std::thread::JoinHandle<Vec<u8>>>);

impl Collected {
    fn join(self) -> String {
        let bytes: Vec<u8> = self
            .0
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

fn collect(child: &mut Child) -> Collected {
    let readers = [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|mut stream| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = stream.read_to_end(&mut bytes);
            bytes
        })
    })
    .collect();
    Collected(readers)
}

fn tail(text: &str) -> String {
    let count = text.chars().count();
    text.chars()
        .skip(count.saturating_sub(TAIL_CHARS))
        .collect()
}

/// Records the outcome as verification evidence of the session that ran
/// the command (`PROMETHEUS_SESSION_ID`, set by `--pass-session-id`).
fn record_evidence(
    scope: &pmt_config::ProfileScope,
    launch: &pmt_core::env::LaunchEnv,
    passed: bool,
    outcomes: &[Outcome],
) {
    let Some(session) = launch
        .inherited("PROMETHEUS_SESSION_ID")
        .and_then(|v| v.to_str().map(ToOwned::to_owned))
    else {
        return;
    };
    let recorded = pmt_store::Store::open(scope).and_then(|store| {
        let id = store.resolve_session(&session)?;
        let summary = serde_json::to_string(outcomes).unwrap_or_default();
        store.record_verification(id, passed, &summary)
    });
    if let Err(err) = recorded {
        tracing::warn!("verification evidence not recorded: {err}");
    }
}
