//! The TUI answering the daemon's secret and sudo requests (spec: tui,
//! Server-to-client requests). The TUI binary runs in a pseudo-terminal
//! against an in-process gateway whose host asks, by prompt text, for a
//! secret (`secret`) or the sudo password (`sudo`) and records the
//! answer: the value typed reaches the host, is never drawn, never
//! written to the TUI history and never shown in the transcript; Esc or
//! an empty input declines; a request that times out withdraws its card.
//! The gateway reports another build than the TUI's, so the same harness
//! covers the stale-build notice (workspace-architecture, Stale daemon
//! build detection); any other prompt is answered `pong`.

#![cfg(all(unix, feature = "gateway", feature = "tui"))]

mod common;

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::mock_home;
use common::tui::{enter, finish, opened, see, tui};
use expectrl::{ControlCode, Expect as _};
use pmt_core::error::{Error, codes};
use pmt_core::runtime::{Role, WriterLock};
use pmt_gateway::{Author, Exit, Host, HostFuture, Identity, Link, Server, ServerOptions};
use pmt_rpc::contracts::{
    Admission, ClientKind, CommandCatalog, CompactParams, Compacted, CompleteParams, Completions,
    ConfigValue, ContextUsage, DispatchParams, Dispatched, ForkParams, ModelOptions, ResolveParams,
    SecretAsk, SecretRequestParams, SessionCreateParams, SessionList, SessionListParams,
    SessionRef, SessionSetup, SubmitParams, Submitted, SudoAsk, SudoRequestParams, Transcript,
    TurnEnded, TurnFinished,
};
use pmt_rpc::discovery::{Discovery, discover};
use tokio::sync::oneshot;

/// What the host was answered, in order (`None`: declined or unanswered).
type Answers = Arc<Mutex<Vec<Option<String>>>>;

/// One session; `secret` and `sudo` prompts ask the TUI.
#[derive(Default)]
struct AskingHost {
    answers: Answers,
}

const SESSION: &str = "ses_01JB7Z3Q5X8V2K4M6N8P0R2T00";

async fn ask(text: String, link: Link, turn: String, answers: Answers) {
    let session_id = link.session_id().to_owned();
    if text != "secret" && text != "sudo" {
        finish_turn(&link, turn, "pong");
        return;
    }
    let answer = if text == "secret" {
        let params = SecretRequestParams {
            session_id,
            name: "DEPLOY_TOKEN".into(),
            reason: Some("the deploy skill pushes releases".into()),
            deadline_ms: None,
        };
        link.ask::<SecretAsk>(&params, None)
            .await
            .and_then(|a| a.value)
    } else {
        let params = SudoRequestParams {
            session_id,
            command: "sudo apt-get update".into(),
            deadline_ms: None,
        };
        link.ask::<SudoAsk>(&params, None)
            .await
            .and_then(|a| a.password)
    };
    let reply = if answer.is_some() {
        "got it"
    } else {
        "none given"
    };
    answers.lock().unwrap().push(answer);
    finish_turn(&link, turn, reply);
}

fn finish_turn(link: &Link, turn: String, reply: &str) {
    link.emit::<TurnEnded>(&TurnFinished {
        turn_id: turn,
        exit_reason: "completed".into(),
        final_response: Some(reply.into()),
        error: None,
        usage: None,
        report: None,
    });
}

fn ready<'a, T: Send + 'a>(value: T) -> HostFuture<'a, T> {
    Box::pin(async move { Ok(value) })
}

impl Host for AskingHost {
    fn owns<'a>(&'a self, _: &'a str, session_id: &'a str) -> HostFuture<'a, bool> {
        ready(session_id == SESSION)
    }

    fn create<'a>(&'a self, _: &'a str, _: SessionCreateParams) -> HostFuture<'a, SessionRef> {
        ready(SessionRef {
            session_id: SESSION.into(),
        })
    }

    fn list<'a>(&'a self, _: &'a str, _: SessionListParams) -> HostFuture<'a, SessionList> {
        ready(SessionList::default())
    }

    fn resume<'a>(&'a self, _: &'a str, _: Option<SessionSetup>, _: Link) -> HostFuture<'a, ()> {
        ready(())
    }

    fn transcript<'a>(&'a self, _: &'a str, _: &'a str) -> HostFuture<'a, Transcript> {
        ready(Transcript::default())
    }

    fn context_usage<'a>(&'a self, _: &'a str, _: &'a str) -> HostFuture<'a, ContextUsage> {
        ready(ContextUsage {
            tokens: 0,
            context_window: None,
            cost_usd: None,
            billing_source: None,
            subscription: None,
        })
    }

    fn compact<'a>(&'a self, _: &'a str, _: CompactParams) -> HostFuture<'a, Compacted> {
        Box::pin(async { Err(Error::new(&codes::SESSION_NOT_FOUND, "nothing to compact")) })
    }

    fn submit<'a>(
        &'a self,
        _: &'a str,
        params: SubmitParams,
        _: Author,
        link: Link,
    ) -> HostFuture<'a, Submitted> {
        let turn = format!("turn-{}", self.answers.lock().unwrap().len());
        tokio::spawn(ask(
            params.text,
            link,
            turn.clone(),
            Arc::clone(&self.answers),
        ));
        ready(Submitted {
            turn_id: turn,
            admission: Admission::Started,
            notice: None,
        })
    }

    fn dispatch<'a>(
        &'a self,
        _: &'a str,
        params: DispatchParams,
        _: Link,
    ) -> HostFuture<'a, Dispatched> {
        ready(Dispatched::Refused {
            reason: format!("/{} is not a command here", params.name),
        })
    }

    fn interrupt<'a>(&'a self, _: &'a str, _: &'a str) -> HostFuture<'a, ()> {
        ready(())
    }

    fn resolve<'a>(&'a self, _: &'a str, _: ResolveParams) -> HostFuture<'a, SessionRef> {
        Box::pin(async { Err(Error::new(&codes::SESSION_NOT_FOUND, "no such session")) })
    }

    fn fork<'a>(&'a self, _: &'a str, _: ForkParams) -> HostFuture<'a, SessionRef> {
        Box::pin(async { Err(Error::new(&codes::SESSION_NOT_FOUND, "no forks here")) })
    }

    fn commands<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: ClientKind,
    ) -> HostFuture<'a, CommandCatalog> {
        ready(CommandCatalog::default())
    }

    fn complete_path<'a>(&'a self, _: &'a str, _: CompleteParams) -> HostFuture<'a, Completions> {
        ready(Completions::default())
    }

    fn model_options<'a>(&'a self, _: &'a str, _: &'a str) -> HostFuture<'a, ModelOptions> {
        ready(ModelOptions::default())
    }

    fn config_get<'a>(&'a self, _: &'a str, key: &'a str) -> HostFuture<'a, ConfigValue> {
        Box::pin(async move { Err(Error::new(&codes::CONFIG_INVALID, format!("no {key}"))) })
    }

    fn close_idle<'a>(&'a self, _: &'a str, _: &'a str) -> HostFuture<'a, bool> {
        ready(false)
    }

    fn load(&self) -> (usize, usize) {
        (1, 0)
    }

    fn drain(&self, _: Duration) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
}

/// The gateway of `home`, run on `runtime` until dropped.
struct Gateway {
    answers: Answers,
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<Result<Exit, Error>>>,
    runtime: tokio::runtime::Runtime,
}

impl Gateway {
    /// Secret and sudo requests settle unanswered after `timeout_secs`.
    fn start(home: &Path, timeout_secs: u64) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let identity = Identity {
            version: env!("CARGO_PKG_VERSION").into(),
            build_id: "test-gateway".into(),
            target: "test".into(),
            tier: "tier1".into(),
        };
        let mut options = ServerOptions::new(identity, vec!["default".into()]);
        options.rpc.request_timeout_secs = timeout_secs;
        let host = Arc::new(AskingHost::default());
        let answers = Arc::clone(&host.answers);
        let server = Server::new(options, host as Arc<dyn Host>).unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let path: PathBuf = home.to_owned();
        let task = runtime.spawn(async move {
            let lock = WriterLock::acquire(&path, Role::Daemon, server.profiles())?;
            let shutdown = async {
                let _ = stopped.await;
            };
            pmt_gateway::run(&lock, &path, server, Duration::from_secs(1), shutdown).await
        });
        while !matches!(discover(home).unwrap(), Discovery::Running(_)) {
            std::thread::sleep(Duration::from_millis(10));
        }
        Self {
            answers,
            stop: Some(stop),
            task: Some(task),
            runtime,
        }
    }

    fn answers(&self) -> Vec<Option<String>> {
        self.answers.lock().unwrap().clone()
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = self
                .runtime
                .block_on(async move { tokio::time::timeout(Duration::from_secs(10), task).await });
        }
    }
}

/// Scenario: secret and sudo requests are answered on a masked card. The
/// typed value reaches the daemon and appears nowhere on the screen, in
/// the history or the transcript; Esc and an empty input decline.
#[test]
fn secret_and_sudo_requests_are_answered_masked_and_can_be_declined() {
    const VALUE: &str = "hunter2-sekret";
    let home = mock_home();
    let gateway = Gateway::start(home.path(), 600);
    let mut p = tui(home.path(), &["tui"], &[]);
    opened(&mut p);
    let mut drawn = String::new();

    enter(&mut p, "secret");
    drawn += &see(&mut p, "secret needed: DEPLOY_TOKEN");
    drawn += &see(&mut p, "the deploy skill pushes releases");
    drawn += &see(&mut p, "answer within");
    p.send(VALUE).unwrap();
    // Only changed cells are redrawn: the bullets come one or a few at a time.
    drawn += &see(&mut p, "•");
    p.send("\r").unwrap();
    drawn += &see(&mut p, "(secret DEPLOY_TOKEN sent)");
    drawn += &see(&mut p, "got it");

    enter(&mut p, "sudo");
    drawn += &see(&mut p, "sudo password needed");
    drawn += &see(&mut p, "for: sudo apt-get update");
    p.send("abc").unwrap();
    drawn += &see(&mut p, "•");
    p.send(ControlCode::Escape).unwrap();
    drawn += &see(&mut p, "(sudo password declined)");
    drawn += &see(&mut p, "none given");

    enter(&mut p, "secret");
    drawn += &see(&mut p, "secret needed: DEPLOY_TOKEN");
    p.send("\r").unwrap();
    drawn += &see(&mut p, "(secret DEPLOY_TOKEN declined)");

    p.send(ControlCode::EndOfTransmission).unwrap();
    let (rest, code) = finish(p);
    drawn += &rest;
    assert_eq!(code, Some(0), "{drawn}");
    assert_eq!(
        gateway.answers(),
        vec![Some(VALUE.to_owned()), None, None],
        "what the daemon was answered"
    );
    assert!(!drawn.contains(VALUE), "the value was drawn");
    assert!(!drawn.contains("abc"), "the declined input was drawn");
    let history = std::fs::read_to_string(home.path().join("profiles/default/tui/history"))
        .unwrap_or_default();
    assert!(!history.contains(VALUE), "the value is in the history");
    assert!(history.contains("secret"), "prompts are kept: {history}");
}

/// Scenario: a request that times out withdraws its card, which shows its
/// deadline (the countdown itself is unit-tested: only the changed digit
/// is redrawn); what was typed into it is dropped, not sent as a prompt.
#[test]
fn an_unanswered_secret_request_counts_down_and_is_withdrawn() {
    let home = mock_home();
    let gateway = Gateway::start(home.path(), 3);
    let mut p = tui(home.path(), &["tui"], &[]);
    opened(&mut p);
    enter(&mut p, "secret");
    see(&mut p, "secret needed: DEPLOY_TOKEN");
    see(&mut p, "answer within 3 s");
    p.send("half-typed").unwrap();
    see(
        &mut p,
        "(the question timed out or nobody could answer it; denied)",
    );
    see(&mut p, "none given");
    p.send(ControlCode::EndOfTransmission).unwrap();
    let (rest, code) = finish(p);
    assert_eq!(code, Some(0), "{rest}");
    assert_eq!(gateway.answers(), vec![None]);
    let history = std::fs::read_to_string(home.path().join("profiles/default/tui/history"))
        .unwrap_or_default();
    assert!(!history.contains("half-typed"), "{history}");
}

/// Scenario: Binary updated under a running daemon. The gateway runs
/// another build (`test-gateway`) with the same protocol major: the TUI
/// shows the stale-build notice and still attaches (a prompt is answered).
#[test]
fn a_daemon_of_another_build_is_reported_and_the_tui_still_attaches() {
    let home = mock_home();
    let _gateway = Gateway::start(home.path(), 600);
    let mut p = tui(home.path(), &["tui"], &[]);
    // The notice may be drawn before or after the status line: matched
    // without `opened`, which consumes up to that line.
    p.expect(common::tui::ENTER).unwrap();
    see(
        &mut p,
        &format!(
            "daemon is running an older build ({}); restart with 'prometheus daemon restart'",
            env!("CARGO_PKG_VERSION")
        ),
    );
    enter(&mut p, "ping");
    see(&mut p, "pong");
    p.send(ControlCode::EndOfTransmission).unwrap();
    let (rest, code) = finish(p);
    assert_eq!(code, Some(0), "{rest}");
}
