//! Helpers shared by the CLI tests: the built binary run against a
//! temporary home with a clean environment.

#![allow(dead_code, unreachable_pub)]

use std::io::{BufRead as _, Read as _, Write as _};
use std::path::Path;
use std::process::{Command, Output, Stdio};

pub const BIN: &str = env!("CARGO_BIN_EXE_prometheus");

/// A temporary directory under the target dir (never the real home).
pub fn temp_home() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

/// The binary with no inherited Prometheus, editor, log or secret variables.
pub fn command(home: &Path) -> Command {
    let mut cmd = Command::new(BIN);
    for (var, _) in std::env::vars_os() {
        let name = var.to_string_lossy();
        if name.starts_with("PMT_") || name.starts_with("PROMETHEUS_") {
            cmd.env_remove(&var);
        }
    }
    for var in ["VISUAL", "EDITOR", "RUST_LOG"] {
        cmd.env_remove(var);
    }
    // Provider keys a developer machine may export would change routing.
    for var in pmt_providers::builtin::secret_names() {
        cmd.env_remove(var);
    }
    isolate_user_home(&mut cmd, home);
    cmd.arg("--home").arg(home);
    cmd
}

/// The user home every test process sees: a directory inside the test's
/// Prometheus home, so no test reads the developer's own agent CLI
/// logins (`~/.claude`, `~/.codex`) or other dotfiles.
pub fn user_home(home: &Path) -> std::path::PathBuf {
    home.join("user-home")
}

/// Points `HOME` (and `USERPROFILE`) of `cmd` at [`user_home`].
pub fn isolate_user_home(cmd: &mut Command, home: &Path) {
    let user = user_home(home);
    std::fs::create_dir_all(&user).unwrap();
    cmd.env("HOME", &user).env("USERPROFILE", &user);
}

/// Runs `args` with stdin closed.
pub fn run(home: &Path, args: &[&str]) -> Output {
    command(home)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

/// Runs `args` with `input` on stdin (a pipe, never a terminal).
pub fn run_with_stdin(home: &Path, args: &[&str], input: &str) -> Output {
    run_cmd_with_stdin(command(home).args(args), input)
}

/// Runs a prepared command with `input` on stdin.
pub fn run_cmd_with_stdin(cmd: &mut Command, input: &str) -> Output {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let written = child.stdin.take().unwrap().write_all(input.as_bytes());
    // A command that refuses its arguments exits before reading stdin; the
    // pipe is then closed, which is not a test failure.
    if let Err(err) = written {
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe, "{err}");
    }
    child.wait_with_output().unwrap()
}

pub fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

/// Asserts success and returns stdout.
pub fn ok(out: &Output) -> String {
    assert!(
        out.status.success(),
        "exit {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        stdout(out),
        stderr(out)
    );
    stdout(out)
}

/// Every file under `dir`, recursively.
pub fn files_under(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found
}

#[cfg(unix)]
pub mod pty;

#[cfg(unix)]
pub mod tui;

#[cfg(feature = "gateway")]
pub mod daemon;

#[cfg(feature = "gateway")]
pub mod gateway;

/// A 1x1 PNG.
pub const PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 218, 99, 100, 96, 248, 95, 15, 0, 2,
    135, 1, 128, 235, 71, 186, 146, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

/// A home whose default profile talks to the mock provider.
pub fn mock_home() -> tempfile::TempDir {
    let home = temp_home();
    ok(&run(
        home.path(),
        &["config", "set", "model.provider", "mock"],
    ));
    ok(&run(
        home.path(),
        &["config", "set", "model.default", "mock"],
    ));
    home
}

/// Appends `text` to the default profile's `config.toml`.
pub fn append_config(home: &Path, text: &str) {
    let path = home.join("profiles/default/config.toml");
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(text.as_bytes()).unwrap();
}

/// The newest session's id.
pub fn newest_session(home: &Path) -> String {
    let listed = ok(&run(home, &["--json", "sessions", "list", "--all"]));
    let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
    listed["sessions"][0]["id"].as_str().unwrap().to_owned()
}

/// The messages of session `id` (`sessions export`), active and not.
pub fn session_messages(home: &Path, id: &str) -> Vec<serde_json::Value> {
    ok(&run(
        home,
        &["sessions", "export", id, "--include-inactive"],
    ))
    .lines()
    .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
    .filter_map(|line| line.get("message").cloned())
    .collect()
}

/// The active messages of the newest session.
pub fn active_messages(home: &Path) -> Vec<serde_json::Value> {
    session_messages(home, &newest_session(home))
        .into_iter()
        .filter(|m| m["active"] == true)
        .collect()
}

/// A fake installed version `dir` of `name` whose record matches `files`.
pub fn fake_runtime(
    home: &Path,
    name: &str,
    dir: &str,
    files: &[(&str, &[u8])],
) -> std::path::PathBuf {
    let root = home.join("runtimes").join(name).join(dir);
    let mut digests = std::collections::BTreeMap::new();
    for (path, bytes) in files {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, bytes).unwrap();
        digests.insert((*path).to_owned(), pmt_update::manifest::sha256_hex(bytes));
    }
    let record = serde_json::json!({
        "schema": "prometheus.runtime.v1", "name": name, "version": dir.split('-').next().unwrap(),
        "sha256": "0".repeat(64), "url": "https://example.invalid/a.tar.gz", "source": "origin",
        "installed_at": "2026-10-31T00:00:00Z", "retries": 0, "resumed": false,
        "exe": files[0].0, "files": digests,
    });
    std::fs::write(
        root.join(pmt_update::runtimes::RECORD_FILE),
        record.to_string(),
    )
    .unwrap();
    root
}

/// Reads one HTTP request (headers, then a sized or chunked body) and
/// returns its body, so a fake server never closes the connection under
/// a body the client is still sending.
pub fn read_http_request(reader: &mut impl std::io::BufRead) -> Vec<u8> {
    let (mut length, mut chunked) = (0, false);
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        let line = line.to_ascii_lowercase();
        if let Some(value) = line.strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
        chunked |= line.starts_with("transfer-encoding:") && line.contains("chunked");
    }
    let mut body = Vec::new();
    if !chunked {
        body.resize(length, 0);
        let _ = reader.read_exact(&mut body);
        return body;
    }
    loop {
        let mut size = String::new();
        if reader.read_line(&mut size).unwrap_or(0) == 0 {
            return body;
        }
        let size = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
        let mut chunk = vec![0; size + 2];
        let _ = reader.read_exact(&mut chunk);
        if size == 0 {
            return body;
        }
        body.extend_from_slice(&chunk[..size]);
    }
}

/// Waits (at most 30 s) until a turn has started in the default profile:
/// its `turn.start` event is in a trajectory file. Tests that signal a
/// running one-shot turn wait on this instead of sleeping. A finished
/// session's trajectory is compressed (`.jsonl.zst`), so turns of runs that
/// already ended are not counted.
pub fn wait_for_turn_start(home: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while turns_started(home) == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "no turn started within 30 s"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// How many turns have started in the default profile (`turn.start`
/// events across its live trajectory files).
pub fn turns_started(home: &Path) -> usize {
    std::fs::read_dir(home.join("profiles/default/trajectories"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .map(|text| text.matches("\"turn.start\"").count())
        .sum()
}

/// Runs a browser sign-in: reads stderr until the authorization URL,
/// answers its callback like the browser would, and returns stderr.
pub fn browser_sign_in(cmd: &mut std::process::Command) -> (Option<i32>, String) {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = std::io::BufReader::new(child.stderr.take().unwrap()).lines();
    let mut seen = String::new();
    let mut url = None;
    for line in lines.by_ref() {
        let line = line.unwrap();
        seen.push_str(&line);
        seen.push('\n');
        if seen.contains("Open this URL") && line.starts_with("  http") {
            url = Some(line.trim().to_owned());
        }
        if line.starts_with("Waiting for the callback") {
            break;
        }
    }
    let url = url::Url::parse(&url.unwrap_or_else(|| panic!("no URL in:\n{seen}"))).unwrap();
    let query: std::collections::BTreeMap<String, String> =
        url.query_pairs().into_owned().collect();
    let redirect = url::Url::parse(&query["redirect_uri"]).unwrap();
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", redirect.port().unwrap())).unwrap();
    write!(
        stream,
        "GET {}?code=browser-code&state={} HTTP/1.1\r\nhost: localhost\r\n\r\n",
        redirect.path(),
        query["state"]
    )
    .unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    for line in lines {
        seen.push_str(&line.unwrap());
        seen.push('\n');
    }
    (child.wait().unwrap().code(), seen)
}
