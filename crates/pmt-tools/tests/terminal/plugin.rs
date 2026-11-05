//! A `terminal-backend` plugin behind the terminal (spec:
//! execution-environments, Remote sandboxes are plugins; Session cwd
//! tracking; Foreground cap, promotion and server guard; Background process
//! registry). The plugin is a fake implementing the host's contract; the
//! WASM side of the contract is covered by the pmt-plugins tests.

use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use pmt_config::schema::TerminalConfig;
use pmt_sandbox::backend::{Descriptor, ExecOutput, ExecRequest, Isolation, RemoteBackend};
use pmt_tools::terminal::process::{Completion, ProcessArgs};
use pmt_tools::terminal::target::PluginTarget;
use pmt_tools::terminal::{Status, Terminal, TerminalArgs};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::common::{Fixture, KNOWN_SECRET};

#[derive(Debug)]
struct Fake {
    descriptor: Descriptor,
    seen: Mutex<Vec<ExecRequest>>,
    /// Ids the host cancelled; a `block` command waits for its own.
    cancelled: (Mutex<Vec<String>>, Condvar),
}

impl Fake {
    fn cancelled(&self) -> Vec<String> {
        self.cancelled.0.lock().unwrap().clone()
    }

    fn wait_for_cancel(&self, id: &str) {
        let (list, signal) = &self.cancelled;
        let mut list = list.lock().unwrap();
        while !list.iter().any(|c| c == id) {
            list = signal.wait(list).unwrap();
        }
    }
}

impl RemoteBackend for Fake {
    fn name(&self) -> &'static str {
        "acmebox"
    }
    fn descriptor(&self) -> &Descriptor {
        &self.descriptor
    }
    fn check(&self) -> Result<String, String> {
        Ok("ok".to_owned())
    }
    fn execute(&self, request: &ExecRequest) -> Result<ExecOutput, String> {
        self.seen.lock().unwrap().push(request.clone());
        if request.command.contains("block") {
            self.wait_for_cancel(&request.id);
            return Err("cancelled".to_owned());
        }
        if request.command.contains("unreachable") {
            return Err("sandbox API returned 503".to_owned());
        }
        Ok(ExecOutput {
            exit_code: (!request.command.contains("omit")).then_some(0),
            stdout: format!("out {KNOWN_SECRET}\n"),
            stderr: String::new(),
            cwd_observed: request.command.strip_prefix("cd ").map(str::to_owned),
        })
    }
    fn upload(&self, _: &str, _: &[u8]) -> Result<(), String> {
        Ok(())
    }
    fn download(&self, _: &str) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }
    fn cancel(&self, id: &str) {
        self.cancelled.0.lock().unwrap().push(id.to_owned());
        self.cancelled.1.notify_all();
    }
    fn teardown(&self) {}
}

fn terminal(fixture: &Fixture, fake: &Arc<Fake>) -> Terminal {
    notified_terminal(fixture, fake).0
}

fn notified_terminal(
    fixture: &Fixture,
    fake: &Arc<Fake>,
) -> (Terminal, mpsc::UnboundedReceiver<Completion>) {
    let env = fixture.child_env(&[], &[]);
    let mut setup = fixture.setup(TerminalConfig::default(), env, fixture.sandbox(&[], None));
    setup.target = Arc::new(PluginTarget::new(Arc::clone(fake) as Arc<dyn RemoteBackend>));
    setup.start_cwd = "/box".into();
    let (tx, rx) = mpsc::unbounded_channel();
    setup.notify = Some(tx);
    (Terminal::new(setup).unwrap(), rx)
}

async fn act(terminal: &Terminal, action: &str, id: &str) -> Value {
    terminal
        .processes()
        .handle(&ProcessArgs {
            action: action.to_owned(),
            process_id: Some(id.to_owned()),
            data: None,
            timeout: Some(30.0),
        })
        .await
}

fn fake() -> Arc<Fake> {
    Arc::new(Fake {
        descriptor: Descriptor {
            isolation: Isolation::Remote,
            uses_host_filesystem: false,
            default_cwd: "/box".to_owned(),
            session_isolated_when_ephemeral: true,
            persistent: false,
        },
        seen: Mutex::new(Vec::new()),
        cancelled: (Mutex::new(Vec::new()), Condvar::new()),
    })
}

#[tokio::test]
async fn plugin_omits_exit_code() {
    let fixture = Fixture::new();
    let fake = fake();
    let terminal = terminal(&fixture, &fake);
    let result = terminal.run(TerminalArgs::new("echo omit")).await;
    assert_eq!(result.status, Status::Error);
    assert_eq!(
        result.error.as_deref(),
        Some("terminal backend 'acmebox' broke its contract: the execute result has no exit_code")
    );
    assert_ne!(result.exit_code, Some(0), "never read as success");
}

#[tokio::test]
async fn commands_reach_the_plugin_with_their_cwd_and_come_back_shaped() {
    let fixture = Fixture::new();
    let fake = fake();
    let terminal = terminal(&fixture, &fake);
    let moved = terminal.run(TerminalArgs::new("cd /box/src")).await;
    assert_eq!(moved.exit_code, Some(0));
    assert_eq!(moved.cwd.as_deref(), Some("/box/src"));
    // Output is redacted like any other terminal output.
    assert!(!moved.output.contains(KNOWN_SECRET), "{}", moved.output);
    terminal.run(TerminalArgs::new("ls")).await;
    let second = fake.seen.lock().unwrap()[1].clone();
    assert_eq!(second.cwd, "/box/src");
    assert!(second.timeout_ms > 0);
    let down = terminal.run(TerminalArgs::new("unreachable")).await;
    assert_eq!(down.status, Status::Degraded);
}

#[tokio::test]
async fn a_long_test_run_on_a_plugin_backend_is_promoted_and_notifies() {
    let fixture = Fixture::new();
    let fake = fake();
    let (terminal, mut completions) = notified_terminal(&fixture, &fake);
    let args = TerminalArgs {
        timeout: Some(1800.0),
        ..TerminalArgs::new("cargo test")
    };
    let result = terminal.run(args).await;
    assert_eq!(result.status, Status::Started, "{result:?}");
    assert!(result.promoted_from_foreground);
    assert!(
        result
            .notices
            .iter()
            .any(|n| n.starts_with("promoted_from_foreground")),
        "{:?}",
        result.notices
    );
    let id = result.process_id.expect("a tracked process");
    let done = act(&terminal, "wait", &id).await;
    assert_eq!(done["state"], "exited", "{done}");
    assert_eq!(done["exit_code"], 0);
    let output = done["output"].as_str().unwrap();
    assert!(output.starts_with("out "), "{output}");
    assert!(
        !output.contains(KNOWN_SECRET),
        "background output is redacted"
    );
    let completion = tokio::time::timeout(Duration::from_secs(10), completions.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(completion.process_id, id);
    assert_eq!(completion.exit_code, 0);
    // The plugin bounds the command by the call's timeout.
    assert_eq!(fake.seen.lock().unwrap()[0].timeout_ms, 1_800_000);
}

#[tokio::test]
async fn a_background_plugin_command_is_killed_through_cancel() {
    let fixture = Fixture::new();
    let fake = fake();
    let (terminal, mut completions) = notified_terminal(&fixture, &fake);
    let args = TerminalArgs {
        background: true,
        notify_on_complete: true,
        ..TerminalArgs::new("block forever")
    };
    let result = terminal.run(args).await;
    assert_eq!(result.status, Status::Started, "{result:?}");
    let id = result.process_id.expect("a tracked process");
    while fake.seen.lock().unwrap().is_empty() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let listed = terminal
        .processes()
        .handle(&ProcessArgs {
            action: "list".to_owned(),
            ..ProcessArgs::default()
        })
        .await;
    assert_eq!(listed["processes"][0]["state"], "running", "{listed}");
    let killed = act(&terminal, "kill", &id).await;
    assert_eq!(killed["state"], "killed", "{killed}");
    let request = fake.seen.lock().unwrap()[0].clone();
    // Without a timeout the command is not bounded, like a local one.
    assert_eq!(request.timeout_ms, u64::MAX);
    tokio::time::timeout(Duration::from_secs(10), async {
        while !fake.cancelled().contains(&request.id) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("kill asks the plugin to cancel the call");
    assert!(
        completions.try_recv().is_err(),
        "a kill is not a completion"
    );
}

#[tokio::test]
async fn a_trailing_ampersand_runs_in_the_background_and_session_end_cancels_it() {
    let fixture = Fixture::new();
    let fake = fake();
    let terminal = terminal(&fixture, &fake);
    let result = terminal.run(TerminalArgs::new("block server &")).await;
    assert_eq!(result.status, Status::Started, "{result:?}");
    let request = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(request) = fake.seen.lock().unwrap().first().cloned() {
                return request;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the plugin receives the command");
    assert_eq!(request.command, "block server");
    terminal.processes().close().await;
    let id = request.id;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !fake.cancelled().contains(&id) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("session end cancels running plugin commands");
}

// Real time (about 11 s: the timeout plus the plugin's grace); paused time
// would not advance while the plugin call blocks a thread.
#[tokio::test]
async fn a_background_plugin_command_past_its_timeout_is_cancelled() {
    let fixture = Fixture::new();
    let fake = fake();
    let terminal = terminal(&fixture, &fake);
    let args = TerminalArgs {
        background: true,
        timeout: Some(1.0),
        ..TerminalArgs::new("block slow")
    };
    let id = terminal.run(args).await.process_id.expect("started");
    let done = act(&terminal, "wait", &id).await;
    assert_eq!(done["exit_code"], 124, "{done}");
    assert!(
        done["output"]
            .as_str()
            .unwrap()
            .contains("stopped after 1s"),
        "{done}"
    );
    let request = fake.seen.lock().unwrap()[0].clone();
    assert_eq!(request.timeout_ms, 1000);
    // The cancel is sent from its own thread so a slow plugin cannot hold up
    // the timeout; it lands shortly after the job ends.
    tokio::time::timeout(Duration::from_secs(10), async {
        while !fake.cancelled().contains(&request.id) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the timeout asks the plugin to cancel the call");
}
