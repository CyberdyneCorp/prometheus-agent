//! A daemon on a temporary home, served by a fake host whose turns are
//! scripted by their prompt text.

#![allow(dead_code, unreachable_pub)] // each test file uses part of it

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_core::error::{Error, codes};
use pmt_core::runtime::{Role, WriterLock};
use pmt_gateway::{Exit, Host, HostFuture, Identity, Link, Server, ServerOptions};
use pmt_rpc::client::{Client, Inbox, Socket, dial, subprotocols};
use pmt_rpc::contracts::{
    ApprovalAsk, ApprovalRequestParams, ClientCapabilities, ClientKind, CompactParams, Compacted,
    ContextUsage, Decision, HelloParams, SessionCreateParams, SessionEvent, SessionList,
    SessionListParams, SessionRef, SessionSummary, SubmitParams, Submitted, Transcript, TurnEnded,
    TurnFinished, Version,
};
use pmt_rpc::control::{self, ControlRequest, Purpose};
use pmt_rpc::discovery::{Discovery, Endpoint, discover};
use serde_json::json;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

/// Sessions by id, with their profile.
#[derive(Debug, Default)]
pub struct FakeHost {
    sessions: Mutex<HashMap<String, String>>,
    next: AtomicUsize,
    turns: Arc<AtomicUsize>,
}

impl FakeHost {
    /// Adds a session of `profile`.
    pub fn add(&self, profile: &str) -> String {
        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let id = format!("ses_01JB7Z3Q5X8V2K4M6N8P0R2T{n:02}");
        self.sessions
            .lock()
            .unwrap()
            .insert(id.clone(), profile.to_owned());
        id
    }
}

fn event(link: &Link, n: u64) {
    link.emit::<SessionEvent>(&json!({"type": "test.step", "n": n}));
}

fn finish(link: &Link, turn: &str, answer: &str) {
    link.emit::<TurnEnded>(&TurnFinished {
        turn_id: turn.to_owned(),
        exit_reason: "completed".into(),
        final_response: Some(answer.to_owned()),
        error: None,
    });
}

/// What a turn does, by prompt: `steps:<n>:<ms>` emits n events `ms`
/// apart; `approval` asks for one; `budget` is refused before it starts.
async fn script(text: String, link: Link, turn: String) {
    if text == "approval" {
        let params = ApprovalRequestParams {
            session_id: link.session_id().to_owned(),
            tool: "terminal".into(),
            call_id: "c1".into(),
            subject: Some("rm -rf build".into()),
            findings: Vec::new(),
            remember: None,
        };
        let answer = link
            .ask::<ApprovalAsk>(&params, Some(Duration::from_secs(30)))
            .await;
        let approved = answer.is_some_and(|a| a.decision != Decision::Deny);
        finish(&link, &turn, if approved { "approved" } else { "denied" });
        return;
    }
    if let Some(rest) = text.strip_prefix("steps:") {
        let (count, ms) = rest.split_once(':').unwrap_or((rest, "0"));
        for n in 1..=count.parse().unwrap_or(0) {
            tokio::time::sleep(Duration::from_millis(ms.parse().unwrap_or(0))).await;
            event(&link, n);
        }
    }
    finish(&link, &turn, "done");
}

impl Host for FakeHost {
    fn owns<'a>(&'a self, profile: &'a str, session_id: &'a str) -> HostFuture<'a, bool> {
        let owned = self
            .sessions
            .lock()
            .unwrap()
            .get(session_id)
            .is_some_and(|p| p == profile);
        Box::pin(async move { Ok(owned) })
    }

    fn create<'a>(
        &'a self,
        profile: &'a str,
        _: SessionCreateParams,
    ) -> HostFuture<'a, SessionRef> {
        let session_id = self.add(profile);
        Box::pin(async move { Ok(SessionRef { session_id }) })
    }

    fn list<'a>(&'a self, profile: &'a str, _: SessionListParams) -> HostFuture<'a, SessionList> {
        let sessions = self
            .sessions
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, p)| *p == profile)
            .map(|(id, _)| SessionSummary {
                session_id: id.clone(),
                title: None,
                started_at: "2026-01-01T00:00:00Z".into(),
                live: false,
            })
            .collect();
        Box::pin(async move { Ok(SessionList { sessions }) })
    }

    fn resume<'a>(&'a self, _: &'a str, _: Link) -> HostFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn transcript<'a>(&'a self, _: &'a str, _: &'a str) -> HostFuture<'a, Transcript> {
        Box::pin(async { Ok(Transcript::default()) })
    }

    fn context_usage<'a>(&'a self, _: &'a str, _: &'a str) -> HostFuture<'a, ContextUsage> {
        Box::pin(async {
            Ok(ContextUsage {
                tokens: 10,
                context_window: None,
            })
        })
    }

    fn compact<'a>(&'a self, _: &'a str, _: CompactParams) -> HostFuture<'a, Compacted> {
        // A long compaction.
        Box::pin(async {
            tokio::time::sleep(Duration::from_secs(3)).await;
            Ok(Compacted {
                status: "compacted".into(),
                tokens_before: 100,
                tokens_after: 10,
            })
        })
    }

    fn submit<'a>(
        &'a self,
        _: &'a str,
        params: SubmitParams,
        link: Link,
    ) -> HostFuture<'a, Submitted> {
        Box::pin(async move {
            if params.text == "budget" {
                return Err(Error::new(
                    &codes::BUDGET_EXCEEDED,
                    "session budget exhausted",
                ));
            }
            let turn = format!("turn-{}", self.next.fetch_add(1, Ordering::Relaxed));
            let turns = Arc::clone(&self.turns);
            turns.fetch_add(1, Ordering::Relaxed);
            let id = turn.clone();
            tokio::spawn(async move {
                script(params.text, link, id).await;
                turns.fetch_sub(1, Ordering::Relaxed);
            });
            Ok(Submitted { turn_id: turn })
        })
    }

    fn load(&self) -> (usize, usize) {
        (
            self.sessions.lock().unwrap().len(),
            self.turns.load(Ordering::Relaxed),
        )
    }

    fn drain(
        &self,
        deadline: Duration,
    ) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let until = tokio::time::Instant::now() + deadline;
            while self.turns.load(Ordering::Relaxed) > 0 && tokio::time::Instant::now() < until {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
    }
}

/// A running daemon.
pub struct Daemon {
    pub home: tempfile::TempDir,
    pub host: Arc<FakeHost>,
    pub server: Arc<Server>,
    pub endpoint: Endpoint,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<Result<Exit, Error>>>,
}

pub fn identity() -> Identity {
    Identity {
        version: "0.1.0".into(),
        build_id: "build-a".into(),
        target: "test".into(),
        tier: "tier1".into(),
    }
}

pub fn options() -> ServerOptions {
    let mut options = ServerOptions::new(
        identity(),
        vec!["default".into(), "work".into(), "home".into()],
    );
    // Contract violations are counted, not fatal, so a test can see them.
    options.abort_on_violation = false;
    options
}

impl Daemon {
    pub async fn start() -> Self {
        Self::start_with(options(), tempfile::tempdir().unwrap()).await
    }

    pub async fn start_with(options: ServerOptions, home: tempfile::TempDir) -> Self {
        let host = Arc::new(FakeHost::default());
        let server = Server::new(options, Arc::clone(&host) as Arc<dyn Host>).unwrap();
        let (stop, stopped) = oneshot::channel::<()>();
        let path: PathBuf = home.path().to_owned();
        let lock = WriterLock::acquire(&path, Role::Daemon, server.profiles().to_vec()).unwrap();
        let task = {
            let server = Arc::clone(&server);
            tokio::spawn(async move {
                let shutdown = async {
                    let _ = stopped.await;
                };
                pmt_gateway::run(&lock, &path, server, Duration::from_secs(5), shutdown).await
            })
        };
        let endpoint = loop {
            if let Discovery::Running(record) = discover(home.path()).unwrap() {
                break record.endpoint;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        Self {
            home,
            host,
            server,
            endpoint,
            stop: Some(stop),
            task: Some(task),
        }
    }

    /// Stops the daemon and returns its home.
    pub async fn stop(mut self) -> (tempfile::TempDir, Exit) {
        let _ = self.stop.take().unwrap().send(());
        let exit = self.task.take().unwrap().await.unwrap().unwrap();
        (self.home, exit)
    }

    pub async fn ticket(&self, profile: &str) -> String {
        let request = ControlRequest::Ticket {
            purpose: Purpose::Interactive,
            profiles: vec![profile.into()],
        };
        control::ticket_of(
            control::call(&self.endpoint.control, &request)
                .await
                .unwrap(),
        )
        .unwrap()
        .ticket
    }

    /// A raw socket for `profile`, before `rpc.hello`.
    pub async fn socket(&self, profile: &str) -> Socket {
        let ticket = self.ticket(profile).await;
        dial(&self.endpoint.rpc_url, &subprotocols(&ticket, 1))
            .await
            .unwrap()
    }

    /// An attached client for `profile`.
    pub async fn client(&self, profile: &str, server_requests: bool) -> (Client, Inbox) {
        Client::connect(
            &self.endpoint,
            Purpose::Interactive,
            vec![profile.into()],
            &hello(1, 0, server_requests),
        )
        .await
        .unwrap()
    }
}

pub fn hello(major: u16, minor: u16, server_requests: bool) -> HelloParams {
    HelloParams {
        client: ClientKind::Tui,
        client_version: "0.1.0".into(),
        protocol: Version { major, minor },
        capabilities: ClientCapabilities {
            server_requests,
            events: Vec::new(),
        },
    }
}
