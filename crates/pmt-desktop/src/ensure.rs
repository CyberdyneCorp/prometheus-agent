//! Running the bundled binary (spec: desktop-app, Daemon attachment through
//! the bundled binary; Ensure is bounded). The shell never starts an agent
//! or owns the daemon's pid: it runs `prometheus daemon ensure --json`, which
//! starts the daemon detached when it is absent, and reads exactly one JSON
//! object from its stdout.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use tokio::process::Command;

/// How long `daemon ensure` may take to report a ready daemon.
pub const DEADLINE: Duration = Duration::from_mins(1);
/// Extra time on top of [`DEADLINE`] before the run is given up.
pub const GRACE: Duration = Duration::from_secs(15);

/// What `daemon ensure --json` reports: the daemon of the home.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct EnsureReport {
    /// The loopback WebSocket URL.
    pub rpc_url: String,
    /// The owner-only control channel (dial tickets come from it).
    pub control: String,
    /// Random per daemon start.
    pub instance_id: String,
    /// The daemon binary's build id.
    pub build_id: String,
    /// The daemon's semantic version.
    #[serde(default)]
    pub version: String,
    /// The protocol, `<major>.<minor>`.
    pub rpc_protocol: String,
    /// Protocol features the daemon serves (`session-attach`).
    #[serde(default)]
    pub features: Vec<String>,
}

/// What `prometheus version --json` reports about the bundled binary.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BinaryIdentity {
    /// Semantic version.
    pub version: String,
    /// Hash unique to the binary.
    pub build_id: String,
}

/// Why running the bundled binary gave no usable answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// It exited without printing exactly one JSON object of the expected
    /// shape.
    NoResult {
        /// The subcommand (`daemon ensure`).
        command: &'static str,
        /// Its exit code, `None` when a signal ended it.
        exit: Option<i32>,
        /// The last non-empty line it wrote to stderr.
        stderr: String,
    },
    /// It did not finish within its bound (it is killed).
    TimedOut {
        /// The bound, in seconds.
        secs: u64,
    },
    /// It could not be started.
    Spawn(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoResult {
                command,
                exit,
                stderr,
            } => {
                let exit = exit.map_or_else(|| "signal".to_owned(), |code| code.to_string());
                write!(
                    f,
                    "prometheus {command} produced no result (exit {exit}): {stderr}"
                )
            }
            Self::TimedOut { secs } => {
                write!(f, "gateway daemon did not become ready within {secs} s")
            }
            Self::Spawn(why) => write!(f, "cannot run the bundled prometheus binary: {why}"),
        }
    }
}

impl std::error::Error for RunError {}

/// Runs `binary daemon ensure --json [--profile <profile>]` within `bound`
/// ([`DEADLINE`] plus [`GRACE`] in the app).
///
/// # Errors
///
/// [`RunError`]: an expired run is never treated as success.
pub async fn ensure(
    binary: &Path,
    profile: Option<&str>,
    bound: Duration,
) -> Result<EnsureReport, RunError> {
    let mut args = vec!["--json", "daemon", "ensure"];
    if let Some(profile) = profile {
        args.extend(["--profile", profile]);
    }
    run_json(binary, &args, "daemon ensure", bound).await
}

/// The bundled binary's version and build id (`version --json`).
///
/// # Errors
///
/// As [`ensure`].
pub async fn identity(binary: &Path, bound: Duration) -> Result<BinaryIdentity, RunError> {
    run_json(binary, &["--json", "version"], "version", bound).await
}

/// Runs `binary args` and reads exactly one JSON object of type `T` from
/// its stdout; anything else is [`RunError::NoResult`].
///
/// # Errors
///
/// [`RunError`].
pub async fn run_json<T: DeserializeOwned>(
    binary: &Path,
    args: &[&str],
    command: &'static str,
    bound: Duration,
) -> Result<T, RunError> {
    let child = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // An expired run is killed when the future is dropped; a daemon it
        // started is detached and keeps running.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| RunError::Spawn(format!("{}: {e}", binary.display())))?;
    let output = tokio::time::timeout(bound, child.wait_with_output())
        .await
        .map_err(|_| RunError::TimedOut {
            secs: bound.as_secs(),
        })?
        .map_err(|e| RunError::Spawn(e.to_string()))?;
    let parsed = output
        .status
        .success()
        .then(|| one_object(&String::from_utf8_lossy(&output.stdout)))
        .flatten();
    parsed.ok_or_else(|| RunError::NoResult {
        command,
        exit: output.status.code(),
        stderr: last_line(&String::from_utf8_lossy(&output.stderr)),
    })
}

/// `text` when it is exactly one JSON object of type `T` and nothing else.
fn one_object<T: DeserializeOwned>(text: &str) -> Option<T> {
    let mut values = serde_json::Deserializer::from_str(text).into_iter::<serde_json::Value>();
    let first = values.next()?.ok()?;
    if values.next().is_some() || !first.is_object() {
        return None;
    }
    serde_json::from_value(first).ok()
}

fn last_line(text: &str) -> String {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("(nothing on stderr)")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::{EnsureReport, RunError, one_object};

    #[test]
    fn only_one_json_object_is_a_result() {
        let one = r#"{"rpc_url":"ws://127.0.0.1:1/rpc","control":"c","instance_id":"i","build_id":"b","rpc_protocol":"1.0"}"#;
        assert!(one_object::<EnsureReport>(one).is_some());
        assert!(one_object::<EnsureReport>(&format!("{one}\n{one}")).is_none());
        assert!(one_object::<EnsureReport>("starting...\n").is_none());
        assert!(one_object::<EnsureReport>("[1]").is_none());
        assert!(one_object::<EnsureReport>(r#"{"rpc_url":"x"}"#).is_none());
    }

    #[test]
    fn the_errors_read_as_the_spec_words_them() {
        let none = RunError::NoResult {
            command: "daemon ensure",
            exit: Some(2),
            stderr: "boom".into(),
        };
        assert_eq!(
            none.to_string(),
            "prometheus daemon ensure produced no result (exit 2): boom"
        );
        let bound = super::DEADLINE + super::GRACE;
        assert_eq!(
            RunError::TimedOut {
                secs: bound.as_secs()
            }
            .to_string(),
            "gateway daemon did not become ready within 75 s"
        );
    }
}
