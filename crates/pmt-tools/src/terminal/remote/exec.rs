//! Short blocking calls on a remote backend: the file tools, sync-back and
//! the terminal's own probes run `sh -c <script> <args>` there.

use std::io::Write as _;
use std::process::Stdio;

use super::{CALL_TIMEOUT, Transport};

/// The outcome of one call.
#[derive(Debug, Clone, Default)]
pub struct ExecResult {
    /// The exit code (`-1` when killed).
    pub code: i32,
    /// Standard output.
    pub stdout: Vec<u8>,
    /// Standard error.
    pub stderr: Vec<u8>,
}

/// Runs scripts on a backend whose files are not host files.
pub trait RemoteExec: Send + Sync + std::fmt::Debug {
    /// Runs `sh -c script pmt args...`, feeding `stdin` when given.
    ///
    /// # Errors
    ///
    /// The backend could not be reached.
    fn exec(&self, script: &str, args: &[&str], stdin: Option<&[u8]>)
    -> Result<ExecResult, String>;

    /// A native whole-file write, when the backend has one (`None`: write
    /// through [`RemoteExec::exec`]).
    fn upload(&self, _path: &str, _bytes: &[u8]) -> Option<Result<(), String>> {
        None
    }

    /// A native whole-file read, when the backend has one.
    fn download(&self, _path: &str) -> Option<Result<Vec<u8>, String>> {
        None
    }
}

/// [`RemoteExec`] over a `terminal-backend` plugin: scripts go to its
/// `execute` (no stdin), whole files to `upload` and `download`.
#[derive(Debug)]
pub struct PluginExec(pub std::sync::Arc<dyn pmt_sandbox::backend::RemoteBackend>);

impl RemoteExec for PluginExec {
    fn exec(
        &self,
        script: &str,
        args: &[&str],
        stdin: Option<&[u8]>,
    ) -> Result<ExecResult, String> {
        if stdin.is_some() {
            return Err("plugin backends take file contents through upload only".to_owned());
        }
        let mut line = vec!["sh", "-c", script, "pmt"];
        line.extend_from_slice(args);
        let output = self.0.execute(&pmt_sandbox::backend::ExecRequest {
            id: ulid::Ulid::generate().to_string(),
            command: pmt_sandbox::ssh::quote_all(&line),
            cwd: self.0.descriptor().default_cwd.clone(),
            env: Vec::new(),
            timeout_ms: u64::try_from(CALL_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
        })?;
        Ok(ExecResult {
            code: output
                .exit_code
                .ok_or("the execute result has no exit_code")?,
            stdout: output.stdout.into_bytes(),
            stderr: output.stderr.into_bytes(),
        })
    }

    fn upload(&self, path: &str, bytes: &[u8]) -> Option<Result<(), String>> {
        Some(self.0.upload(path, bytes))
    }

    fn download(&self, path: &str) -> Option<Result<Vec<u8>, String>> {
        Some(self.0.download(path))
    }
}

/// File tools may run before the first command: the environment is
/// connected (the container started) first.
impl RemoteExec for std::sync::Arc<dyn Transport> {
    fn exec(
        &self,
        script: &str,
        args: &[&str],
        stdin: Option<&[u8]>,
    ) -> Result<ExecResult, String> {
        self.connect().map_err(|err| match err {
            crate::terminal::spawn::SpawnError::Refused(err) => err.message().to_owned(),
            crate::terminal::spawn::SpawnError::Infrastructure(why) => why,
        })?;
        run(self.as_ref(), script, args, stdin)
    }
}

/// [`RemoteExec::exec`] over a transport.
///
/// # Errors
///
/// The client cannot start or times out.
pub fn run(
    transport: &dyn Transport,
    script: &str,
    args: &[&str],
    stdin: Option<&[u8]>,
) -> Result<ExecResult, String> {
    let mut line = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        script.to_owned(),
        "pmt".to_owned(),
    ];
    line.extend(args.iter().map(|a| (*a).to_owned()));
    let mut command = transport.command(&line, &[], stdin.is_some())?;
    let Some(input) = stdin else {
        let output = pmt_sandbox::container::run_bounded(command, CALL_TIMEOUT)?;
        return Ok(ExecResult {
            code: output.status.code().unwrap_or(-1),
            stdout: output.stdout,
            stderr: output.stderr,
        });
    };
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let mut pipe = child.stdin.take().ok_or("no stdin")?;
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = pipe.write_all(&input);
    });
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    let _ = writer.join();
    Ok(ExecResult {
        code: output.status.code().unwrap_or(-1),
        stdout: output.stdout,
        stderr: output.stderr,
    })
}
