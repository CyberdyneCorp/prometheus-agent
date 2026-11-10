//! Command-template speech providers (spec: web-and-media-tools,
//! Command-template speech providers).
//!
//! `[tts.providers.<name>]` and `[stt.providers.<name>]` run a shell
//! command whose `{placeholders}` are replaced by values quoted for the
//! quoting context they appear in (bare, inside single quotes, inside
//! double quotes) in the language of the shell that runs the line (POSIX
//! sh, PowerShell or cmd.exe: [`CommandRunner::syntax`]), so a path with
//! spaces stays one argument and no value is expanded; `{{` and `}}` are
//! literal braces. Commands run through the session's
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
    /// The language of the shell [`command`](Self::command) runs lines in.
    fn syntax(&self) -> ShellSyntax;

    /// A command running `line` through the shell in `cwd`.
    ///
    /// # Errors
    ///
    /// The sandbox or shell cannot be set up.
    fn command(&self, line: &str, cwd: &Path) -> Result<tokio::process::Command, String>;
}

/// The quoting rules of the shell a command line runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellSyntax {
    /// `sh` and `bash`.
    Posix,
    /// PowerShell (Windows PowerShell 5 and `pwsh`).
    PowerShell,
    /// `cmd.exe /C`.
    Cmd,
}

/// Runs command lines through the platform shell without confinement
/// (tests, and hosts without a sandbox).
pub struct PlainShell;

impl CommandRunner for PlainShell {
    fn syntax(&self) -> ShellSyntax {
        if cfg!(windows) {
            ShellSyntax::Cmd
        } else {
            ShellSyntax::Posix
        }
    }

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

/// PowerShell treats these typographic quotes like `'` and `"`.
const PS_SINGLE: [char; 5] = ['\'', '\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'];
const PS_DOUBLE: [char; 4] = ['"', '\u{201C}', '\u{201D}', '\u{201E}'];

impl ShellSyntax {
    /// `value` as a literal for a `quote` position: a bare value becomes
    /// one quoted word; inside quotes only what would end the quotes or
    /// expand is escaped.
    fn quote(self, value: &str, quote: Quote) -> String {
        match self {
            Self::Posix => posix(value, quote),
            Self::PowerShell => powershell(value, quote),
            Self::Cmd => cmd(value, quote),
        }
    }

    /// Whether `c` starts a single-quoted string here.
    fn single_quote(self, c: char) -> bool {
        match self {
            Self::Posix => c == '\'',
            Self::PowerShell => PS_SINGLE.contains(&c),
            Self::Cmd => false,
        }
    }

    /// Whether `c` starts a double-quoted string here.
    fn double_quote(self, c: char) -> bool {
        match self {
            Self::PowerShell => PS_DOUBLE.contains(&c),
            Self::Posix | Self::Cmd => c == '"',
        }
    }

    /// Whether `c` escapes the next template character in `quote`.
    fn escapes(self, c: char, quote: Quote) -> bool {
        match self {
            Self::Posix => c == '\\' && quote != Quote::Single,
            Self::PowerShell => c == '`' && quote != Quote::Single,
            Self::Cmd => c == '^' && quote == Quote::Bare,
        }
    }

    /// The quoting context after the template character `c`.
    fn after(self, c: char, quote: Quote) -> Quote {
        match quote {
            Quote::Bare if self.single_quote(c) => Quote::Single,
            Quote::Bare if self.double_quote(c) => Quote::Double,
            Quote::Single if self.single_quote(c) => Quote::Bare,
            Quote::Double if self.double_quote(c) => Quote::Bare,
            other => other,
        }
    }
}

/// POSIX sh: single quotes are literal (a `'` is closed, escaped and
/// reopened); inside double quotes `\`, `"`, `$` and `` ` `` are escaped.
fn posix(value: &str, quote: Quote) -> String {
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

/// PowerShell: single-quoted strings are literal (each single quote,
/// typographic ones too, is doubled); inside double quotes the backtick
/// escapes `` ` ``, `$` and the double quotes.
fn powershell(value: &str, quote: Quote) -> String {
    let single = |value: &str| {
        value
            .chars()
            .flat_map(|c| {
                if PS_SINGLE.contains(&c) {
                    vec![c, c]
                } else {
                    vec![c]
                }
            })
            .collect::<String>()
    };
    match quote {
        Quote::Bare => format!("'{}'", single(value)),
        Quote::Single => single(value),
        Quote::Double => value
            .chars()
            .flat_map(|c| {
                if c == '`' || c == '$' || PS_DOUBLE.contains(&c) {
                    vec!['`', c]
                } else {
                    vec![c]
                }
            })
            .collect(),
    }
}

/// cmd.exe: inside double quotes `&|<>^` are literal and a `"` is
/// doubled; `%` would still expand, so it is written outside the quotes
/// as `^%` (`"a"^%"b"` reaches the program as `a%b`).
fn cmd(value: &str, quote: Quote) -> String {
    let inner = value.replace('"', "\"\"").replace('%', "\"^%\"");
    match quote {
        Quote::Bare => format!("\"{inner}\""),
        // cmd has no single quotes: a `Single` position never occurs.
        Quote::Single | Quote::Double => inner,
    }
}

/// Renders `template` with `values` (`{name}`), each quoted for its
/// context in `syntax`.
///
/// # Errors
///
/// An unknown or unclosed placeholder.
pub fn render(
    template: &str,
    values: &BTreeMap<&str, String>,
    syntax: ShellSyntax,
) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut quote = Quote::Bare;
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if matches!(c, '{' | '}') && chars.next_if_eq(&c).is_some() {
            out.push(c);
        } else if c == '{' {
            let name: String = chars.by_ref().take_while(|c| *c != '}').collect();
            let value = values
                .get(name.as_str())
                .ok_or_else(|| format!("unknown placeholder {{{name}}}"))?;
            out.push_str(&syntax.quote(value, quote));
        } else if syntax.escapes(c, quote) {
            out.push(c);
            out.extend(chars.next());
        } else {
            out.push(c);
            quote = syntax.after(c, quote);
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

    #[test]
    fn posix_values_are_quoted_for_their_context() {
        let v = values(&[
            ("output_path", "/tmp/my out/a'b.mp3"),
            ("voice", "x\"$y`z\\"),
        ]);
        let render = |t: &str| render(t, &v, ShellSyntax::Posix).unwrap();
        assert_eq!(
            render("mytts --out {output_path}"),
            r"mytts --out '/tmp/my out/a'\''b.mp3'"
        );
        assert_eq!(
            render("mytts --out '{output_path}'"),
            r"mytts --out '/tmp/my out/a'\''b.mp3'"
        );
        assert_eq!(
            render("mytts --voice \"{voice}\""),
            r#"mytts --voice "x\"\$y\`z\\""#
        );
        assert_eq!(render(r"echo \'{voice}"), r#"echo \''x"$y`z\'"#);
        assert_eq!(render("echo {{literal}}"), "echo {literal}");
        assert!(
            super::render("x {nope}", &v, ShellSyntax::Posix)
                .unwrap_err()
                .contains("{nope}")
        );
    }

    #[test]
    fn powershell_values_are_literal_in_every_context() {
        let v = values(&[
            ("output_path", r"C:\my out\it's ‘q’.mp3"),
            ("voice", "a$env:HOME `n \"b\" “c”"),
        ]);
        let render = |t: &str| render(t, &v, ShellSyntax::PowerShell).unwrap();
        assert_eq!(
            render("mytts --out {output_path}"),
            r"mytts --out 'C:\my out\it''s ‘‘q’’.mp3'"
        );
        assert_eq!(
            render("mytts --out '{output_path}'"),
            r"mytts --out 'C:\my out\it''s ‘‘q’’.mp3'"
        );
        assert_eq!(
            render("mytts --voice \"{voice}\""),
            "mytts --voice \"a`$env:HOME ``n `\"b`\" `“c`”\""
        );
        // A backtick escapes the next template character, not a quote.
        assert_eq!(
            render("echo `\"{voice}"),
            "echo `\"'a$env:HOME `n \"b\" “c”'"
        );
        // A single-quoted template string is closed by its own `''`.
        assert!(render("echo 'x''y' {voice}").ends_with("'a$env:HOME `n \"b\" “c”'"));
    }

    #[test]
    fn cmd_values_keep_metacharacters_and_percent_literal() {
        let v = values(&[
            ("output_path", r"C:\my out\50%PATH%&x.mp3"),
            ("voice", "say \"hi\""),
        ]);
        let render = |t: &str| render(t, &v, ShellSyntax::Cmd).unwrap();
        assert_eq!(
            render("mytts --out {output_path}"),
            r#"mytts --out "C:\my out\50"^%"PATH"^%"&x.mp3""#
        );
        assert_eq!(
            render("mytts --voice \"{voice}\""),
            r#"mytts --voice "say ""hi""""#
        );
        // A single quote is an ordinary character in cmd.
        assert_eq!(render("echo '{voice}'"), r#"echo '"say ""hi"""'"#);
        assert_eq!(render("echo ^\"{voice}"), r#"echo ^""say ""hi""""#);
    }

    /// A rendered line run by the real shell hands the engine each value
    /// unchanged, as one argument.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_rendered_posix_line_passes_values_through_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let value = "it's a \"$HOME\" `x` \\ path";
        let v = values(&[("v", value)]);
        for template in [
            "printf '%s|' {v}",
            "printf '%s|' '{v}'",
            "printf '%s|' \"{v}\"",
        ] {
            let line = render(template, &v, ShellSyntax::Posix).unwrap();
            let ran = run(&PlainShell, &line, dir.path(), Duration::from_secs(5), true)
                .await
                .unwrap();
            assert_eq!(ran.stdout, format!("{value}|"), "{template}");
        }
    }

    /// The same through Windows PowerShell, the shell Windows sessions
    /// run command providers in.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_rendered_powershell_line_passes_values_through_unchanged() {
        struct Pwsh;
        impl CommandRunner for Pwsh {
            fn syntax(&self) -> ShellSyntax {
                ShellSyntax::PowerShell
            }
            fn command(&self, line: &str, cwd: &Path) -> Result<tokio::process::Command, String> {
                let mut c = tokio::process::Command::new("powershell");
                c.args(["-NoProfile", "-NonInteractive", "-Command", line])
                    .current_dir(cwd);
                Ok(c)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        // ASCII only: the console's code page would mangle other output.
        let value = "it's \"a\" $HOME `n path";
        let v = values(&[("v", value)]);
        for template in [
            "[Console]::Out.Write({v})",
            "[Console]::Out.Write('{v}')",
            "[Console]::Out.Write(\"{v}\")",
        ] {
            let line = render(template, &v, ShellSyntax::PowerShell).unwrap();
            let ran = run(&Pwsh, &line, dir.path(), Duration::from_secs(60), true)
                .await
                .unwrap();
            assert_eq!(ran.stdout, value, "{template}");
        }
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
