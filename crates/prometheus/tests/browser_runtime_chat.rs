//! The browser tools of a chat session over the managed runtime store
//! (spec: distribution-and-update, Lazy runtime installation policy —
//! Lazy install disabled in an image; Persistent runtime opt-outs —
//! Browser opted out at install time), against the built binary with the
//! mock provider and a local runtime mirror that serves nothing, so any
//! download attempt shows up as a request.
//!
//! Linux only: without Chromium in the store, the runtime policy uses a
//! Chromium-family browser of the host, found on `PATH` (which these tests
//! control) and, on macOS and Windows, at fixed install locations (which
//! they cannot), so only Linux can stage a host without one.
#![cfg(target_os = "linux")]

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use serde_json::Value;
use wiremock::MockServer;

/// What `browser_exec` answers when Chromium is missing and lazy installs
/// are disabled.
const LAZY_DISABLED: &str = "chromium is not installed and lazy installs are disabled: run 'prometheus runtimes install chromium'";

struct Fixture {
    home: tempfile::TempDir,
    /// The session's working directory.
    work: PathBuf,
    /// The only directory on the session's `PATH`.
    bin: PathBuf,
    server: MockServer,
    runtime: tokio::runtime::Runtime,
}

impl Fixture {
    /// A home on the mock provider whose `daemon.toml` holds `runtimes`
    /// (keys of `[runtimes]`) and points runtime downloads at the mirror.
    fn new(runtimes: &str) -> Self {
        let home = common::temp_home();
        for (key, value) in [("model.provider", "mock"), ("model.default", "mock")] {
            common::ok(&common::run(home.path(), &["config", "set", key, value]));
        }
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let server = runtime.block_on(MockServer::start());
        std::fs::write(
            home.path().join("daemon.toml"),
            format!(
                "[runtimes]\n{runtimes}\n[runtimes.mirrors]\nruntimes = \"{}/rt\"\n",
                server.uri()
            ),
        )
        .unwrap();
        let work = home.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        let work = work.canonicalize().unwrap();
        let bin = work.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for shell in ["sh", "bash"] {
            let target = Path::new("/bin").join(shell);
            if target.exists() {
                std::os::unix::fs::symlink(target, bin.join(shell)).unwrap();
            }
        }
        Self {
            home,
            work,
            bin,
            server,
            runtime,
        }
    }

    /// An executable named `name` on the session's `PATH` (never run).
    fn host_program(&self, name: &str) -> PathBuf {
        let path = self.bin.join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A one-shot `browse` run with the terminal and browser toolsets.
    fn chat(&self, script: &str) -> Value {
        let path = self.home.path().join("script.toml");
        std::fs::write(&path, script).unwrap();
        let out: Output = common::command(self.home.path())
            .current_dir(&self.work)
            .env("PATH", &self.bin)
            .args([
                "--json",
                "-z",
                "browse",
                "--yolo",
                "--toolsets",
                "terminal,browser",
            ])
            .arg("--mock-script")
            .arg(&path)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        serde_json::from_str(common::stdout(&out).trim()).unwrap_or_else(|e| {
            panic!(
                "{e}: stdout {} stderr {}",
                common::stdout(&out),
                common::stderr(&out)
            )
        })
    }

    /// How the session's frozen surface lists `browser_exec`.
    fn browser_exposure(&self, session: &Value) -> String {
        let id = session["session_id"].as_str().unwrap();
        let doc: Value = serde_json::from_str(&common::ok(&common::run(
            self.home.path(),
            &["--json", "tools", "list", "--session", id],
        )))
        .unwrap();
        doc["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == "browser_exec")
            .map(|row| row["exposure"].as_str().unwrap().to_owned())
            .unwrap()
    }

    fn runtime_requests(&self) -> Vec<String> {
        self.runtime
            .block_on(self.server.received_requests())
            .unwrap_or_default()
            .iter()
            .map(|r| r.url.path().to_owned())
            .collect()
    }

    fn nothing_downloaded(&self) {
        assert_eq!(self.runtime_requests(), Vec::<String>::new());
        assert!(!self.home.path().join("runtimes/chromium").exists());
    }
}

/// The model calls `browser_exec` once; the answer says what it got.
const CALL_BROWSER: &str = r#"
[[rule]]
user = "browse"
[[rule.tool_calls]]
name = "browser_exec"
arguments = { code = "print(page_info())" }

[[rule]]
tool_result = "unknown tool: browser_exec"
text = "not offered"

[[rule]]
repeat = true
text = "other"
"#;

/// Scenario: Lazy install disabled in an image.
#[test]
fn with_lazy_installs_disabled_the_browser_is_unavailable_and_nothing_downloads() {
    let f = Fixture::new("allow_lazy_install = false");
    let session = f.chat(CALL_BROWSER);
    assert_eq!(session["answer"], "not offered");
    assert_eq!(
        f.browser_exposure(&session),
        "unavailable:lazy installs disabled"
    );
    f.nothing_downloaded();
}

/// Scenario: Browser opted out at install time (the session half).
#[test]
fn an_excluded_browser_is_unavailable_and_nothing_downloads() {
    let f = Fixture::new("exclude = [\"chromium\"]");
    let session = f.chat(CALL_BROWSER);
    assert_eq!(session["answer"], "not offered");
    assert_eq!(f.browser_exposure(&session), "unavailable:excluded");
    f.nothing_downloaded();
}

/// The browser was offered (a host browser existed at session start) and
/// is gone by the call: the call fails with the spec message and nothing
/// is downloaded.
#[test]
fn a_call_that_would_need_a_lazy_install_fails_with_the_spec_message() {
    let f = Fixture::new("allow_lazy_install = false");
    let chromium = f.host_program("chromium");
    f.host_program("python3");
    let session = f.chat(&format!(
        r#"
[[rule]]
user = "browse"
[[rule.tool_calls]]
name = "terminal"
arguments = {{ command = "/bin/rm -f {chromium}" }}

[[rule]]
tool_result = "{LAZY_DISABLED}"
text = "refused"

[[rule]]
user = "browse"
[[rule.tool_calls]]
name = "browser_exec"
arguments = {{ code = "print(page_info())" }}

[[rule]]
repeat = true
text = "other"
"#,
        chromium = chromium.display()
    ));
    assert!(!chromium.exists(), "the terminal call removed it");
    assert_eq!(f.browser_exposure(&session), "exposed");
    assert_eq!(session["answer"], "refused");
    f.nothing_downloaded();
}

/// The control: with lazy installs allowed, a non-interactive session
/// installs Chromium on first use without asking, from the mirror.
#[test]
fn a_missing_browser_is_installed_on_first_use() {
    let f = Fixture::new("allow_lazy_install = true");
    let session = f.chat(CALL_BROWSER);
    assert_eq!(f.browser_exposure(&session), "exposed");
    let index = pmt_update::runtimes::index::RuntimeIndex::embedded();
    let pin = index.pin("chromium").unwrap();
    // Hosts without a pinned Chromium build (musl) cannot install one.
    if pin
        .artifacts
        .contains_key(&pmt_update::runtimes::platform())
    {
        let wanted = format!("/rt/chromium/{}/", pin.version);
        let requests = f.runtime_requests();
        assert!(
            requests.iter().any(|p| p.starts_with(&wanted)),
            "{requests:?}"
        );
    }
}

/// An interactive session asks `Install chromium <version> (<size>)?
/// [Y/n]` before a lazy install; `n` declines and nothing is downloaded.
#[test]
fn an_interactive_session_asks_before_a_lazy_install() {
    use expectrl::Expect as _;

    let f = Fixture::new("allow_lazy_install = true");
    let index = pmt_update::runtimes::index::RuntimeIndex::embedded();
    let pin = index.pin("chromium").unwrap();
    if !pin
        .artifacts
        .contains_key(&pmt_update::runtimes::platform())
    {
        // No Chromium build is pinned for this host (musl): nothing to ask.
        return;
    }
    let script = f.home.path().join("script.toml");
    std::fs::write(
        &script,
        r#"
[[rule]]
tool_result = "chromium was not installed"
text = "declined"

[[rule]]
user = "browse"
[[rule.tool_calls]]
name = "browser_exec"
arguments = { code = "print(page_info())" }

[[rule]]
repeat = true
text = "other"
"#,
    )
    .unwrap();
    let script = script.to_string_lossy().into_owned();
    let path = f.bin.to_string_lossy().into_owned();
    let mut p = common::pty::spawn_with(
        f.home.path(),
        &["--mock-script", &script, "--toolsets", "terminal,browser"],
        &[("PATH", &path)],
    );
    common::pty::prompt(&mut p);
    p.send_line("browse").unwrap();
    p.expect(format!("Install chromium {} (", pin.version).as_str())
        .unwrap();
    p.expect(")? [Y/n]").unwrap();
    p.send_line("n").unwrap();
    p.expect("declined").unwrap();
    common::pty::prompt(&mut p);
    common::pty::exit(p);
    f.nothing_downloaded();
}
