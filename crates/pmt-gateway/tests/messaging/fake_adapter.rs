//! The fake adapter of the messaging harness: records what the gateway
//! sends, edits, reacts and streams; failures and answers can be scripted.

#![allow(dead_code, unreachable_pub)] // each test module uses part of it

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFuture, AdapterLink, Attachment, AudioOp, ChatInfo, ChatType,
    Descriptor, HttpRequest, HttpResponse, Inbound, Interactive, MediaStream, MenuCommand,
    Outgoing, Reaction, SendError, SenderVerdict, Sent, SessionHints, TurnState,
};

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
    /// The client address each webhook request carried.
    pub http_peers: Mutex<Vec<Option<String>>>,
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
    /// Turn states told, in order (`turn <trigger|-> started|ended`).
    pub turns: Mutex<Vec<String>>,
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
            http_peers: Mutex::default(),
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
            turns: Mutex::default(),
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

    fn reach(&self) -> Result<pmt_gateway::messaging::probe::Reach, String> {
        Ok(pmt_gateway::messaging::probe::Reach::InboundOnly)
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

    fn turn_state<'a>(&'a self, turn: &'a TurnState) -> AdapterFuture<'a, ()> {
        let state = if turn.running { "started" } else { "ended" };
        let trigger = turn.trigger.as_deref().unwrap_or("-");
        self.turns
            .lock()
            .unwrap()
            .push(format!("turn {trigger} {state}"));
        Box::pin(async {})
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
        self.http_peers.lock().unwrap().push(
            request
                .header(pmt_gateway::messaging::PEER_HEADER)
                .map(str::to_owned),
        );
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
