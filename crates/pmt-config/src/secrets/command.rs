//! The command secret source (spec: security-boundaries, Command secret
//! source): `secrets.command.argv` is run once per process, without a
//! shell, in its own process group, and its stdout is read as `NAME=VALUE`
//! lines. Stderr is discarded and only the exit status is ever logged, so
//! a helper that echoes a secret or a prompt cannot leak it. Every failure
//! (empty argv, a non-zero exit, a timeout, output that is not a map) is
//! one WARN line with a remediation hint and never stops startup: the
//! source then contributes nothing.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
#[cfg(unix)]
use std::time::{Duration, Instant};

use pmt_core::secret::SecretString;

use crate::schema::{SecretCommandConfig, is_secret_name};

/// The most stdout a helper may print.
pub const MAX_OUTPUT: usize = 1024 * 1024;

/// Values from the helper, by name.
#[derive(Default)]
pub struct CommandSecrets {
    values: BTreeMap<String, SecretString>,
    /// `secrets.command.override_existing`.
    pub override_existing: bool,
}

impl std::fmt::Debug for CommandSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandSecrets")
            .field("names", &self.values.keys().collect::<Vec<_>>())
            .field("override_existing", &self.override_existing)
            .finish()
    }
}

impl CommandSecrets {
    /// The value of `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&SecretString> {
        self.values.get(name)
    }

    /// Every value, by name.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &SecretString)> {
        self.values.iter()
    }
}

/// Why the helper contributed nothing (the WARN line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// `argv` is empty.
    EmptyArgv,
    /// It could not start, exited non-zero, was killed or timed out.
    Failed(String),
    /// Its stdout was not `NAME=VALUE` lines within [`MAX_OUTPUT`].
    NotAMap,
    /// Windows has no command source.
    Unsupported,
}

impl Failure {
    /// The one line logged, with its remediation hint.
    #[must_use]
    pub fn line(&self) -> String {
        match self {
            Self::EmptyArgv => "secrets.command.argv is empty; set the helper's argv or set secrets.command.enabled = false".to_owned(),
            Self::Failed(status) => format!(
                "helper command failed ({status}); run secrets.command.argv by hand to check it prints NAME=VALUE lines without waiting for input"
            ),
            Self::NotAMap => "helper output was not a NAME=VALUE map; print one NAME=VALUE per line (names A-Z, 0-9, _), at most 1 MiB".to_owned(),
            Self::Unsupported => "secrets.command is not supported on Windows; set secrets.command.enabled = false".to_owned(),
        }
    }
}

/// The helper's values for `config`, run at most once per process for the
/// same argv, timeout and precedence; `None` when the source is disabled.
#[must_use]
pub fn load(config: &SecretCommandConfig) -> Option<Arc<CommandSecrets>> {
    type Key = (Vec<String>, u64, bool);
    static RUNS: OnceLock<Mutex<HashMap<Key, Arc<CommandSecrets>>>> = OnceLock::new();
    if !config.enabled {
        return None;
    }
    let key = (
        config.argv.clone(),
        config.timeout_secs,
        config.override_existing,
    );
    let mut runs = RUNS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let loaded = runs.entry(key).or_insert_with(|| {
        let values = run(config).unwrap_or_else(|failure| {
            // Printed and logged once per process (never the helper's
            // output: the line names only the status).
            eprintln!("warning: {}", failure.line());
            tracing::warn!("{}", failure.line());
            BTreeMap::new()
        });
        Arc::new(CommandSecrets {
            values,
            override_existing: config.override_existing,
        })
    });
    Some(Arc::clone(loaded))
}

/// Runs the helper of `config` (no caching).
///
/// # Errors
///
/// The [`Failure`] to log.
pub fn run(config: &SecretCommandConfig) -> Result<BTreeMap<String, SecretString>, Failure> {
    if config.argv.is_empty() {
        return Err(Failure::EmptyArgv);
    }
    let stdout = execute(&config.argv, config.timeout_secs)?;
    parse(&stdout)
}

#[cfg(windows)]
fn execute(_argv: &[String], _timeout_secs: u64) -> Result<Vec<u8>, Failure> {
    Err(Failure::Unsupported)
}

#[cfg(unix)]
fn execute(argv: &[String], timeout_secs: u64) -> Result<Vec<u8>, Failure> {
    use std::io::Read as _;
    use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
    use std::process::{Command, Stdio};

    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Failure::Failed(format!("could not start: {}", e.kind())))?;
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let limit = u64::try_from(MAX_OUTPUT + 1).unwrap_or(u64::MAX);
        let _ = (&mut stdout).take(limit).read_to_end(&mut out);
        out
    });
    let deadline = Instant::now() + Duration::from_secs(timeout_secs.max(1));
    let status = loop {
        if let Ok(Some(status)) = child.try_wait() {
            break status;
        }
        if Instant::now() >= deadline {
            kill_group(child.id());
            let _ = child.wait();
            return Err(Failure::Failed(format!(
                "timed out after {} s; killed",
                timeout_secs.max(1)
            )));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // Descendants left in the group would keep stdout open (and the reader
    // waiting); the group outlives its reaped leader while they run.
    kill_group(child.id());
    let out = reader.join().unwrap_or_default();
    match (status.code(), status.signal()) {
        (Some(0), _) => Ok(out),
        (Some(code), _) => Err(Failure::Failed(format!("exit {code}"))),
        (None, Some(signal)) => Err(Failure::Failed(format!("signal {signal}"))),
        (None, None) => Err(Failure::Failed("unknown status".to_owned())),
    }
}

#[cfg(unix)]
fn kill_group(pid: u32) {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;
    if let Ok(raw) = i32::try_from(pid) {
        let _ = killpg(Pid::from_raw(raw), Signal::SIGKILL);
    }
}

/// `NAME=VALUE` lines; blank lines are skipped, whitespace-only values are
/// unset and the first claim of a name wins.
fn parse(stdout: &[u8]) -> Result<BTreeMap<String, SecretString>, Failure> {
    if stdout.len() > MAX_OUTPUT {
        return Err(Failure::NotAMap);
    }
    let text = std::str::from_utf8(stdout).map_err(|_| Failure::NotAMap)?;
    let mut values = BTreeMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let (name, value) = line.split_once('=').ok_or(Failure::NotAMap)?;
        let name = name.trim();
        if !is_secret_name(name) {
            return Err(Failure::NotAMap);
        }
        if value.trim().is_empty() || values.contains_key(name) {
            continue;
        }
        values.insert(name.to_owned(), super::clean_value(name, value.to_owned()));
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::{Failure, parse};

    #[test]
    fn output_is_a_name_value_map() {
        let values = parse(b"A_KEY=one\n\nB_KEY=  \nA_KEY=two\nC_KEY=x=y\n").unwrap();
        let names: Vec<&String> = values.keys().collect();
        assert_eq!(names, ["A_KEY", "C_KEY"]);
        assert_eq!(values["A_KEY"].expose(), "one");
        assert_eq!(values["C_KEY"].expose(), "x=y");
        assert_eq!(parse(b"Enter PIN:").unwrap_err(), Failure::NotAMap);
        assert_eq!(parse(b"lower=1").unwrap_err(), Failure::NotAMap);
        assert_eq!(
            parse(&vec![b'A'; super::MAX_OUTPUT + 1]).unwrap_err(),
            Failure::NotAMap
        );
    }
}
