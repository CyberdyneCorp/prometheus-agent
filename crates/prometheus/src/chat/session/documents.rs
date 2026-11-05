//! The managed `doc-converter` runtime behind `read_file` document
//! extraction (spec: execution-environments, `read_file` document
//! extraction; distribution-and-update, Lazy runtime installation policy).
//!
//! The runtime is an executable called as `doc-converter --json <file>`
//! that prints one JSON object: `{"pages": ["...", ...]}` (text per page,
//! for PDFs) or `{"markdown": "..."}`. It comes from the managed runtime
//! store, else from `PATH`, else is installed on first use when
//! `runtimes.allow_lazy_install` allows; the conversion runs with an empty
//! environment and a time bound, and its output is size-bounded.

use std::io::Read as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_tools::browser::RuntimePolicy;
use pmt_tools::files::documents::{ConvertError, Converted, DocConverter};
use serde_json::Value;

/// The runtime's name (and its executable's name on `PATH`).
pub const RUNTIME: &str = "doc-converter";
/// Longest conversion.
const TIMEOUT: Duration = Duration::from_mins(2);
/// Most bytes of converter output read.
const MAX_OUTPUT: u64 = 64 * 1024 * 1024;

/// The profile's converter, when its runtime store can be opened.
#[must_use]
pub fn converter(scope: &ProfileScope, launch: &LaunchEnv) -> Option<Arc<dyn DocConverter>> {
    let policies = crate::runtimes_cmd::browser::for_home(
        scope.home(),
        &scope.config().network,
        None,
        launch.inherited("PATH"),
    )
    .inspect_err(|err| tracing::debug!("no doc-converter runtime: {err}"))
    .ok()?;
    let handle = tokio::runtime::Handle::try_current().ok()?;
    Some(Arc::new(RuntimeConverter {
        policy: policies.documents,
        handle,
    }))
}

/// Converts through the `doc-converter` executable.
pub struct RuntimeConverter {
    policy: RuntimePolicy,
    handle: tokio::runtime::Handle,
}

impl RuntimeConverter {
    /// A converter over `policy`; installs run on `handle`.
    #[must_use]
    pub fn new(policy: RuntimePolicy, handle: tokio::runtime::Handle) -> Self {
        Self { policy, handle }
    }
}

impl DocConverter for RuntimeConverter {
    fn convert(&self, path: &Path, _ext: &str) -> Result<Converted, ConvertError> {
        // `read_file` runs on a blocking thread: waiting here is allowed.
        let executable = self
            .handle
            .block_on(self.policy.executable(RUNTIME))
            .map_err(ConvertError::Unavailable)?;
        let output = run(&executable, path)?;
        parse(&output)
    }
}

/// Runs the converter on `path`, bounded by [`TIMEOUT`].
fn run(executable: &Path, path: &Path) -> Result<Vec<u8>, ConvertError> {
    let mut child = Command::new(executable)
        .arg("--json")
        .arg(path)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| ConvertError::Failed(format!("cannot start {RUNTIME}: {e}")))?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = (&mut stdout).take(MAX_OUTPUT).read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ConvertError::Failed(format!(
                    "{RUNTIME} did not finish within {} s",
                    TIMEOUT.as_secs()
                )));
            }
        }
    };
    let bytes = reader.join().unwrap_or_default();
    if !status.success() {
        return Err(ConvertError::Failed(format!("{RUNTIME} failed ({status})")));
    }
    Ok(bytes)
}

/// The converter's JSON answer.
fn parse(bytes: &[u8]) -> Result<Converted, ConvertError> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|e| ConvertError::Failed(format!("{RUNTIME} printed no JSON: {e}")))?;
    if let Some(pages) = value.get("pages").and_then(Value::as_array) {
        return Ok(Converted::Pages(
            pages
                .iter()
                .map(|p| p.as_str().unwrap_or_default().to_owned())
                .collect(),
        ));
    }
    value
        .get("markdown")
        .and_then(Value::as_str)
        .map(|text| Converted::Markdown(text.to_owned()))
        .ok_or_else(|| {
            ConvertError::Failed(format!("{RUNTIME} printed neither pages nor markdown"))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_answer_is_pages_or_markdown() {
        assert_eq!(
            parse(br#"{"pages": ["a", ""]}"#).unwrap(),
            Converted::Pages(vec!["a".into(), String::new()])
        );
        assert_eq!(
            parse(br##"{"markdown": "# T"}"##).unwrap(),
            Converted::Markdown("# T".into())
        );
        assert!(matches!(parse(b"{}"), Err(ConvertError::Failed(_))));
    }
}
