//! A messaging gateway over real profile stores on a temporary home, with
//! a fake agent side whose turns are scripted by their prompt text and a
//! fake adapter that records what it sends.

#![allow(dead_code, unreachable_pub)] // each test module uses part of it

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFactory, AdapterFuture, AdapterLink, Admission, Agents,
    Attachment, AudioOp, BusyPolicy, ChatInfo, ChatType, Conversation, Descriptor, Gateway, Hooks,
    HttpRequest, HttpResponse, Inbound, Interactive, Lookup, MediaStream, MenuCommand, Options,
    Outgoing, Reaction, SendError, SenderVerdict, Sent, SessionHints, Speech,
};
use pmt_gateway::{Author, Host, HostFuture, Identity, Link, Server, ServerOptions};
use pmt_rpc::contracts::{
    Admission as RpcAdmission, ApprovalAsk, ApprovalRequestParams, ClarifyAsk,
    ClarifyRequestParams, ClientKind, CommandCatalog, CompactParams, Compacted, CompleteParams,
    Completions, ConfigValue, ContextUsage, Decision, DispatchParams, Dispatched, ForkParams,
    ModelOptions, ResolveParams, SessionCreateParams, SessionList, SessionListParams, SessionRef,
    SessionSetup, SubmitParams, Submitted, Transcript, TurnEnded, TurnFinished,
};
use pmt_store::{EndReason, LineageKind, NewSession, Origin, Source, Store};
use tokio_util::sync::CancellationToken;

/// The fake turns' scripts, by prompt text:
///
/// - `slow:<ms>[:<answer>]` answers after `ms` (or ends interrupted);
/// - `approval` asks one approval and answers `approved` or `denied`;
/// - `clarify` asks a choice question (`red`, `green`, `blue`) and a
///   free-text one, and answers `clarified: <answers as JSON>`;
/// - `clarify-multi` asks a multi-select question of 4 options;
/// - `stream` streams its answer in pieces first;
/// - `say:<text>` answers `<text>` verbatim;
/// - `wait:<n>` announces a wait for a turn slot with `n` ahead first;
/// - `compact` reports a loop step calling `terminal` and `read_file` and
///   a committed compaction (epoch 2), then answers 600 `x`;
/// - anything else answers `echo: <text>` at once.
async fn script(
    text: &str,
    link: &Link,
    cancel: &CancellationToken,
    timeout: Duration,
) -> (&'static str, Option<String>) {
    if text == "approval" {
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
        let answer = link.ask::<ApprovalAsk>(&params, Some(timeout)).await;
        let approved = answer.is_some_and(|a| a.decision != Decision::Deny);
        return (
            "completed",
            Some(if approved { "approved" } else { "denied" }.into()),
        );
    }
    if text == "clarify" {
        let params = ClarifyRequestParams {
            session_id: link.session_id().to_owned(),
            questions: vec![
                serde_json::json!({"question": "Which colour?", "choices": ["red", "green", "blue"]}),
                serde_json::json!({"question": "Why?"}),
            ],
            allow_other: true,
            deadline_ms: None,
        };
        let answer = link.ask::<ClarifyAsk>(&params, Some(timeout)).await;
        let answers = answer.map_or(serde_json::Value::Null, |a| a.answers.into());
        return ("completed", Some(format!("clarified: {answers}")));
    }
    if text == "clarify-multi" {
        let params = ClarifyRequestParams {
            session_id: link.session_id().to_owned(),
            questions: vec![serde_json::json!({"question": "Which days?",
                "choices": ["mon", "tue", "wed", "thu"], "multi": true})],
            allow_other: true,
            deadline_ms: None,
        };
        let answer = link.ask::<ClarifyAsk>(&params, Some(timeout)).await;
        let answers = answer.map_or(serde_json::Value::Null, |a| a.answers.into());
        return ("completed", Some(format!("clarified: {answers}")));
    }
    if text == "stream" {
        for piece in ["hello ", "wor", "ld, this is a streamed answer"] {
            link.delta(piece);
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        return (
            "completed",
            Some("hello world, this is a streamed answer".into()),
        );
    }
    if let Some(rest) = text.strip_prefix("tools:") {
        return tools(rest, link).await;
    }
    if let Some(ahead) = text.strip_prefix("wait:") {
        // The daemon's turn slots announce a wait this way.
        let ahead: u64 = ahead.parse().unwrap_or(1);
        link.emit::<pmt_rpc::contracts::SessionEvent>(
            &serde_json::json!({"type": "turn.waiting", "payload": {"ahead": ahead}}),
        );
        return ("completed", Some(format!("echo: {text}")));
    }
    if text == "compact" {
        let event = |kind: &str, payload: serde_json::Value| {
            link.emit::<pmt_rpc::contracts::SessionEvent>(
                &serde_json::json!({"type": kind, "payload": payload}),
            );
        };
        event(
            "turn.step",
            serde_json::json!({"iteration": 1, "calls": [{"tool": "terminal"}, {"tool": "read_file"}]}),
        );
        event(
            "compression.committed",
            serde_json::json!({"trigger": "threshold", "epoch": 2, "tokens_before": 900,
                "tokens_after": 300, "summary_tokens": 100, "duration_ms": 5}),
        );
        return ("completed", Some("x".repeat(600)));
    }
    if let Some(rest) = text.strip_prefix("say:") {
        return ("completed", Some(rest.to_owned()));
    }
    if text == "fail" {
        return ("provider_error", None);
    }
    if let Some(rest) = text.strip_prefix("slow:") {
        let mut parts = rest.splitn(2, ':');
        let ms: u64 = parts.next().unwrap_or("0").parse().unwrap_or(0);
        let answer = parts.next().unwrap_or("slow done").to_owned();
        return tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(ms)) => ("completed", Some(answer)),
            () = cancel.cancelled() => ("interrupted_by_user", None),
        };
    }
    ("completed", Some(format!("echo: {text}")))
}

/// `tools:<n>[:fail]`: says "Let me look." then calls `terminal` `n`
/// times (as `session.event`s), then answers `tools done` or fails.
async fn tools(rest: &str, link: &Link) -> (&'static str, Option<String>) {
    let mut parts = rest.split(':');
    let n: usize = parts.next().unwrap_or("1").parse().unwrap_or(1);
    let fail = parts.next() == Some("fail");
    let event = |kind: &str, payload: serde_json::Value| {
        link.emit::<pmt_rpc::contracts::SessionEvent>(
            &serde_json::json!({"type": kind, "payload": payload}),
        );
    };
    event(
        "assistant.message",
        serde_json::json!({"text": "Let me look."}),
    );
    for i in 0..n {
        let call = format!("c{i}");
        event(
            "tool.call",
            serde_json::json!({"call_id": call, "tool": "terminal",
                "arguments": {"command": format!("ls /tmp/{i} sk-proj-abcdefghijklmnopqrstuvwxyz")}}),
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
        event(
            "tool.result",
            serde_json::json!({"call_id": call, "output": "ok"}),
        );
    }
    if fail {
        ("provider_error", None)
    } else {
        ("completed", Some("tools done".into()))
    }
}

struct Profile {
    dir: pmt_config::ProfileDir,
    store: Arc<Store>,
}

/// The agent side.
pub struct FakeAgents {
    profiles: BTreeMap<String, Profile>,
    turns: Arc<Mutex<HashMap<String, CancellationToken>>>,
    next: AtomicUsize,
    /// Every turn started: (session, text).
    pub started: Mutex<Vec<(String, String)>>,
    /// Steered texts.
    pub steered: Mutex<Vec<String>>,
    /// Registry commands run: (session, line).
    pub commands: Mutex<Vec<(String, String)>>,
    /// Evictions: (profile, session).
    pub evicted: Mutex<Vec<(String, String)>>,
    /// How long an `approval` turn waits.
    pub approval_timeout: Mutex<Duration>,
    /// Every conversation session created, in order.
    pub conversations: Mutex<Vec<Conversation>>,
    /// The profiles' speech providers.
    pub speech: Mutex<Option<Arc<dyn Speech>>>,
    /// The working directory new sessions record.
    pub cwd: Mutex<Option<String>>,
    /// The commands native menus show.
    pub menu: Mutex<Vec<MenuCommand>>,
    /// The container every session's terminal runs in.
    pub container: Mutex<Option<pmt_gateway::messaging::attachments::ContainerMounts>>,
    /// Every confirmed `/update`.
    pub updates: Mutex<Vec<pmt_gateway::messaging::UpdateFromChat>>,
    /// The author of every turn started.
    pub authors: Mutex<Vec<Author>>,
    /// What the chat model picker offers.
    pub models: Mutex<Vec<pmt_gateway::messaging::ModelChoice>>,
}

impl FakeAgents {
    pub fn store_of(&self, profile: &str) -> Arc<Store> {
        Arc::clone(&self.profiles[profile].store)
    }

    /// Texts of the turns started so far.
    pub fn texts(&self) -> Vec<String> {
        self.started
            .lock()
            .unwrap()
            .iter()
            .map(|(_, t)| t.clone())
            .collect()
    }

    /// The sessions of the turns started so far.
    pub fn turn_sessions(&self) -> Vec<String> {
        self.started
            .lock()
            .unwrap()
            .iter()
            .map(|(s, _)| s.clone())
            .collect()
    }

    pub fn running(&self) -> usize {
        self.turns.lock().unwrap().len()
    }
}

fn not_here<T>() -> HostFuture<'static, T>
where
    T: Send + 'static,
{
    Box::pin(async { Err(Error::new(&codes::RPC_NOT_SERVED, "not in this fake")) })
}

impl Host for FakeAgents {
    fn owns<'a>(&'a self, _: &'a str, _: &'a str) -> HostFuture<'a, bool> {
        Box::pin(async { Ok(true) })
    }

    fn create<'a>(&'a self, _: &'a str, _: SessionCreateParams) -> HostFuture<'a, SessionRef> {
        not_here()
    }

    fn list<'a>(&'a self, _: &'a str, _: SessionListParams) -> HostFuture<'a, SessionList> {
        not_here()
    }

    fn resume<'a>(&'a self, _: &'a str, _: Option<SessionSetup>, _: Link) -> HostFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }

    fn transcript<'a>(&'a self, _: &'a str, _: &'a str) -> HostFuture<'a, Transcript> {
        not_here()
    }

    fn context_usage<'a>(&'a self, _: &'a str, _: &'a str) -> HostFuture<'a, ContextUsage> {
        not_here()
    }

    fn compact<'a>(&'a self, _: &'a str, _: CompactParams) -> HostFuture<'a, Compacted> {
        not_here()
    }

    fn submit<'a>(
        &'a self,
        _: &'a str,
        params: SubmitParams,
        author: Author,
        link: Link,
    ) -> HostFuture<'a, Submitted> {
        self.authors.lock().unwrap().push(author);
        Box::pin(async move {
            let turn = format!("turn-{}", self.next.fetch_add(1, Ordering::Relaxed));
            let session = params.session_id.clone();
            self.started
                .lock()
                .unwrap()
                .push((session.clone(), params.text.clone()));
            let cancel = CancellationToken::new();
            self.turns
                .lock()
                .unwrap()
                .insert(session.clone(), cancel.clone());
            let timeout = *self.approval_timeout.lock().unwrap();
            let turns = Arc::clone(&self.turns);
            let turn_id = turn.clone();
            tokio::spawn(async move {
                let (reason, answer) = script(&params.text, &link, &cancel, timeout).await;
                // Gone before the end is announced, as in the daemon.
                turns.lock().unwrap().remove(&session);
                link.emit::<TurnEnded>(&TurnFinished {
                    turn_id,
                    exit_reason: reason.to_owned(),
                    final_response: answer,
                    error: (reason == "provider_error").then(|| pmt_rpc::contracts::TurnError {
                        kind: "provider".into(),
                        code: "provider.unavailable".into(),
                        message: "the provider failed".into(),
                        hint: None,
                    }),
                    usage: None,
                    report: None,
                });
            });
            Ok(Submitted {
                turn_id: turn,
                admission: RpcAdmission::Started,
                notice: None,
            })
        })
    }

    fn dispatch<'a>(
        &'a self,
        _: &'a str,
        _: DispatchParams,
        _: Link,
    ) -> HostFuture<'a, Dispatched> {
        not_here()
    }

    fn interrupt<'a>(&'a self, _: &'a str, session_id: &'a str) -> HostFuture<'a, ()> {
        if let Some(cancel) = self.turns.lock().unwrap().get(session_id) {
            cancel.cancel();
        }
        Box::pin(async { Ok(()) })
    }

    fn resolve<'a>(&'a self, _: &'a str, _: ResolveParams) -> HostFuture<'a, SessionRef> {
        not_here()
    }

    fn fork<'a>(&'a self, profile: &'a str, params: ForkParams) -> HostFuture<'a, SessionRef> {
        Box::pin(async move {
            let store = self
                .store(profile)
                .ok_or_else(|| Error::new(&codes::PROFILE_NOT_FOUND, "no profile"))?;
            let fork = store.fork_session(params.session_id.parse()?, None, None)?;
            Ok(SessionRef {
                session_id: fork.id.to_string(),
            })
        })
    }

    fn commands<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: ClientKind,
    ) -> HostFuture<'a, CommandCatalog> {
        not_here()
    }

    fn complete_path<'a>(&'a self, _: &'a str, _: CompleteParams) -> HostFuture<'a, Completions> {
        not_here()
    }

    fn model_options<'a>(&'a self, _: &'a str, _: &'a str) -> HostFuture<'a, ModelOptions> {
        not_here()
    }

    fn config_get<'a>(&'a self, _: &'a str, _: &'a str) -> HostFuture<'a, ConfigValue> {
        not_here()
    }

    fn close_idle<'a>(&'a self, _: &'a str, _: &'a str) -> HostFuture<'a, bool> {
        Box::pin(async { Ok(false) })
    }

    fn load(&self) -> (usize, usize) {
        (0, self.running())
    }

    fn drain(&self, _: Duration) -> Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
}

/// The registry the fake knows: name, aliases, busy policy.
const REGISTRY: &[(&str, &[&str], BusyPolicy)] = &[
    ("help", &[], BusyPolicy::Immediate),
    ("new", &["reset"], BusyPolicy::InterruptThenDispatch),
    ("stop", &[], BusyPolicy::InterruptThenDispatch),
    ("queue", &[], BusyPolicy::Immediate),
    ("approve", &[], BusyPolicy::Immediate),
    ("deny", &[], BusyPolicy::Immediate),
    ("whoami", &[], BusyPolicy::Immediate),
    ("pair", &[], BusyPolicy::Immediate),
    ("pause", &[], BusyPolicy::Immediate),
    ("sethome", &["set-home"], BusyPolicy::Immediate),
    ("policy", &[], BusyPolicy::Immediate),
    ("model", &[], BusyPolicy::Reject),
    ("usage", &[], BusyPolicy::Immediate),
    ("retry", &[], BusyPolicy::InterruptThenDispatch),
    ("commands", &[], BusyPolicy::Immediate),
    ("busy", &[], BusyPolicy::Immediate),
    ("discard", &[], BusyPolicy::Immediate),
    ("platforms", &["gateway"], BusyPolicy::Immediate),
    ("platform", &[], BusyPolicy::Immediate),
    ("restart", &[], BusyPolicy::Immediate),
    ("update", &[], BusyPolicy::Immediate),
    ("verbose", &[], BusyPolicy::Immediate),
    ("footer", &[], BusyPolicy::Immediate),
    ("sessions", &[], BusyPolicy::Immediate),
    ("resume", &[], BusyPolicy::Reject),
    ("background", &["bg"], BusyPolicy::Immediate),
    ("fork", &["branch"], BusyPolicy::Reject),
    ("topic", &[], BusyPolicy::Immediate),
];

impl Agents for FakeAgents {
    fn store(&self, profile: &str) -> Option<Arc<Store>> {
        self.profiles.get(profile).map(|p| Arc::clone(&p.store))
    }

    /// Read from disk at every call, as the daemon's reload would.
    fn scope(&self, profile: &str) -> Option<ProfileScope> {
        let dir = self.profiles.get(profile)?.dir.clone();
        ProfileScope::open(dir, &Overrides::default(), Launch::Other).ok()
    }

    fn create_conversation<'a>(
        &'a self,
        profile: &'a str,
        conversation: Conversation,
    ) -> HostFuture<'a, String> {
        self.conversations
            .lock()
            .unwrap()
            .push(conversation.clone());
        Box::pin(async move {
            let store = self
                .store(profile)
                .ok_or_else(|| Error::new(&codes::PROFILE_NOT_FOUND, "no profile"))?;
            let mut new = match &conversation.parent {
                Some(parent) => {
                    NewSession::child(Source::Gateway, parent.parse()?, LineageKind::Reset)
                }
                None => NewSession::new(Source::Gateway),
            };
            new.cwd.clone_from(&self.cwd.lock().unwrap());
            new.origin = Origin::Gateway {
                platform: conversation.platform,
                chat: conversation.chat_id,
                thread: conversation.thread_id,
                user: Some(conversation.user_id),
            };
            Ok(store.create_session(&new)?.id.to_string())
        })
    }

    fn steer<'a>(&'a self, _: &'a str, session_id: &'a str, text: &'a str) -> HostFuture<'a, bool> {
        let running = self.turns.lock().unwrap().contains_key(session_id);
        if running {
            self.steered.lock().unwrap().push(text.to_owned());
        }
        Box::pin(async move { Ok(running) })
    }

    fn lookup<'a>(&'a self, _: &'a str, _: &'a str, line: &'a str) -> HostFuture<'a, Lookup> {
        let line = line.trim().trim_start_matches('/');
        let (token, args) = line.split_once(' ').unwrap_or((line, ""));
        let token = token.to_lowercase();
        let found = REGISTRY
            .iter()
            .find(|(name, aliases, _)| *name == token || aliases.contains(&token.as_str()));
        let lookup = match found {
            Some((name, _, busy)) => Lookup::Command {
                name: (*name).to_owned(),
                args: args.trim().to_owned(),
                busy: *busy,
            },
            None if token == "theme" => Lookup::Reply("/theme is not available here".into()),
            None => Lookup::Text,
        };
        Box::pin(async move { Ok(lookup) })
    }

    fn run_command<'a>(
        &'a self,
        _: &'a str,
        session_id: &'a str,
        line: &'a str,
        _: Link,
    ) -> HostFuture<'a, Dispatched> {
        self.commands
            .lock()
            .unwrap()
            .push((session_id.to_owned(), line.to_owned()));
        let name = line
            .trim_start_matches('/')
            .split(' ')
            .next()
            .unwrap_or_default();
        let last = self
            .started
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(s, _)| s == session_id)
            .map_or_else(|| "the last message again".to_owned(), |(_, t)| t.clone());
        let outcome = match name {
            "retry" => Dispatched::Send {
                message: last,
                images: Vec::new(),
            },
            "model" => Dispatched::Done {
                notice: "model switched".into(),
            },
            other => Dispatched::Done {
                notice: format!("ran /{other}"),
            },
        };
        Box::pin(async move { Ok(outcome) })
    }

    fn evict<'a>(&'a self, profile: &'a str, session_id: &'a str) -> HostFuture<'a, bool> {
        let busy = self.turns.lock().unwrap().contains_key(session_id);
        if !busy {
            self.evicted
                .lock()
                .unwrap()
                .push((profile.to_owned(), session_id.to_owned()));
        }
        Box::pin(async move { Ok(!busy) })
    }

    fn serve_new_profiles(&self) -> Vec<String> {
        Vec::new()
    }

    fn model_choices(&self, _profile: &str) -> Vec<pmt_gateway::messaging::ModelChoice> {
        self.models.lock().unwrap().clone()
    }

    fn menu_commands(&self, _profile: &str) -> Vec<MenuCommand> {
        self.menu.lock().unwrap().clone()
    }

    fn container_mounts(
        &self,
        _profile: &str,
        _session: &str,
    ) -> Option<pmt_gateway::messaging::attachments::ContainerMounts> {
        self.container.lock().unwrap().clone()
    }

    fn speech(&self, _profile: &str) -> Option<Arc<dyn Speech>> {
        self.speech.lock().unwrap().clone()
    }

    fn update_from_chat<'a>(
        &'a self,
        request: &'a pmt_gateway::messaging::UpdateFromChat,
    ) -> HostFuture<'a, String> {
        self.updates.lock().unwrap().push(request.clone());
        Box::pin(async { Ok("update started".to_owned()) })
    }

    fn end_conversation<'a>(&'a self, profile: &'a str, session_id: &'a str) -> HostFuture<'a, ()> {
        Box::pin(async move {
            let store = self
                .store(profile)
                .ok_or_else(|| Error::new(&codes::PROFILE_NOT_FOUND, "no profile"))?;
            store.end_session(session_id.parse()?, EndReason::NewSession)?;
            Ok(())
        })
    }
}

/// What the fake adapter did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Did {
    /// A new message.
    Send(Outgoing),
    /// An edit of message `id`.
    Edit(String, Outgoing),
}

/// A scripted platform: records sends; failures can be queued.
pub struct FakeAdapter {
    pub descriptor: Mutex<Descriptor>,
    pub did: Mutex<Vec<Did>>,
    /// Results the next sends get (then success).
    pub failures: Mutex<VecDeque<SendError>>,
    /// What `connect` answers.
    pub connect_error: Mutex<Option<AdapterError>>,
    pub link: Mutex<Option<Arc<dyn AdapterLink>>>,
    next: AtomicUsize,
    /// When each typing indicator call started.
    pub typing: Mutex<Vec<std::time::Instant>>,
    /// How long a typing call takes (then fails).
    pub typing_delay: Mutex<Duration>,
    /// Inbound media by location: (declared length, chunks, chunk size).
    pub media: Mutex<HashMap<String, MediaScript>>,
    /// Streamed audio operations.
    pub audio: Mutex<Vec<AudioOp>>,
    /// `edit` answers `unsupported` although declared.
    pub edit_unsupported: std::sync::atomic::AtomicBool,
    /// Edits attempted.
    pub edits_tried: AtomicUsize,
    /// The credential identity it connects with.
    pub credential: Mutex<Option<String>>,
    /// The shared secret a webhook request must carry in `x-secret`.
    pub http_secret: Mutex<Option<String>>,
    /// What its sender check answers, by sender (`unknown` otherwise).
    pub verdicts: Mutex<HashMap<String, Result<SenderVerdict, AdapterError>>>,
    /// Sender checks asked.
    pub sender_checks: AtomicUsize,
    /// Every command menu registered.
    pub menus: Mutex<Vec<Vec<MenuCommand>>>,
    /// Every reaction added or removed.
    pub reactions: Mutex<Vec<Reaction>>,
    /// What new sessions of a thread start with, by thread id.
    pub hints: Mutex<HashMap<String, SessionHints>>,
    /// Messages deleted, by id.
    pub deleted: Mutex<Vec<String>>,
    /// What `member_roles` answers, by sender (an error otherwise).
    pub roles: Mutex<HashMap<String, Vec<String>>>,
    /// What `fetch_history` answers.
    pub history: Mutex<Vec<pmt_gateway::messaging::HistoryEntry>>,
    /// Threads opened: (chat, title, thread id); renames: (thread, title).
    pub threads: Mutex<Vec<(String, String, String)>>,
    pub renamed: Mutex<Vec<(String, String)>>,
    /// Native stream operations, in order (`start <id>`, `append <text>`,
    /// `task <id> <status>`, `stop <id>`, `status <text|none>`).
    pub streamed: Mutex<Vec<String>>,
    /// Stream appends that succeed before the next ones fail.
    pub appends_before_failure: Mutex<Option<usize>>,
    /// Replies only one member saw.
    pub privately: Mutex<Vec<pmt_gateway::messaging::Private>>,
    /// `reply_privately` fails (the platform could not reach the member).
    pub private_fails: std::sync::atomic::AtomicBool,
}

impl FakeAdapter {
    pub fn new(platform: &str) -> Arc<Self> {
        Arc::new(Self {
            descriptor: Mutex::new(Descriptor {
                supports_edit: true,
                interactive: Interactive::Native,
                ..Descriptor::new(platform, 4096)
            }),
            did: Mutex::default(),
            failures: Mutex::default(),
            connect_error: Mutex::default(),
            link: Mutex::default(),
            next: AtomicUsize::new(1),
            typing: Mutex::default(),
            typing_delay: Mutex::new(Duration::ZERO),
            media: Mutex::default(),
            audio: Mutex::default(),
            edit_unsupported: std::sync::atomic::AtomicBool::new(false),
            edits_tried: AtomicUsize::new(0),
            credential: Mutex::default(),
            http_secret: Mutex::default(),
            verdicts: Mutex::default(),
            sender_checks: AtomicUsize::new(0),
            menus: Mutex::default(),
            hints: Mutex::default(),
            reactions: Mutex::default(),
            deleted: Mutex::default(),
            roles: Mutex::default(),
            history: Mutex::default(),
            threads: Mutex::default(),
            renamed: Mutex::default(),
            streamed: Mutex::default(),
            appends_before_failure: Mutex::default(),
            privately: Mutex::default(),
            private_fails: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Texts of new messages (not edits), in order.
    pub fn texts(&self) -> Vec<String> {
        self.did
            .lock()
            .unwrap()
            .iter()
            .filter_map(|d| match d {
                Did::Send(m) => Some(m.text.clone()),
                Did::Edit(..) => None,
            })
            .collect()
    }

    /// Everything it did.
    pub fn all(&self) -> Vec<Did> {
        self.did.lock().unwrap().clone()
    }
}

impl Adapter for FakeAdapter {
    fn describe(&self) -> Descriptor {
        self.descriptor.lock().unwrap().clone()
    }

    fn register_commands<'a>(
        &'a self,
        commands: &'a [MenuCommand],
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        self.menus.lock().unwrap().push(commands.to_vec());
        Box::pin(async { Ok(()) })
    }

    fn react<'a>(&'a self, reaction: &'a Reaction) -> AdapterFuture<'a, Result<(), SendError>> {
        let supported = self.descriptor.lock().unwrap().supports_reactions;
        if supported {
            self.reactions.lock().unwrap().push(reaction.clone());
        }
        Box::pin(async move {
            if supported {
                Ok(())
            } else {
                Err(SendError::Unsupported)
            }
        })
    }

    fn open_thread<'a>(
        &'a self,
        chat_id: &'a str,
        title: &'a str,
    ) -> AdapterFuture<'a, Result<String, SendError>> {
        let supported = self.descriptor.lock().unwrap().supports_threads;
        let mut threads = self.threads.lock().unwrap();
        let id = format!("th-{}", threads.len() + 1);
        if supported {
            threads.push((chat_id.to_owned(), title.to_owned(), id.clone()));
        }
        Box::pin(async move {
            if supported {
                Ok(id)
            } else {
                Err(SendError::Unsupported)
            }
        })
    }

    fn reply_privately<'a>(
        &'a self,
        to: &'a pmt_gateway::messaging::Private,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        if self.private_fails.load(Ordering::Relaxed) {
            return Box::pin(async { Err(SendError::Permanent("403 blocked".into())) });
        }
        self.privately.lock().unwrap().push(to.clone());
        Box::pin(async { Ok(()) })
    }

    fn rename_thread<'a>(
        &'a self,
        _chat_id: &'a str,
        thread_id: &'a str,
        title: &'a str,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        self.renamed
            .lock()
            .unwrap()
            .push((thread_id.to_owned(), title.to_owned()));
        Box::pin(async { Ok(()) })
    }

    fn stream_start<'a>(&'a self, _: &'a Outgoing) -> AdapterFuture<'a, Result<String, SendError>> {
        let id = format!("s{}", self.next.fetch_add(1, Ordering::Relaxed));
        self.streamed.lock().unwrap().push(format!("start {id}"));
        Box::pin(async move { Ok(id) })
    }

    fn stream_append<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        text: &'a str,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        let mut left = self.appends_before_failure.lock().unwrap();
        let refused = match left.as_mut() {
            Some(0) => true,
            Some(n) => {
                *n -= 1;
                false
            }
            None => false,
        };
        if !refused {
            self.streamed.lock().unwrap().push(format!("append {text}"));
        }
        Box::pin(async move {
            if refused {
                Err(SendError::Server("stream refused".into()))
            } else {
                Ok(())
            }
        })
    }

    fn stream_task<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        task: &'a pmt_gateway::messaging::TaskCard,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        self.streamed
            .lock()
            .unwrap()
            .push(format!("task {} {:?}", task.id, task.status));
        Box::pin(async { Ok(()) })
    }

    fn stream_stop<'a>(
        &'a self,
        _: &'a str,
        id: &'a str,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        self.streamed.lock().unwrap().push(format!("stop {id}"));
        Box::pin(async { Ok(()) })
    }

    fn set_status<'a>(
        &'a self,
        _: &'a str,
        _: Option<&'a str>,
        status: Option<&'a str>,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        self.streamed
            .lock()
            .unwrap()
            .push(format!("status {}", status.unwrap_or("none")));
        Box::pin(async { Ok(()) })
    }

    fn session_hints(&self, _chat_id: &str, thread_id: Option<&str>) -> SessionHints {
        thread_id
            .and_then(|t| self.hints.lock().unwrap().get(t).cloned())
            .unwrap_or_default()
    }

    fn connect(&self, link: Arc<dyn AdapterLink>) -> AdapterFuture<'_, Result<(), AdapterError>> {
        let error = self.connect_error.lock().unwrap().clone();
        if error.is_none() {
            *self.link.lock().unwrap() = Some(link);
        }
        Box::pin(async move { error.map_or(Ok(()), Err) })
    }

    fn disconnect(&self) -> AdapterFuture<'_, ()> {
        Box::pin(async {})
    }

    fn send(&self, message: &Outgoing) -> AdapterFuture<'_, Result<Sent, SendError>> {
        let failure = self.failures.lock().unwrap().pop_front();
        let outcome = if let Some(err) = failure {
            Err(err)
        } else {
            self.did.lock().unwrap().push(Did::Send(message.clone()));
            let id = self.next.fetch_add(1, Ordering::Relaxed);
            Ok(Sent {
                message_id: Some(format!("m{id}")),
            })
        };
        Box::pin(async move { outcome })
    }

    fn chat_info<'a>(&'a self, chat_id: &'a str) -> AdapterFuture<'a, Result<ChatInfo, SendError>> {
        Box::pin(async move {
            Ok(ChatInfo {
                name: chat_id.to_owned(),
                chat_type: ChatType::Dm,
            })
        })
    }

    fn edit<'a>(
        &'a self,
        message_id: &'a str,
        message: &'a Outgoing,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        self.edits_tried.fetch_add(1, Ordering::Relaxed);
        if self.edit_unsupported.load(Ordering::Relaxed) {
            return Box::pin(async { Err(SendError::Unsupported) });
        }
        self.did
            .lock()
            .unwrap()
            .push(Did::Edit(message_id.to_owned(), message.clone()));
        Box::pin(async { Ok(()) })
    }

    fn member_roles<'a>(
        &'a self,
        inbound: &'a Inbound,
    ) -> AdapterFuture<'a, Result<Vec<String>, SendError>> {
        let roles = self.roles.lock().unwrap().get(&inbound.sender_id).cloned();
        Box::pin(
            async move { roles.ok_or_else(|| SendError::Server("member lookup failed".into())) },
        )
    }

    fn fetch_history<'a>(
        &'a self,
        _chat_id: &'a str,
        _thread_id: Option<&'a str>,
        limit: usize,
    ) -> AdapterFuture<'a, Result<Vec<pmt_gateway::messaging::HistoryEntry>, SendError>> {
        let all = self.history.lock().unwrap().clone();
        let skip = all.len().saturating_sub(limit);
        Box::pin(async move { Ok(all.into_iter().skip(skip).collect()) })
    }

    fn delete<'a>(
        &'a self,
        _chat_id: &'a str,
        _thread_id: Option<&'a str>,
        message_id: &'a str,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        self.deleted.lock().unwrap().push(message_id.to_owned());
        Box::pin(async { Ok(()) })
    }

    fn send_typing<'a>(
        &'a self,
        _chat_id: &'a str,
        _thread_id: Option<&'a str>,
    ) -> AdapterFuture<'a, Result<(), SendError>> {
        self.typing.lock().unwrap().push(std::time::Instant::now());
        let delay = *self.typing_delay.lock().unwrap();
        Box::pin(async move {
            tokio::time::sleep(delay).await;
            Err(SendError::Network("typing failed".into()))
        })
    }

    fn credential_identity(&self) -> Option<String> {
        self.credential.lock().unwrap().clone()
    }

    fn open_media<'a>(
        &'a self,
        attachment: &'a Attachment,
    ) -> AdapterFuture<'a, Result<MediaStream, SendError>> {
        let found = self
            .media
            .lock()
            .unwrap()
            .get(&attachment.location)
            .copied();
        Box::pin(async move {
            let (declared_len, chunks, size) = found.ok_or(SendError::Unsupported)?;
            let body = futures_util::stream::iter((0..chunks).map(move |_| Ok(vec![7u8; size])));
            Ok(MediaStream {
                declared_len,
                chunks: Box::pin(body),
            })
        })
    }

    fn audio<'a>(&'a self, op: &'a AudioOp) -> AdapterFuture<'a, Result<(), SendError>> {
        self.audio.lock().unwrap().push(op.clone());
        Box::pin(async { Ok(()) })
    }

    fn authorize_sender<'a>(
        &'a self,
        inbound: &'a Inbound,
    ) -> AdapterFuture<'a, Result<SenderVerdict, AdapterError>> {
        self.sender_checks.fetch_add(1, Ordering::Relaxed);
        let verdict = self
            .verdicts
            .lock()
            .unwrap()
            .get(&inbound.sender_id)
            .cloned();
        Box::pin(async move { verdict.unwrap_or(Ok(SenderVerdict::Unknown)) })
    }

    fn verify_http(&self, request: &HttpRequest) -> bool {
        let secret = self.http_secret.lock().unwrap().clone();
        secret.is_some_and(|s| request.header("x-secret") == Some(s.as_str()))
    }

    fn handle_http<'a>(&'a self, request: &'a HttpRequest) -> AdapterFuture<'a, HttpResponse> {
        let link = self.link.lock().unwrap().clone();
        let text = String::from_utf8_lossy(&request.body).into_owned();
        Box::pin(async move {
            if let Some(link) = link {
                let mut event = Inbound::text(ChatType::Dm, "hook", "hook", &text);
                event.message_id = format!("h{}", text.len());
                link.submit(event).await;
            }
            HttpResponse::status(200)
        })
    }
}

/// An inbound file the fake serves: (declared length, chunks, chunk
/// size).
pub type MediaScript = (Option<u64>, usize, usize);

/// Scripted speech providers.
#[derive(Default)]
pub struct FakeSpeech {
    /// What transcription answers.
    pub transcript: Mutex<Option<Result<String, String>>>,
    /// Texts synthesized.
    pub spoken: Mutex<Vec<String>>,
}

impl Speech for FakeSpeech {
    fn transcribe<'a>(
        &'a self,
        _path: &'a std::path::Path,
    ) -> AdapterFuture<'a, Result<String, String>> {
        let answer = self
            .transcript
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| Err("no transcript".into()));
        Box::pin(async move { answer })
    }

    fn synthesize<'a>(
        &'a self,
        text: &'a str,
        out_dir: &'a std::path::Path,
    ) -> AdapterFuture<'a, Result<std::path::PathBuf, String>> {
        self.spoken.lock().unwrap().push(text.to_owned());
        let n = self.spoken.lock().unwrap().len();
        let path = out_dir.join(format!("speech-{n}.ogg"));
        Box::pin(async move {
            std::fs::write(&path, b"OggS").map_err(|e| e.to_string())?;
            Ok(path)
        })
    }
}

/// A gateway on a temporary home.
pub struct Harness {
    pub home: tempfile::TempDir,
    pub agents: Arc<FakeAgents>,
    pub server: Arc<Server>,
    pub gateway: Gateway,
    pub adapters: BTreeMap<(String, String), Arc<FakeAdapter>>,
}

/// How a harness starts.
pub struct Setup {
    /// Profiles and their `config.toml`.
    pub profiles: Vec<(&'static str, String)>,
    /// Hooks.
    pub hooks: Arc<Hooks>,
    /// `on_all_adapters_down = "exit"`.
    pub exit_when_down: bool,
    /// Adapters that fail connecting, by (profile, platform).
    pub connect_errors: Vec<((String, String), AdapterError)>,
    /// Drain timeout.
    pub drain: Duration,
    /// Adapters without editing, by platform.
    pub no_edit: Vec<&'static str>,
    /// Platforms whose direct adapter is not expected to connect.
    pub unconnected: Vec<&'static str>,
    /// The relay connector.
    pub relay: Option<pmt_gateway::messaging::RelayOptions>,
    /// The health export.
    pub health: Option<pmt_gateway::messaging::health::HealthOptions>,
    /// Descriptors replacing the fake's, by platform.
    pub descriptors: Vec<Descriptor>,
    /// Credential identities, by (profile, platform).
    pub credentials: Vec<((&'static str, &'static str), String)>,
    /// Take platform credential locks (in the home).
    pub locks: bool,
}

impl Setup {
    /// One `default` profile with `config`.
    pub fn new(config: &str) -> Self {
        Self {
            profiles: vec![("default", config.to_owned())],
            hooks: Arc::new(Hooks::default()),
            exit_when_down: false,
            connect_errors: Vec::new(),
            drain: Duration::from_secs(2),
            no_edit: Vec::new(),
            unconnected: Vec::new(),
            relay: None,
            health: None,
            descriptors: Vec::new(),
            credentials: Vec::new(),
            locks: false,
        }
    }
}

/// A `config.toml` enabling `platform` with `table` keys (allowing every
/// sender unless `table` says otherwise), no debounce, and group messages
/// answered without a mention (the gating tests set their own).
pub fn config(platform: &str, table: &str) -> String {
    format!(
        "[gateway.busy]\ndebounce_ms = 0\n\n[gateway.groups]\nrequire_mention = false\n\n\
         [gateway.platforms.{platform}]\nenabled = true\n{}{table}\n",
        required_options(platform)
    )
}

/// The settings an enabled first-party platform cannot load without (the
/// fake adapter stands in for the real one).
fn required_options(platform: &str) -> &'static str {
    match platform {
        "email" => {
            "options = { address = \"agent@example.org\", imap_host = \"imap.example.org\", smtp_host = \"smtp.example.org\", require_authenticated_sender = false }\n"
        }
        "matrix" => "options = { homeserver = \"https://matrix.example.org\" }\n",
        "signal" => {
            "options = { http_url = \"http://127.0.0.1:8080\", account = \"+15550000001\" }\n"
        }
        _ => "",
    }
}

/// A `config.toml` where `platform` admits everyone.
pub fn open(platform: &str) -> String {
    config(platform, "allow_all = true")
}

fn identity() -> Identity {
    Identity {
        version: "0.0.0".into(),
        build_id: "test".into(),
        target: "test".into(),
        tier: "tier1".into(),
    }
}

fn agents_for(home: &std::path::Path, setup: &Setup) -> Arc<FakeAgents> {
    let layout = Home::open(home).unwrap();
    let mut profiles = BTreeMap::new();
    for (name, config) in &setup.profiles {
        let dir = layout.profile(&ProfileName::parse(name).unwrap()).unwrap();
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        let scope = ProfileScope::open(dir.clone(), &Overrides::default(), Launch::Other).unwrap();
        let store = Arc::new(Store::open(&scope).unwrap());
        profiles.insert((*name).to_owned(), Profile { dir, store });
    }
    Arc::new(FakeAgents {
        profiles,
        turns: Arc::default(),
        next: AtomicUsize::new(0),
        started: Mutex::default(),
        steered: Mutex::default(),
        commands: Mutex::default(),
        evicted: Mutex::default(),
        approval_timeout: Mutex::new(Duration::from_secs(30)),
        conversations: Mutex::default(),
        speech: Mutex::default(),
        cwd: Mutex::default(),
        menu: Mutex::default(),
        container: Mutex::default(),
        updates: Mutex::default(),
        authors: Mutex::default(),
        models: Mutex::default(),
    })
}

/// The fake adapter of `(profile, platform)` as `setup` describes it.
fn fake_adapter(setup: &Setup, key: &(String, String)) -> Arc<FakeAdapter> {
    let (name, platform) = key;
    let adapter = FakeAdapter::new(platform);
    if setup.no_edit.contains(&platform.as_str()) {
        adapter.descriptor.lock().unwrap().supports_edit = false;
    }
    if let Some(descriptor) = setup.descriptors.iter().find(|d| &d.platform == platform) {
        *adapter.descriptor.lock().unwrap() = descriptor.clone();
    }
    if let Some((_, credential)) = setup
        .credentials
        .iter()
        .find(|((p, pl), _)| p == name && pl == platform)
    {
        *adapter.credential.lock().unwrap() = Some(credential.clone());
    }
    if let Some((_, err)) = setup.connect_errors.iter().find(|(k, _)| k == key) {
        *adapter.connect_error.lock().unwrap() = Some(err.clone());
    }
    adapter
}

impl Harness {
    pub async fn start(setup: Setup) -> Self {
        Self::start_in(tempfile::tempdir().unwrap(), setup).await
    }

    /// A gateway over an existing home (a restart).
    pub async fn start_in(home: tempfile::TempDir, setup: Setup) -> Self {
        let agents = agents_for(home.path(), &setup);
        let names: Vec<String> = setup
            .profiles
            .iter()
            .map(|(n, _)| (*n).to_owned())
            .collect();
        let host: Arc<dyn Host> = Arc::clone(&agents) as Arc<dyn Host>;
        let server = Server::new(ServerOptions::new(identity(), names.clone()), host).unwrap();
        let mut adapters = BTreeMap::new();
        for name in &names {
            for platform in agents
                .scope(name)
                .unwrap()
                .config()
                .gateway
                .platforms
                .keys()
            {
                let key = (name.clone(), platform.clone());
                let adapter = fake_adapter(&setup, &key);
                adapters.insert(key, adapter);
            }
        }
        let mut options = Options::new(home.path());
        for (_, platform) in adapters.keys() {
            let by_key = adapters.clone();
            let factory: AdapterFactory = Arc::new(move |setup| {
                let key = (setup.scope.name().to_string(), setup.platform.clone());
                Ok(Arc::clone(&by_key[&key]) as Arc<dyn Adapter>)
            });
            options.factories.insert(platform.clone(), factory);
        }
        options.hooks = Arc::clone(&setup.hooks);
        options.relay.clone_from(&setup.relay);
        options.health.clone_from(&setup.health);
        options.drain_timeout = setup.drain;
        options.http.listen = std::net::SocketAddr::from(([127, 0, 0, 1], 0));
        if setup.locks {
            options.locks_dir = Some(home.path().join("user-runtime").join("locks"));
        }
        if setup.exit_when_down {
            options.on_all_adapters_down = pmt_config::daemon::AllAdaptersDown::Exit;
        }
        let gateway = Gateway::start(
            Arc::clone(&server),
            Arc::clone(&agents) as Arc<dyn Agents>,
            options,
        )
        .await
        .unwrap();
        let harness = Self {
            home,
            agents,
            server,
            gateway,
            adapters,
        };
        harness.connected(&setup.unconnected).await;
        harness
    }

    /// Waits until every adapter that can connect has.
    async fn connected(&self, unconnected: &[&str]) {
        for ((_, platform), adapter) in &self.adapters {
            if adapter.connect_error.lock().unwrap().is_some()
                || unconnected.contains(&platform.as_str())
            {
                continue;
            }
            eventually("the adapter connects", || {
                adapter.link.lock().unwrap().is_some()
            })
            .await;
        }
    }

    pub fn adapter(&self, platform: &str) -> Arc<FakeAdapter> {
        self.adapter_of("default", platform)
    }

    pub fn adapter_of(&self, profile: &str, platform: &str) -> Arc<FakeAdapter> {
        Arc::clone(&self.adapters[&(profile.to_owned(), platform.to_owned())])
    }

    pub fn store(&self) -> Arc<Store> {
        self.agents.store_of("default")
    }

    /// Submits `inbound` through the adapter's link, as the platform would.
    pub async fn send(&self, platform: &str, inbound: Inbound) -> Admission {
        self.send_as("default", platform, inbound).await
    }

    pub async fn send_as(&self, profile: &str, platform: &str, inbound: Inbound) -> Admission {
        let link = self
            .adapter_of(profile, platform)
            .link
            .lock()
            .unwrap()
            .clone()
            .expect("the adapter is connected");
        link.submit(inbound).await
    }

    /// A DM from `sender` on `platform`.
    pub async fn dm(&self, platform: &str, sender: &str, text: &str) -> Admission {
        self.send(platform, Inbound::text(ChatType::Dm, sender, sender, text))
            .await
    }

    /// The sessions of the default profile.
    pub fn sessions(&self) -> Vec<SessionId> {
        self.sessions_of("default")
    }

    pub fn sessions_of(&self, profile: &str) -> Vec<SessionId> {
        let store = self.agents.store_of(profile);
        store
            .list_sessions(&pmt_store::ListOptions {
                limit: Some(100),
                ..pmt_store::ListOptions::default()
            })
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect()
    }

    /// Stops the gateway and hands back its home (for a restart).
    pub async fn stop(self) -> tempfile::TempDir {
        self.gateway.shutdown().await;
        self.home
    }
}

/// Waits up to ten seconds for `check`.
pub async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !check() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting: {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Waits until the adapter sent a message containing `text`.
pub async fn said(adapter: &FakeAdapter, text: &str) {
    eventually(&format!("a message containing {text:?}"), || {
        adapter.texts().iter().any(|t| t.contains(text))
    })
    .await;
}

/// Captures every log line of the test's thread while the guard lives.
#[derive(Clone, Default)]
pub struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    /// Starts capturing (the test must run on a current-thread runtime).
    pub fn capture() -> (Self, tracing::subscriber::DefaultGuard) {
        let logs = Self::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        (logs, tracing::subscriber::set_default(subscriber))
    }

    /// Everything logged so far.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}
