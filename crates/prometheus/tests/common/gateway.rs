//! The messaging gateway inside the real daemon, driven through the
//! debug-build file adapter (`PMT_TEST_ADAPTER_DIR`, platform `test`):
//! the inbox the platform "receives", what the adapter sent, and a mock
//! home answering on that platform.

use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use pmt_rpc::discovery::{Discovery, discover};
use serde_json::{Value, json};

use super::{command, mock_home, ok, run};

/// A foreground daemon (`daemon run`) with the file adapter.
pub struct Daemon {
    child: Child,
}

impl Daemon {
    pub fn start(home: &Path) -> Self {
        let child = command(home)
            .args(["daemon", "run"])
            .env("PMT_TEST_ADAPTER_DIR", adapter_dir(home))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait("the daemon runs", Duration::from_secs(30), || {
            matches!(discover(home), Ok(Discovery::Running(_)))
        });
        Self { child }
    }

    /// SIGKILL (`TerminateProcess` on Windows): nothing runs after it.
    pub fn kill(mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }

    /// A graceful stop; returns the exit code.
    pub fn stop(mut self, home: &Path) -> Option<i32> {
        ok(&run(home, &["daemon", "stop"]));
        self.child.wait().unwrap().code()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn adapter_dir(home: &Path) -> PathBuf {
    home.join("adapter")
}

pub fn wait(what: &str, limit: Duration, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Drops a message into the profile's inbox.
pub fn send(home: &Path, profile: &str, sender: &str, text: &str) {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let inbox = adapter_dir(home).join(profile).join("inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    let message = json!({"id": format!("m{n}"), "sender": sender, "chat": sender,
        "chat_type": "dm", "text": text});
    let tmp = inbox.join(format!("{n:06}.tmp"));
    std::fs::write(&tmp, message.to_string()).unwrap();
    std::fs::rename(&tmp, inbox.join(format!("{n:06}.json"))).unwrap();
}

/// Drops a message of `sender` in chat `chat`, thread `thread`.
pub fn send_in(home: &Path, profile: &str, sender: &str, chat: &str, thread: &str, text: &str) {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(500_000);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let inbox = adapter_dir(home).join(profile).join("inbox");
    std::fs::create_dir_all(&inbox).unwrap();
    let message = json!({"id": format!("m{n}"), "sender": sender, "chat": chat,
        "chat_type": "dm", "thread": thread, "text": text});
    let tmp = inbox.join(format!("{n:06}.tmp"));
    std::fs::write(&tmp, message.to_string()).unwrap();
    std::fs::rename(&tmp, inbox.join(format!("{n:06}.json"))).unwrap();
}

/// What the profile's adapter sent, in order.
pub fn sent(home: &Path, profile: &str) -> Vec<Value> {
    std::fs::read_to_string(adapter_dir(home).join(profile).join("sent.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

pub fn texts(home: &Path, profile: &str) -> Vec<String> {
    sent(home, profile)
        .iter()
        .filter(|v| v["kind"] == "send")
        .map(|v| v["text"].as_str().unwrap_or_default().to_owned())
        .collect()
}

pub fn wait_said(home: &Path, profile: &str, text: &str) {
    wait_said_within(home, profile, text, Duration::from_mins(1));
}

/// [`wait_said`] with a budget of its own, for a step that takes longer
/// on a slow runner (an update's download, self-test and handover).
pub fn wait_said_within(home: &Path, profile: &str, text: &str, budget: Duration) {
    let deadline = Instant::now() + budget;
    while !texts(home, profile).iter().any(|t| t.contains(text)) {
        assert!(
            Instant::now() < deadline,
            "{profile} never said {text:?}; it said {:?}",
            texts(home, profile)
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A mock home whose default profile answers on the `test` platform
/// with the mock `rules` (TOML `[[rule]]` entries), plus `extra` config.
pub fn gateway_home_with(rules: &str, extra: &str) -> tempfile::TempDir {
    let home = mock_home();
    let profile = home.path().join("profiles/default");
    std::fs::write(profile.join("mock.toml"), rules).unwrap();
    super::append_config(
        home.path(),
        &format!(
            "\n[providers.mock]\nscript = \"mock.toml\"\n\n[gateway.busy]\ndebounce_ms = 0\n\n\
             [gateway.platforms.test]\nenabled = true\nallow_all = true\n{extra}"
        ),
    );
    home
}

/// A mock home whose default profile answers on the `test` platform:
/// `slow` takes a minute, anything else answers `fast done`.
pub fn gateway_home(extra: &str) -> tempfile::TempDir {
    let home = mock_home();
    let profile = home.path().join("profiles/default");
    std::fs::write(
        profile.join("mock.toml"),
        // Only a request whose user text is exactly `slow` is slow: after a
        // crash, the stored `slow` is merged with the next message.
        "[[rule]]\nuser_regex = '^slow$'\nrepeat = true\nlatency_ms = 60000\ntext = \"slow done\"\n\n\
         [[rule]]\nrepeat = true\ntext = \"fast done\"\n",
    )
    .unwrap();
    super::append_config(
        home.path(),
        &format!(
            "\n[providers.mock]\nscript = \"mock.toml\"\n\n[gateway.busy]\ndebounce_ms = 0\n\n\
             [gateway.platforms.test]\nenabled = true\nallow_all = true\n{extra}"
        ),
    );
    home
}
