//! Command-template speech providers (spec: web-and-media-tools,
//! Command-template speech providers).
//!
//! `[tts.providers.<name>]` and `[stt.providers.<name>]` run a shell
//! command whose `{placeholders}` are replaced by values quoted for the
//! quoting context they appear in (bare, inside single quotes, inside
//! double quotes), so a path with spaces stays one argument; `{{` and `}}`
//! are literal braces. Commands run through the session's
//! [`CommandRunner`] (inside the execution sandbox under the profile's
//! `process.spawn` grant); a timeout kills the whole process tree, and a
//! non-zero exit, empty output or timeout fails with the stderr tail.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt as _;

use super::audio::tail;

/// Builds the confined command that runs one shell command line.
pub trait CommandRunner: Send + Sync {
    /// A command running `line` through the shell in `cwd`.
    ///
    /// # Errors
    ///
    /// The sandbox or shell cannot be set up.
    fn command(&self, line: &str, cwd: &Path) -> Result<tokio::process::Command, String>;
}

/// Runs command lines through the platform shell without confinement
/// (tests, and hosts without a sandbox).
pub struct PlainShell;

impl CommandRunner for PlainShell {
    fn command(&self, line: &str, cwd: &Path) -> Result<tokio::process::Command, String> {
        let mut command = if cfg!(windows) {
            let mut c = tokio::process::Command::new("cmd");
            c.args(["/C", line]);
            c
        } else {
            let mut c = tokio::process::Command::new("sh");
            c.args(["-c", line]);
            c
        };
        command.current_dir(cwd);
        Ok(command)
    }
}

/// The quoting context of a template position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Quote {
    Bare,
    Single,
    Double,
}

fn quoted(value: &str, quote: Quote) -> String {
    if cfg!(windows) {
        // cmd.exe: double quotes, doubled inside.
        return match quote {
            Quote::Bare => format!("\"{}\"", value.replace('"', "\"\"")),
            Quote::Single | Quote::Double => value.replace('"', "\"\""),
        };
    }
    match quote {
        Quote::Bare => format!("'{}'", value.replace('\'', r"'\''")),
        Quote::Single => value.replace('\'', r"'\''"),
        Quote::Double => value
            .chars()
            .flat_map(|c| match c {
                '\\' | '"' | '$' | '`' => vec!['\\', c],
                c => vec![c],
            })
            .collect(),
    }
}

/// Renders `template` with `values` (`{name}`), each quoted for its
/// context.
///
/// # Errors
///
/// An unknown or unclosed placeholder.
pub fn render(template: &str, values: &BTreeMap<&str, String>) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut quote = Quote::Bare;
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let name: String = chars.by_ref().take_while(|c| *c != '}').collect();
                let value = values
                    .get(name.as_str())
                    .ok_or_else(|| format!("unknown placeholder {{{name}}}"))?;
                out.push_str(&quoted(value, quote));
            }
            '\'' if quote != Quote::Double => {
                quote = if quote == Quote::Single {
                    Quote::Bare
                } else {
                    Quote::Single
                };
                out.push(c);
            }
            '"' if quote != Quote::Single => {
                quote = if quote == Quote::Double {
                    Quote::Bare
                } else {
                    Quote::Double
                };
                out.push(c);
            }
            '\\' if quote != Quote::Single => {
                out.push(c);
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            }
            c => out.push(c),
        }
    }
    Ok(out)
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ran {
    /// Its stdout.
    pub stdout: String,
    /// Its stderr.
    pub stderr: String,
}

/// Runs `line` in `cwd`, killing the process tree when no output arrives
/// for `idle` (an `idle_resets = false` deadline counts from the start).
///
/// # Errors
///
/// It could not start, exited non-zero or timed out; the message carries
/// the stderr tail.
pub async fn run(
    runner: &dyn CommandRunner,
    line: &str,
    cwd: &Path,
    idle: Duration,
    idle_resets: bool,
) -> Result<Ran, String> {
    let mut command = runner.command(line, cwd)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not start: {e}"))?;
    let mut stdout = child.stdout.take().ok_or("no stdout")?;
    let mut stderr = child.stderr.take().ok_or("no stderr")?;
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let (mut out_open, mut err_open) = (true, true);
    let mut deadline = tokio::time::Instant::now() + idle;
    let (mut out_buf, mut err_buf) = ([0u8; 4096], [0u8; 4096]);
    while out_open || err_open {
        tokio::select! {
            read = stdout.read(&mut out_buf), if out_open => match read {
                Ok(0) | Err(_) => out_open = false,
                Ok(n) => { out.extend_from_slice(&out_buf[..n]); if idle_resets { deadline = tokio::time::Instant::now() + idle; } }
            },
            read = stderr.read(&mut err_buf), if err_open => match read {
                Ok(0) | Err(_) => err_open = false,
                Ok(n) => { err.extend_from_slice(&err_buf[..n]); if idle_resets { deadline = tokio::time::Instant::now() + idle; } }
            },
            () = tokio::time::sleep_until(deadline) => {
                kill_tree(&mut child).await;
                let stderr = String::from_utf8_lossy(&err);
                return Err(format!("timed out after {} s without output; stderr: {}", idle.as_secs(), tail(&stderr)));
            }
        }
    }
    let Ok(status) = tokio::time::timeout_at(deadline, child.wait()).await else {
        kill_tree(&mut child).await;
        return Err(format!("timed out after {} s", idle.as_secs()));
    };
    let status = status.map_err(|e| e.to_string())?;
    let ran = Ran {
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
    };
    if status.success() {
        Ok(ran)
    } else {
        Err(format!(
            "exited with {}; stderr: {}",
            status
                .code()
                .map_or_else(|| "a signal".to_owned(), |c| c.to_string()),
            tail(&ran.stderr)
        ))
    }
}

/// Kills the child's whole process group (its own on Unix, the tree on
/// Windows).
async fn kill_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id().and_then(|id| i32::try_from(id).ok()) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let _ = tokio::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .output()
            .await;
    }
    let _ = child.kill().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(pairs: &[(&'static str, &str)]) -> BTreeMap<&'static str, String> {
        pairs.iter().map(|(k, v)| (*k, (*v).to_owned())).collect()
    }

    #[cfg(unix)]
    #[test]
    fn values_are_quoted_for_their_context() {
        let v = values(&[("output_path", "/tmp/my out/a'b.mp3"), ("voice", "x\"$y")]);
        assert_eq!(
            render("mytts --out {output_path}", &v).unwrap(),
            r"mytts --out '/tmp/my out/a'\''b.mp3'"
        );
        assert_eq!(
            render("mytts --out '{output_path}'", &v).unwrap(),
            r"mytts --out '/tmp/my out/a'\''b.mp3'"
        );
        assert_eq!(
            render("mytts --voice \"{voice}\"", &v).unwrap(),
            r#"mytts --voice "x\"\$y""#
        );
        assert_eq!(render("echo {{literal}}", &v).unwrap(), "echo {literal}");
        assert!(render("x {nope}", &v).unwrap_err().contains("{nope}"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_silent_command_is_killed_with_its_children() {
        let dir = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();
        let err = run(
            &PlainShell,
            "sleep 30 & sleep 30; echo late",
            dir.path(),
            Duration::from_millis(300),
            true,
        )
        .await
        .unwrap_err();
        assert!(err.starts_with("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
        let err = run(
            &PlainShell,
            "echo oops >&2; exit 3",
            dir.path(),
            Duration::from_secs(5),
            true,
        )
        .await
        .unwrap_err();
        assert_eq!(err, "exited with 3; stderr: oops");
    }
}
