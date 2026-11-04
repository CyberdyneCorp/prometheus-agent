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
use std::path::{Path, PathBuf};
use std::process::Stdio;

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
                let content = m["content"].as_str().unwrap_or_default();
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

/// Egress of the session terminal (spec: security-boundaries, Egress
/// enforcement for child processes). The `AppContainer` cannot reach the
/// loopback egress proxy, so no proxy variables point there; a child
/// connects directly to public addresses (`internetClient`, S-1-15-3-1)
/// only under the default `public` policy, an allowlisted session's child
/// gets no network at all, and neither reaches loopback.
#[test]
fn session_children_reach_only_what_the_egress_policy_admits() {
    let fixture = Fixture::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let commands = [
        "whoami /groups".to_owned(),
        "Write-Output \"proxy=[$env:HTTPS_PROXY]\"".to_owned(),
        format!(
            "try {{ (New-Object Net.Sockets.TcpClient).Connect('127.0.0.1', {port}); 'connected' }} catch {{ 'refused' }}"
        ),
    ];
    let public = fixture.session("powershell", &[], &commands);
    let allowlisted = fixture.session(
        "powershell",
        &["--egress", "allowlist", "--allow-host", "127.0.0.1"],
        &commands,
    );
    for (results, internet) in [(&public, true), (&allowlisted, false)] {
        let output = |i: usize| results[i]["output"].as_str().unwrap_or_default().to_owned();
        assert_eq!(
            output(0).split_whitespace().any(|w| w == "S-1-15-3-1"),
            internet,
            "{results:#?}"
        );
        assert_eq!(output(1).trim(), "proxy=[]", "{results:#?}");
        assert_eq!(output(2).trim(), "refused", "{results:#?}");
    }
    assert!(
        listener.accept().is_err(),
        "a session child reached loopback"
    );
}
