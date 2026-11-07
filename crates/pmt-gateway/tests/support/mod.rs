//! A daemon on a temporary home, served by a fake host whose turns are
//! scripted by their prompt text.

#![allow(dead_code, unreachable_pub)] // each test file uses part of it

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use pmt_core::error::{Error, codes};
use pmt_core::runtime::{Role, WriterLock};
use pmt_gateway::{Author, Exit, Host, HostFuture, Identity, Link, Server, ServerOptions};
use pmt_rpc::client::{Client, Inbox, Socket, dial, subprotocols};
use pmt_rpc::contracts::{
    Admission, ApprovalAsk, ApprovalRequestParams, ClientCapabilities, ClientKind, CommandCatalog,
    CommandEntry, CommandSource, CompactParams, Compacted, CompleteParams, Completion, Completions,
    ConfigValue, ContextUsage, Decision, DispatchParams, Dispatched, Disposition, ForkParams,
    HelloParams, ModelOption, ModelOptions, ResolveParams, SessionCreateParams, SessionEvent,
    SessionList, SessionListParams, SessionRef, SessionSetup, SessionSummary, SubmitParams,
    Submitted, Transcript, TranscriptMessage, TurnEnded, TurnFinished, Version,
};
use pmt_rpc::control::{self, ControlRequest, Purpose};
use pmt_rpc::discovery::{Discovery, Endpoint, discover};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

/// Sessions by id, with their profile.
#[derive(Debug, Default)]
pub struct FakeHost {
    sessions: Mutex<HashMap<String, String>>,
    next: AtomicUsize,
    turns: Arc<AtomicUsize>,
    /// Every admitted input's stamped author, in order.
    pub authors: Mutex<Vec<Author>>,
    /// Turns started, by prompt text.
    pub started: Mutex<Vec<String>>,
    /// `config.get` reads, by profile.
    pub config_reads: Mutex<Vec<String>>,
    /// Sessions closed as idle.
    pub closed: Mutex<Vec<String>>,
    /// Sessions unloaded without being ended, in order.
    pub unloaded: Mutex<Vec<String>>,
    /// Each session's turn answers, kept as its transcript.
    answers: Answers,
}

type Answers = Arc<Mutex<HashMap<String, Vec<String>>>>;

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

fn finish(answers: &Answers, link: &Link, turn: &str, answer: &str) {
    record_answer(answers, link, answer);
    link.emit::<TurnEnded>(&TurnFinished {
        turn_id: turn.to_owned(),
        exit_reason: "completed".into(),
        final_response: Some(answer.to_owned()),
        error: None,
        usage: None,
        report: None,
    });
}

fn record_answer(answers: &Answers, link: &Link, answer: &str) {
    answers
        .lock()
        .unwrap()
        .entry(link.session_id().to_owned())
        .or_default()
        .push(answer.to_owned());
}

/// What a turn does, by prompt: `steps:<n>:<ms>` emits n events `ms`
/// apart; `approval` asks for one; `budget` is refused before it starts.
async fn script(text: String, link: Link, turn: String, answers: Answers) {
    if text == "approval" || text == "approval-fast" {
        let deadline = if text == "approval" {
            Duration::from_secs(30)
        } else {
            Duration::from_millis(200)
        };
        let params = ApprovalRequestParams {
            session_id: link.session_id().to_owned(),
            tool: "terminal".into(),
            call_id: "c1".into(),
            subject: Some("rm -rf build".into()),
            findings: Vec::new(),
            remember: None,
            deadline_ms: None,
            explanation: None,
        };
        let answer = link.ask::<ApprovalAsk>(&params, Some(deadline)).await;
        let approved = answer.is_some_and(|a| a.decision != Decision::Deny);
        finish(
            &answers,
            &link,
            &turn,
            if approved { "approved" } else { "denied" },
        );
        return;
    }
    if let Some(rest) = text.strip_prefix("steps:") {
        let (count, ms) = rest.split_once(':').unwrap_or((rest, "0"));
        for n in 1..=count.parse().unwrap_or(0) {
            tokio::time::sleep(Duration::from_millis(ms.parse().unwrap_or(0))).await;
            event(&link, n);
        }
    }
    finish(&answers, &link, &turn, "done");
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

    /// A setup with model `slow` takes two seconds to load.
    fn resume<'a>(
        &'a self,
        _: &'a str,
        setup: Option<SessionSetup>,
        _: Link,
    ) -> HostFuture<'a, ()> {
        let slow = setup.is_some_and(|s| s.model.as_deref() == Some("slow"));
        Box::pin(async move {
            if slow {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Ok(())
        })
    }

    fn dispatch<'a>(
        &'a self,
        _: &'a str,
        params: DispatchParams,
        _: Link,
    ) -> HostFuture<'a, Dispatched> {
        Box::pin(async move {
            Ok(match params.name.as_str() {
                "panic" => panic!("a handler bug"),
                "echo" => Dispatched::Done {
                    notice: params.args,
                },
                "say" => Dispatched::Send {
                    message: params.args,
                    images: Vec::new(),
                },
                other => Dispatched::Refused {
                    reason: format!("/{other} is not a command here"),
                },
            })
        })
    }

    fn transcript<'a>(&'a self, _: &'a str, session_id: &'a str) -> HostFuture<'a, Transcript> {
        let messages = self
            .answers
            .lock()
            .unwrap()
            .get(session_id)
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(n, text)| TranscriptMessage {
                id: format!("msg_{n}"),
                role: "assistant".into(),
                text: Some(text.clone()),
            })
            .collect();
        Box::pin(async { Ok(Transcript { messages }) })
    }

    fn context_usage<'a>(&'a self, _: &'a str, _: &'a str) -> HostFuture<'a, ContextUsage> {
        Box::pin(async {
            Ok(ContextUsage {
                tokens: 10,
                context_window: None,
                cost_usd: None,
                billing_source: None,
                subscription: None,
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
        author: Author,
        link: Link,
    ) -> HostFuture<'a, Submitted> {
        self.authors.lock().unwrap().push(author);
        self.started.lock().unwrap().push(params.text.clone());
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
            let answers = Arc::clone(&self.answers);
            tokio::spawn(async move {
                script(params.text, link, id, answers).await;
                turns.fetch_sub(1, Ordering::Relaxed);
            });
            Ok(Submitted {
                turn_id: turn,
                admission: Admission::Started,
                notice: None,
            })
        })
    }

    fn interrupt<'a>(&'a self, _: &'a str, _: &'a str) -> HostFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn resolve<'a>(
        &'a self,
        profile: &'a str,
        params: ResolveParams,
    ) -> HostFuture<'a, SessionRef> {
        let owned = self
            .sessions
            .lock()
            .unwrap()
            .get(&params.reference)
            .is_some_and(|p| p == profile);
        Box::pin(async move {
            if owned {
                Ok(SessionRef {
                    session_id: params.reference,
                })
            } else {
                Err(Error::new(&codes::SESSION_NOT_FOUND, "no such session"))
            }
        })
    }

    fn fork<'a>(&'a self, profile: &'a str, _: ForkParams) -> HostFuture<'a, SessionRef> {
        let session_id = self.add(profile);
        Box::pin(async move { Ok(SessionRef { session_id }) })
    }

    fn commands<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: ClientKind,
    ) -> HostFuture<'a, CommandCatalog> {
        let entry = |name: &str, source, disposition| CommandEntry {
            name: name.to_owned(),
            aliases: Vec::new(),
            description: format!("the {name} command"),
            args: None,
            source,
            disposition,
            reason: None,
        };
        let commands = vec![
            entry("compress", CommandSource::Builtin, Disposition::Exec),
            entry("compact", CommandSource::Builtin, Disposition::Exec),
            entry("deploy", CommandSource::Skill, Disposition::Exec),
            entry("snapshot", CommandSource::Builtin, Disposition::Unavailable),
            entry(
                "debug-internals",
                CommandSource::Builtin,
                Disposition::Hidden,
            ),
        ];
        Box::pin(async { Ok(CommandCatalog { commands }) })
    }

    /// The session's backend: a container whose `./src/` holds `main.rs`
    /// and `lib/`; `/unreachable` is a backend that cannot be reached.
    fn complete_path<'a>(
        &'a self,
        _: &'a str,
        params: CompleteParams,
    ) -> HostFuture<'a, Completions> {
        Box::pin(async move {
            if params.prefix.starts_with("/unreachable") {
                return Err(Error::new(&codes::RPC_CONNECTION, "backend unreachable"));
            }
            let entries = [("./src/main.rs", false), ("./src/lib/", true)];
            let items = entries
                .into_iter()
                .filter(|(path, _)| path.starts_with(&params.prefix))
                .map(|(path, dir)| Completion {
                    text: path.to_owned(),
                    description: None,
                    dir,
                })
                .collect();
            Ok(Completions { items })
        })
    }

    fn model_options<'a>(&'a self, _: &'a str, _: &'a str) -> HostFuture<'a, ModelOptions> {
        Box::pin(async {
            Ok(ModelOptions {
                current: Some("mock/m1".into()),
                models: vec![ModelOption {
                    id: "mock/m1".into(),
                    provider: "mock".into(),
                    context_window: Some(8000),
                }],
            })
        })
    }

    fn config_get<'a>(&'a self, profile: &'a str, key: &'a str) -> HostFuture<'a, ConfigValue> {
        self.config_reads.lock().unwrap().push(profile.to_owned());
        Box::pin(async move {
            Ok(ConfigValue {
                key: key.to_owned(),
                value: json!("mock/m1"),
                source: "file".into(),
            })
        })
    }

    fn close_idle<'a>(&'a self, _: &'a str, session_id: &'a str) -> HostFuture<'a, bool> {
        self.closed.lock().unwrap().push(session_id.to_owned());
        Box::pin(async { Ok(true) })
    }

    fn unload_idle<'a>(&'a self, _: &'a str, session_id: &'a str) -> HostFuture<'a, bool> {
        self.unloaded.lock().unwrap().push(session_id.to_owned());
        Box::pin(async { Ok(true) })
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
        let lock = WriterLock::acquire(&path, Role::Daemon, server.profiles()).unwrap();
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
        self.ticket_for(Purpose::Interactive, &[profile]).await
    }

    pub async fn ticket_for(&self, purpose: Purpose, profiles: &[&str]) -> String {
        let request = ControlRequest::Ticket {
            purpose,
            profiles: profiles.iter().map(|p| (*p).to_owned()).collect(),
            pid: Some(std::process::id()),
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

    /// An attached client of kind `kind` for `profile`.
    pub async fn client_as(
        &self,
        kind: ClientKind,
        profile: &str,
        server_requests: bool,
    ) -> (Client, Inbox) {
        let mut hello = hello(1, 0, server_requests);
        hello.client = kind;
        Client::connect(
            &self.endpoint,
            Purpose::Interactive,
            vec![profile.into()],
            &hello,
        )
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

/// The next text frame, as JSON.
pub async fn next_json(socket: &mut Socket) -> Value {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), socket.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => {
                return serde_json::from_str(text.as_str()).unwrap();
            }
            Ok(Some(Ok(_))) => {}
            other => panic!("no frame: {other:?}"),
        }
    }
}

/// The close code the daemon sends next (frames before it are skipped).
pub async fn close_code(socket: &mut Socket) -> u16 {
    loop {
        match tokio::time::timeout(Duration::from_secs(5), socket.next()).await {
            Ok(Some(Ok(Message::Close(Some(frame))))) => return u16::from(frame.code),
            Ok(Some(Ok(_))) => {}
            other => panic!("no close frame: {other:?}"),
        }
    }
}

pub async fn send(socket: &mut Socket, value: &Value) {
    socket.send(Message::text(value.to_string())).await.unwrap();
}
