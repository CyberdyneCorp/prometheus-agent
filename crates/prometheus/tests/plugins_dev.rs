//! `prometheus plugins dev <path> --watch` (spec: plugin-system, Plugin
//! development loop): a local plugin directory is served as `dev:<id>`
//! without installing it; a source change rebuilds it and a running
//! `prometheus chat` handles its next event with the new hook, while cron
//! runs never load it.

#![cfg(unix)]

mod common;

use std::io::{BufRead as _, BufReader};
use std::path::Path;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{mock_home, ok, run};
use expectrl::session::OsSession;
use expectrl::{Eof, Expect};
use serde_json::Value;

const RULES: &str = r#"
[[rule]]
user = "Answer briefly"
repeat = true
text = "old hook answered"

[[rule]]
user = "NEW HOOK ACTIVE"
repeat = true
text = "new hook answered"

[[rule]]
repeat = true
text = "no hook"
"#;

/// A plugin directory whose `build` copies `<variant>.wasm` to
/// `plugin.wasm`: `a` is the sample hook (context "Answer briefly."),
/// `b` the test hook answering the context its settings name.
fn plugin(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("plugin.toml"),
        "manifest_version = 1\nid = \"devhook\"\nversion = \"0.1.0\"\nkind = \"wasm\"\n\
         description = \"dev loop\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
         worlds = [\"hook@1.1\"]\nbuild = \"sh build.sh\"\n\n[capabilities]\nhooks = [\"pre_llm_call\"]\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("build.sh"),
        "cp \"$(cat variant).wasm\" plugin.wasm\n",
    )
    .unwrap();
    std::fs::write(dir.join("variant"), "a\n").unwrap();
    std::fs::write(
        dir.join("a.wasm"),
        pmt_plugins::testkit::component("sample-hook"),
    )
    .unwrap();
    std::fs::write(
        dir.join("b.wasm"),
        pmt_plugins::testkit::component("testing-hooks"),
    )
    .unwrap();
}

/// The dev command, its stdout lines collected as they come.
struct Dev {
    child: Child,
    lines: Arc<Mutex<Vec<String>>>,
}

impl Dev {
    fn start(home: &Path, dir: &Path) -> Self {
        let mut child = common::command(home)
            .args(["plugins", "dev"])
            .arg(dir)
            .args(["--watch", "--grant", "all"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let lines = Arc::new(Mutex::new(Vec::new()));
        let out = child.stdout.take().unwrap();
        let sink = Arc::clone(&lines);
        std::thread::spawn(move || {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                sink.lock().unwrap().push(line);
            }
        });
        Self { child, lines }
    }

    fn wait_for(&self, text: &str) {
        let deadline = Instant::now() + Duration::from_mins(1);
        while Instant::now() < deadline {
            if self.lines.lock().unwrap().iter().any(|l| l.contains(text)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "the dev command never printed {text:?}: {:?}",
            self.lines.lock().unwrap()
        );
    }
}

impl Dev {
    /// Ctrl+C: the command ends on its own.
    fn interrupt(mut self) {
        let pid = self.child.id().to_string();
        let sent = std::process::Command::new("kill")
            .args(["-INT", &pid])
            .status()
            .unwrap();
        assert!(sent.success());
        let status = self.child.wait().unwrap();
        assert!(status.success(), "{status}");
    }
}

impl Drop for Dev {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn chat(home: &Path) -> OsSession {
    let mut cmd = common::command(home);
    cmd.args(["--set", "ui.interface=line"])
        .current_dir(home)
        .env("TERM", "xterm")
        .env_remove("NO_COLOR");
    let mut session = OsSession::spawn(cmd).unwrap();
    session.set_expect_timeout(Some(Duration::from_mins(1)));
    session
}

/// Scenario: Edit-reload cycle.
#[test]
fn a_changed_hook_handles_the_next_event_without_a_restart() {
    let home = mock_home();
    let home = home.path();
    std::fs::write(home.join("profiles/default/mock.toml"), RULES).unwrap();
    common::append_config(home, "\n[providers.mock]\nscript = \"mock.toml\"\n");
    for (key, value) in [
        ("hooks", "pre_llm_call"),
        ("mode", "context:NEW HOOK ACTIVE"),
    ] {
        let key = format!("plugins.entries.devhook.{key}");
        ok(&run(
            home,
            &["config", "set", &key, &format!("\"{value}\"")],
        ));
    }
    let dir = home.join("devhook-src");
    plugin(&dir);
    let dev = Dev::start(home, &dir);
    dev.wait_for("dev:devhook loaded (build 1)");
    dev.wait_for("serving dev:devhook");

    // Never installed: the profile's plugin directory stays empty.
    assert!(!home.join("profiles/default/plugins/devhook").exists());

    let mut p = chat(home);
    p.expect("› ").unwrap();
    p.send_line("first").unwrap();
    p.expect("old hook answered").unwrap();
    p.expect("› ").unwrap();

    std::fs::write(dir.join("variant"), "b\n").unwrap();
    dev.wait_for("dev:devhook loaded (build 2)");
    p.send_line("second").unwrap();
    p.expect("new hook answered").unwrap();
    p.expect("› ").unwrap();
    p.send_line("/exit").unwrap();
    p.expect(Eof).unwrap();

    // Scheduled runs never load a dev plugin.
    let added: Value = serde_json::from_str(&ok(&run(
        home,
        &[
            "--json", "cron", "add", "every 1h", "--prompt", "report", "--yes",
        ],
    )))
    .unwrap();
    let job = added["job"]["id"].as_str().unwrap().to_owned();
    ok(&run(home, &["cron", "run", &job]));
    let runs: Value =
        serde_json::from_str(&ok(&run(home, &["--json", "cron", "runs", &job]))).unwrap();
    let output = runs["runs"][0].to_string();
    assert!(output.contains("no hook"), "{output}");

    // Stopping the command takes the registration and its grants back.
    dev.interrupt();
    assert!(
        !home
            .join("profiles/default/cache/plugin-dev/devhook.toml")
            .exists()
    );
    let grants =
        std::fs::read_to_string(home.join("profiles/default/grants.toml")).unwrap_or_default();
    assert!(!grants.contains("devhook"), "{grants}");
}
