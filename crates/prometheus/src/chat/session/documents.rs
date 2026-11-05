//! The managed `doc-converter` runtime behind `read_file` document
//! extraction (spec: execution-environments, `read_file` document
//! extraction; distribution-and-update, Lazy runtime installation policy).
//!
//! The runtime is the pinned `xberg` CLI (runtime `doc-converter` in the
//! runtime index). It comes from the managed runtime store, else `xberg`
//! on `PATH`, else is installed on first use when
//! `runtimes.allow_lazy_install` allows. Each conversion runs
//! `xberg extract <copy> --format json ...` under the OS sandbox: it reads
//! only a private copy of the document, the runtime's own directory and the
//! system toolchain, writes only a private temp directory and has no
//! network, with an empty environment, a time bound and a size bound on
//! its output (it parses untrusted files).

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pmt_config::ProfileScope;
use pmt_config::schema::SandboxConfig;
use pmt_core::env::LaunchEnv;
use pmt_tools::browser::RuntimePolicy;
use pmt_tools::files::documents::{ConvertError, Converted, DocConverter};
use serde_json::Value;

/// The runtime's name in the pinned runtime index.
pub const RUNTIME: &str = "doc-converter";
/// The converter's executable name on `PATH`.
pub const HOST_EXECUTABLE: &str = "xberg";
/// Longest conversion.
const TIMEOUT: Duration = Duration::from_mins(2);
/// Most bytes of converter output read.
const MAX_OUTPUT: u64 = 64 * 1024 * 1024;
/// Extraction settings beyond the flags: no OCR (a page without a text
/// layer stays empty, which the coverage warning reports).
const CONFIG_JSON: &str = r#"{"disable_ocr":true}"#;

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
    let confinement = Confinement {
        work: scope.path("cache/documents"),
        user_home: launch.user_home().cloned().unwrap_or_default(),
        prometheus_home: scope.home().to_path_buf(),
        sandbox: scope.config().security.sandbox.clone(),
    };
    Some(Arc::new(RuntimeConverter::new(
        policies.documents,
        handle,
        confinement,
    )))
}

/// Where and how conversions are confined.
#[derive(Debug, Clone)]
pub struct Confinement {
    /// Parent of the per-conversion directories (the document copy and
    /// the converter's temp directory); each is removed afterwards.
    pub work: PathBuf,
    /// The user's home (credential stores below a granted root stay
    /// denied).
    pub user_home: PathBuf,
    /// `PROMETHEUS_HOME`.
    pub prometheus_home: PathBuf,
    /// `security.sandbox` (mode and allowed downgrades).
    pub sandbox: SandboxConfig,
}

/// Converts through the `xberg` executable.
pub struct RuntimeConverter {
    policy: RuntimePolicy,
    handle: tokio::runtime::Handle,
    confinement: Confinement,
}

impl RuntimeConverter {
    /// A converter over `policy`; installs run on `handle`.
    #[must_use]
    pub fn new(
        policy: RuntimePolicy,
        handle: tokio::runtime::Handle,
        confinement: Confinement,
    ) -> Self {
        Self {
            policy,
            handle,
            confinement,
        }
    }
}

impl DocConverter for RuntimeConverter {
    fn convert(&self, path: &Path, ext: &str) -> Result<Converted, ConvertError> {
        // `read_file` runs on a blocking thread: waiting here is allowed.
        let executable = self
            .handle
            .block_on(self.policy.executable(RUNTIME))
            .map_err(ConvertError::Unavailable)?;
        let job = Job::stage(&self.confinement, path, ext)
            .map_err(|e| ConvertError::Failed(format!("cannot stage the document: {e}")))?;
        let output = job.run(&self.confinement, &executable)?;
        parse(&output, ext)
    }
}

/// One conversion's private directory: `input/<copy>` (read-only to the
/// converter) and `tmp/` (its only writable place). Removed on drop.
struct Job {
    dir: tempfile::TempDir,
    input: PathBuf,
}

impl Job {
    fn stage(confinement: &Confinement, path: &Path, ext: &str) -> std::io::Result<Self> {
        std::fs::create_dir_all(&confinement.work)?;
        let dir = tempfile::Builder::new()
            .prefix("convert-")
            .tempdir_in(&confinement.work)?;
        std::fs::create_dir(dir.path().join("input"))?;
        std::fs::create_dir(dir.path().join("tmp"))?;
        let input = dir.path().join("input").join(format!("document.{ext}"));
        std::fs::copy(path, &input)?;
        Ok(Self { dir, input })
    }

    fn tmp(&self) -> PathBuf {
        self.dir.path().join("tmp")
    }

    /// The sandbox of this conversion: read the copy, the converter's
    /// directory and the toolchain; write `tmp/`; no network.
    fn sandbox(
        &self,
        confinement: &Confinement,
        executable: &Path,
    ) -> Result<pmt_sandbox::Sandbox, ConvertError> {
        let runtime_dir = std::fs::canonicalize(executable)
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| executable.parent().unwrap_or(executable).to_path_buf());
        let paths = pmt_sandbox::Paths {
            user_home: confinement.user_home.clone(),
            prometheus_home: confinement.prometheus_home.clone(),
            workspace: self.dir.path().join("input"),
            scratch: vec![self.tmp()],
        };
        let capabilities = [
            "fs.read:<workspace>/**".to_owned(),
            "fs.read:<scratch>/**".to_owned(),
            "fs.write:<scratch>/**".to_owned(),
            "fs.read:<toolchain>".to_owned(),
            format!("fs.read:{}/**", runtime_dir.display()),
            "process.spawn:*".to_owned(),
        ];
        let sandbox_error =
            |e: pmt_core::error::Error| ConvertError::Failed(e.message().to_owned());
        let policy =
            pmt_sandbox::Policy::from_capabilities(&capabilities, &paths).map_err(sandbox_error)?;
        pmt_sandbox::Sandbox::new(policy, &confinement.sandbox).map_err(sandbox_error)
    }

    /// Runs the converter on the copy, bounded by [`TIMEOUT`].
    fn run(&self, confinement: &Confinement, executable: &Path) -> Result<Vec<u8>, ConvertError> {
        let sandbox = self.sandbox(confinement, executable)?;
        let mut command = sandbox
            .command(executable)
            .map_err(|e| ConvertError::Failed(e.message().to_owned()))?;
        let tmp = self.tmp();
        command
            .arg("extract")
            .arg(&self.input)
            .args(["--format", "json", "--no-config-discovery"])
            .args(["--extract-pages", "true", "--no-cache", "true"])
            .args(["--content-format", "markdown", "--config-json", CONFIG_JSON])
            .env_clear()
            .env("HOME", &tmp)
            .env("TMPDIR", &tmp)
            .env("TMP", &tmp)
            .env("TEMP", &tmp)
            .current_dir(&tmp)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command
            .spawn()
            .map_err(|e| ConvertError::Failed(format!("cannot start {HOST_EXECUTABLE}: {e}")))?;
        wait_bounded(child)
    }
}

/// The child's stdout once it exits within [`TIMEOUT`].
fn wait_bounded(mut child: std::process::Child) -> Result<Vec<u8>, ConvertError> {
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let reader = std::thread::spawn(move || read_bounded(stdout, MAX_OUTPUT));
    let errors = std::thread::spawn(move || read_bounded(stderr, 4096));
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ConvertError::Failed(format!(
                    "{HOST_EXECUTABLE} did not finish within {} s",
                    TIMEOUT.as_secs()
                )));
            }
        }
    };
    let bytes = reader.join().unwrap_or_default();
    let stderr = errors.join().unwrap_or_default();
    if status.success() {
        return Ok(bytes);
    }
    let detail = String::from_utf8_lossy(&stderr);
    let detail = detail.trim();
    Err(ConvertError::Failed(if detail.is_empty() {
        format!("{HOST_EXECUTABLE} failed ({status})")
    } else {
        format!("{HOST_EXECUTABLE} failed ({status}): {detail}")
    }))
}

fn read_bounded(source: impl std::io::Read, limit: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    let _ = source.take(limit).read_to_end(&mut bytes);
    bytes
}

/// The converter's JSON answer (`{"result": {"content", "pages"}}`): the
/// text per page for a PDF (in `page_number` order), else the Markdown
/// content.
fn parse(bytes: &[u8], ext: &str) -> Result<Converted, ConvertError> {
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|e| ConvertError::Failed(format!("{HOST_EXECUTABLE} printed no JSON: {e}")))?;
    let result = value.get("result").ok_or_else(|| {
        ConvertError::Failed(format!("{HOST_EXECUTABLE} printed no extraction result"))
    })?;
    if ext == "pdf"
        && let Some(pages) = result.get("pages").and_then(Value::as_array)
    {
        return Ok(Converted::Pages(pages_in_order(pages)));
    }
    result
        .get("content")
        .and_then(Value::as_str)
        .map(|text| Converted::Markdown(text.to_owned()))
        .ok_or_else(|| ConvertError::Failed(format!("{HOST_EXECUTABLE} printed no content")))
}

fn pages_in_order(pages: &[Value]) -> Vec<String> {
    let mut numbered: Vec<(u64, String)> = pages
        .iter()
        .zip(1u64..)
        .map(|(page, position)| {
            let number = page
                .get("page_number")
                .and_then(Value::as_u64)
                .unwrap_or(position);
            let text = page.get("content").and_then(Value::as_str).unwrap_or("");
            (number, text.to_owned())
        })
        .collect();
    numbered.sort_by_key(|(number, _)| *number);
    numbered.into_iter().map(|(_, text)| text).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pdf_answer_is_its_pages_in_order_and_others_are_markdown() {
        let pdf = br#"{"result": {"content": "all", "pages": [
            {"page_number": 2, "content": ""}, {"page_number": 1, "content": "a"}]}}"#;
        assert_eq!(
            parse(pdf, "pdf").unwrap(),
            Converted::Pages(vec!["a".into(), String::new()])
        );
        assert_eq!(
            parse(br##"{"result": {"content": "# T", "pages": null}}"##, "odt").unwrap(),
            Converted::Markdown("# T".into())
        );
        assert_eq!(
            parse(br#"{"result": {"content": "x"}}"#, "pdf").unwrap(),
            Converted::Markdown("x".into())
        );
        assert!(matches!(parse(b"{}", "pdf"), Err(ConvertError::Failed(_))));
    }
}
