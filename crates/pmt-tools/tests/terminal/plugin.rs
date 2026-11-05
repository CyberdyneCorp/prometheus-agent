//! A `terminal-backend` plugin behind the terminal (spec:
//! execution-environments, Remote sandboxes are plugins; Session cwd
//! tracking). The plugin is a fake implementing the host's contract; the
//! WASM side of the contract is covered by the pmt-plugins tests.

use std::sync::{Arc, Mutex};

use pmt_config::schema::TerminalConfig;
use pmt_sandbox::backend::{Descriptor, ExecOutput, ExecRequest, Isolation, RemoteBackend};
use pmt_tools::terminal::target::PluginTarget;
use pmt_tools::terminal::{Status, Terminal, TerminalArgs};

use crate::common::{Fixture, KNOWN_SECRET};

#[derive(Debug)]
struct Fake {
    descriptor: Descriptor,
    seen: Mutex<Vec<ExecRequest>>,
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
    fn cancel(&self, _: &str) {}
    fn teardown(&self) {}
}

fn terminal(fixture: &Fixture, fake: &Arc<Fake>) -> Terminal {
    let env = fixture.child_env(&[], &[]);
    let mut setup = fixture.setup(TerminalConfig::default(), env, fixture.sandbox(&[], None));
    setup.target = Arc::new(PluginTarget::new(Arc::clone(fake) as Arc<dyn RemoteBackend>));
    setup.start_cwd = "/box".into();
    Terminal::new(setup).unwrap()
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
    let mut background = TerminalArgs::new("sleep 100");
    background.background = true;
    let refused = terminal.run(background).await;
    assert!(refused.error.unwrap().contains("foreground commands only"));
    let down = terminal.run(TerminalArgs::new("unreachable")).await;
    assert_eq!(down.status, Status::Degraded);
}
