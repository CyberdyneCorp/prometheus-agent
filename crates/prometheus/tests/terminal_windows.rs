//! The terminal tool inside the enforced Windows sandbox, run by the built
//! binary (which is also the sandbox launcher) with the mock provider
//! (spec: execution-environments, Windows local shell — Sandboxed session
//! on Windows; security-boundaries, OS sandbox per platform). Git Bash
//! cannot start inside an `AppContainer`, so a confined session runs its
//! commands in PowerShell; these tests check that it really works there:
//! output, exit codes, moving around the workspace, running a script, and
//! writes outside the workspace failing.

#![cfg(windows)]

mod common;

use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::Value;

/// A mock home with a workspace (`work`, holding `data.txt`, `sub\` and
/// `script.ps1`) and a directory the sandbox does not grant (`outside`).
struct Fixture {
    home: tempfile::TempDir,
    work: PathBuf,
    outside: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let home = common::mock_home();
        let base = home.path().to_path_buf();
        let work = base.join("work");
        let outside = base.join("outside");
        for dir in [work.join("sub"), outside.clone()] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(work.join("data.txt"), "ledger line\n").unwrap();
        std::fs::write(
            work.join("script.ps1"),
            "Write-Output ('script ran in ' + (Split-Path -Leaf (Get-Location).ProviderPath))\n",
        )
        .unwrap();
        Self {
            home,
            work,
            outside,
        }
    }

    /// Runs `commands` one per model step through the terminal tool with
    /// `terminal.windows_shell = shell` and the default (enforcing) sandbox;
    /// returns each call's result.
    fn terminal(&self, shell: &str, commands: &[String]) -> Vec<Value> {
        self.session(shell, &[], commands)
    }

    /// As [`Fixture::terminal`], with extra `chat` arguments.
    fn session(&self, shell: &str, chat: &[&str], commands: &[String]) -> Vec<Value> {
        let mut script = String::new();
        for (step, command) in commands.iter().enumerate() {
            // A JSON string is also a TOML basic string.
            let command = serde_json::to_string(command).unwrap();
            write!(
                script,
                "[[rule]]\nstep = {step}\n[[rule.tool_calls]]\nname = \"terminal\"\narguments = {{ command = {command} }}\n\n"
            )
            .unwrap();
        }
        writeln!(
            script,
            "[[rule]]\nstep = {}\ntext = \"done\"",
            commands.len()
        )
        .unwrap();
        let path = self.home.path().join("script.toml");
        std::fs::write(&path, script).unwrap();
        let out = common::command(self.home.path())
            .args(["--set", &format!("terminal.windows_shell={shell}")])
            .args(["chat", "-z", "go", "--yolo"])
            .args(chat)
            .arg("--mock-script")
            .arg(&path)
            .arg("--cwd")
            .arg(&self.work)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "stdout {} stderr {}",
            common::stdout(&out),
            common::stderr(&out)
        );
        let results: Vec<Value> = common::active_messages(self.home.path())
            .into_iter()
            .filter(|m| m["role"] == "tool")
            .map(|m| {
                let content = untrusted_body(m["content"].as_str().unwrap_or_default());
                serde_json::from_str(content).unwrap_or_else(|_| Value::from(content))
            })
            .collect();
        assert_eq!(results.len(), commands.len(), "{results:#?}");
        results
    }

    /// The common checks, for a session confined in `shell`.
    fn assert_confined_commands_work(&self, shell: &str) -> Vec<Value> {
        let escaped = self.outside.join("escaped.txt");
        let results = self.terminal(
            shell,
            &[
                "Write-Output héllo".to_owned(),
                "Set-Location sub".to_owned(),
                "Get-Content ..\\data.txt".to_owned(),
                "& ..\\script.ps1".to_owned(),
                format!("Set-Content -LiteralPath '{}' -Value x", escaped.display()),
                "cmd.exe /c exit 3".to_owned(),
            ],
        );
        let [hello, cd, read, script, write, exit] = &results[..] else {
            unreachable!()
        };
        assert_eq!(hello["status"], "ok", "{hello:#}");
        assert_eq!(hello["output"].as_str().unwrap().trim(), "héllo");
        assert_eq!(cd["status"], "ok", "{cd:#}");
        assert_eq!(
            cwd(cd),
            std::fs::canonicalize(self.work.join("sub")).unwrap(),
            "the session follows Set-Location: {cd:#}"
        );
        assert_eq!(
            read["output"].as_str().unwrap().trim(),
            "ledger line",
            "{read:#}"
        );
        assert_eq!(
            script["output"].as_str().unwrap().trim(),
            "script ran in sub",
            "{script:#}"
        );
        assert_eq!(write["status"], "error", "{write:#}");
        assert!(!escaped.exists(), "a write outside the workspace escaped");
        assert_eq!(exit["exit_code"], 3, "{exit:#}");
        results
    }
}

/// The result inside its provenance block (`<untrusted-content ...>` …
/// `</untrusted-content>`, spec: tool-system, Tool result provenance).
fn untrusted_body(content: &str) -> &str {
    content
        .strip_prefix("<untrusted-content")
        .and_then(|rest| rest.split_once('\n'))
        .and_then(|(_, body)| body.rsplit_once("\n</untrusted-content>"))
        .map_or(content, |(body, _)| body)
}

/// The directory a result reports, in canonical form.
fn cwd(result: &Value) -> PathBuf {
    let reported = result["cwd"].as_str().expect("cwd reported");
    std::fs::canonicalize(Path::new(reported)).unwrap()
}

/// Scenario: Sandboxed session on Windows. With Git for Windows installed
/// (as on the CI runners), `auto` uses PowerShell under the sandbox and
/// says why.
#[test]
fn auto_runs_commands_confined_in_powershell() {
    let fixture = Fixture::new();
    let results = fixture.assert_confined_commands_work("auto");
    let notices = results[0]["notices"].to_string();
    assert!(
        notices.contains("Git Bash cannot run inside the Windows sandbox"),
        "{notices}"
    );
}

/// Windows PowerShell 5.1, the shell every Windows host has.
#[test]
fn windows_powershell_runs_commands_confined() {
    Fixture::new().assert_confined_commands_work("powershell");
}

/// An explicit `git-bash` under the sandbox makes the terminal unavailable
/// and doctor says why, instead of running Git Bash unconfined.
#[test]
fn explicit_git_bash_under_the_sandbox_is_unavailable() {
    let fixture = Fixture::new();
    let out = common::run(
        fixture.home.path(),
        &[
            "--set",
            "terminal.windows_shell=git-bash",
            "doctor",
            "terminal",
        ],
    );
    let text = common::stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(
        text.contains("git-bash cannot run inside the Windows sandbox (AppContainer)"),
        "{text}"
    );
}

/// A local HTTP server answering `hello` and counting its connections.
fn server() -> (u16, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&hits);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            counter.fetch_add(1, Ordering::SeqCst);
            let mut buffer = [0u8; 2048];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
            );
        }
    });
    (port, hits)
}

/// The `egress.denied` payloads of the newest session's trajectory.
fn denials(home: &Path) -> Vec<Value> {
    let session = common::newest_session(home);
    common::ok(&common::run(
        home,
        &["trajectory", "show", &session, "--format", "jsonl"],
    ))
    .lines()
    .map(|line| serde_json::from_str::<Value>(line).unwrap())
    .filter(|event| event["type"] == "egress.denied")
    .map(|event| event["payload"].clone())
    .collect()
}

/// Egress of the session terminal (spec: security-boundaries, Per-session
/// network egress policy — Allowlist session; Egress enforcement for child
/// processes). The confined child holds no network capability and reaches
/// the session's egress proxy through the relay in its `AppContainer`: in
/// an allowlist session the allowed host answers, another is refused with
/// an `egress.denied` event, and bypassing the proxy fails; in a public
/// session a public host is reached through the proxy, the metadata
/// endpoint is refused, and a direct connection fails.
#[test]
fn session_children_reach_only_what_the_egress_policy_admits() {
    let fixture = Fixture::new();
    let home = fixture.home.path();
    common::append_config(home, "\n[security.egress]\nallow_private = true\n");
    let (allowed, allowed_hits) = server();
    let (other, other_hits) = server();
    let curl = |extra: &str, url: &str| {
        format!("curl.exe -sS --max-time 20 {extra} {url}; Write-Output \" exit=$LASTEXITCODE\"")
    };
    let allow = format!("127.0.0.1:{allowed}");
    let results = fixture.session(
        "powershell",
        &["--egress", "allowlist", "--allow-host", &allow],
        &[
            curl("", &format!("http://127.0.0.1:{allowed}/")),
            curl("", &format!("http://127.0.0.1:{other}/")),
            curl(
                "--noproxy '*' --connect-timeout 5",
                &format!("http://127.0.0.1:{other}/"),
            ),
        ],
    );
    let output = |results: &[Value], i: usize| {
        results[i]["output"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    assert!(output(&results, 0).starts_with("hello"), "{results:#?}");
    assert!(!output(&results, 1).contains("hello"), "{results:#?}");
    let around = output(&results, 2);
    assert!(
        !around.contains("hello") && !around.ends_with("exit=0"),
        "{results:#?}"
    );
    assert_eq!(allowed_hits.load(Ordering::SeqCst), 1, "{results:#?}");
    assert_eq!(other_hits.load(Ordering::SeqCst), 0, "{results:#?}");
    let denied = denials(home);
    assert!(
        denied.iter().any(|d| d["port"] == other
            && d["reason"] == "not_allowlisted"
            && d["requester"] == "terminal"),
        "{denied:?}"
    );

    let results = fixture.session(
        "powershell",
        &[],
        &[
            curl("", "http://169.254.169.254/latest/meta-data/"),
            // github.com: a public host every CI runner reaches.
            curl("--ssl-no-revoke -o NUL -w '%{http_code}'", "https://github.com/"),
            "try { [Net.Sockets.TcpClient]::new().Connect('github.com', 443); 'connected' } catch { 'refused: ' + $_.Exception.GetBaseException().GetType().Name }".to_owned(),
        ],
    );
    assert!(!output(&results, 0).ends_with("exit=0"), "{results:#?}");
    assert!(output(&results, 1).ends_with("exit=0"), "{results:#?}");
    assert!(output(&results, 1).starts_with('2'), "{results:#?}");
    assert_eq!(
        output(&results, 2),
        "refused: SocketException",
        "{results:#?}"
    );
    let denied = denials(home);
    assert!(
        denied
            .iter()
            .any(|d| d["host"] == "169.254.169.254" && d["reason"] == "metadata"),
        "{denied:?}"
    );
}
