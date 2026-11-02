//! The first-party email adapter (spec: platform-adapters, First-party
//! platforms and typed platform names; First-party adapter transports and
//! required settings; Email adapter behaviour): IMAP polling of the inbox
//! every `options.poll_interval_secs` and SMTP replies, as
//! `options.address` with the password in the profile's secret
//! `EMAIL_PASSWORD` (or the table's `token_secret`). The first connect
//! marks mail already in the inbox seen; afterwards every unseen mail is
//! answered, also mail that arrived while disconnected. Long replies go
//! out whole (`splits_long_messages`); choices are numbered.

mod imap;
mod mime;
pub mod mock;
mod smtp;
mod wire;

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use imap::{Imap, ImapError, Login};
use mime::{Draft, File, Thread};
use pmt_config::ProfileScope;
use pmt_config::platforms::EmailOptions;
use pmt_core::error::Error;
use pmt_gateway::messaging::adapter::credential;
use pmt_gateway::messaging::{
    Adapter, AdapterError, AdapterFactory, AdapterFuture, AdapterLink, AdapterSetup, AdapterState,
    Attachment, ChatInfo, ChatType, Descriptor, MediaKind, MediaStream, Outgoing, SendError, Sent,
};
use tokio_util::sync::CancellationToken;

/// The platform name.
pub const PLATFORM: &str = "email";
/// The secret holding the mailbox password unless the table names another.
pub const PASSWORD_SECRET: &str = "EMAIL_PASSWORD";
/// Threads remembered for replies.
const THREADS: usize = 1000;

/// The factory the daemon registers for `email`.
#[must_use]
pub fn factory() -> AdapterFactory {
    Arc::new(|setup| Ok(Arc::new(Email::new(setup)?) as Arc<dyn Adapter>))
}

/// The mailbox settings every connection needs.
#[derive(Debug, Clone)]
struct Mailbox {
    address: String,
    user: String,
    imap_host: String,
    smtp_host: String,
}

/// Inbound threads, so replies thread under the mail they answer.
#[derive(Debug, Default)]
struct Threads {
    by_message: HashMap<String, Thread>,
    latest: HashMap<String, Thread>,
    order: VecDeque<String>,
}

impl Threads {
    fn remember(&mut self, chat: &str, thread: Thread) {
        self.latest.insert(chat.to_owned(), thread.clone());
        self.order.push_back(thread.message_id.clone());
        self.by_message.insert(thread.message_id.clone(), thread);
        while self.order.len() > THREADS {
            if let Some(old) = self.order.pop_front() {
                self.by_message.remove(&old);
            }
        }
    }
}

/// What the adapter and its poller share.
struct Inner {
    scope: ProfileScope,
    secret: String,
    options: EmailOptions,
    extra_ca: Option<PathBuf>,
    /// Mail already in the inbox was marked seen (first connect only).
    primed: AtomicBool,
    threads: Mutex<Threads>,
    /// Inbound attachments until the gateway opens them.
    parts: Mutex<HashMap<String, Vec<u8>>>,
    poller: Mutex<Option<CancellationToken>>,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("profile", &self.scope.name().as_str())
            .field("address", &self.options.address)
            .finish_non_exhaustive()
    }
}

fn missing(key: &str) -> AdapterError {
    AdapterError {
        retryable: false,
        code: "missing_option".to_owned(),
        message: format!("gateway.platforms.email.options.{key} is required"),
    }
}

impl Inner {
    fn new(setup: &AdapterSetup) -> Result<Self, Error> {
        let options: EmailOptions =
            pmt_config::platforms::options(PLATFORM, &setup.config.options)?;
        Ok(Self {
            scope: setup.scope.clone(),
            secret: setup
                .config
                .token_secret
                .as_ref()
                .map_or_else(|| PASSWORD_SECRET.to_owned(), |s| s.0.clone()),
            extra_ca: setup
                .scope
                .config()
                .network
                .extra_ca_file
                .as_deref()
                .map(Into::into),
            options,
            primed: AtomicBool::new(false),
            threads: Mutex::default(),
            parts: Mutex::default(),
            poller: Mutex::default(),
        })
    }

    fn mailbox(&self) -> Result<Mailbox, AdapterError> {
        let address = self
            .options
            .address
            .clone()
            .ok_or_else(|| missing("address"))?;
        Ok(Mailbox {
            user: self
                .options
                .username
                .clone()
                .unwrap_or_else(|| address.clone()),
            address,
            imap_host: self
                .options
                .imap_host
                .clone()
                .ok_or_else(|| missing("imap_host"))?,
            smtp_host: self
                .options
                .smtp_host
                .clone()
                .ok_or_else(|| missing("smtp_host"))?,
        })
    }

    async fn login(&self, mailbox: &Mailbox, password: &str) -> Result<Imap, AdapterError> {
        let login = Login {
            host: &mailbox.imap_host,
            port: self.options.imap_port,
            security: self.options.imap_security(),
            extra_ca: self.extra_ca.as_deref(),
            user: &mailbox.user,
            password,
        };
        Imap::login(&login).await.map_err(|err| match err {
            ImapError::Refused(why) => AdapterError {
                retryable: false,
                code: "unauthorized".to_owned(),
                message: format!("the IMAP server refused the login: {why}"),
            },
            ImapError::Wire(why) => AdapterError {
                retryable: true,
                code: "email_unreachable".to_owned(),
                message: format!("IMAP {}: {why}", mailbox.imap_host),
            },
        })
    }

    async fn connect_with(
        self: &Arc<Self>,
        link: Arc<dyn AdapterLink>,
    ) -> Result<(), AdapterError> {
        let mailbox = self.mailbox()?;
        let password = credential(&self.scope, &self.secret)?;
        let mut session = self.login(&mailbox, &password).await?;
        if !self.primed.load(Ordering::Acquire) {
            // Mail that predates the adapter is not answered.
            let old = session.unseen().await.map_err(|e| lost(&e))?;
            for uid in old {
                session.mark_seen(uid).await.map_err(|e| lost(&e))?;
            }
            self.primed.store(true, Ordering::Release);
        }
        let stop = CancellationToken::new();
        if let Some(old) = self
            .poller
            .lock()
            .expect("poller poisoned")
            .replace(stop.clone())
        {
            old.cancel();
        }
        tokio::spawn(Arc::clone(self).poll(session, link, mailbox.address, stop));
        Ok(())
    }

    async fn poll(
        self: Arc<Self>,
        mut session: Imap,
        link: Arc<dyn AdapterLink>,
        address: String,
        stop: CancellationToken,
    ) {
        let interval = Duration::from_secs(self.options.poll_interval_secs.max(1));
        loop {
            if let Err(err) = self.drain(&mut session, link.as_ref(), &address).await {
                let error = lost(&err);
                tracing::warn!(platform = PLATFORM, "{}", error.message);
                link.report(AdapterState::Retrying, Some(error));
                return;
            }
            tokio::select! {
                () = stop.cancelled() => {
                    session.logout().await;
                    return;
                }
                () = tokio::time::sleep(interval) => {}
            }
        }
    }

    /// Hands every unseen mail to the gateway, then marks it seen.
    async fn drain(
        &self,
        session: &mut Imap,
        link: &dyn AdapterLink,
        address: &str,
    ) -> Result<(), ImapError> {
        for uid in session.unseen().await? {
            if let Some(raw) = session.fetch(uid).await?
                && let Some(mail) = mime::parse(&raw, address)
            {
                let mut inbound = mail.inbound;
                inbound.attachments = self.keep_parts(&inbound.message_id, mail.parts);
                self.threads
                    .lock()
                    .expect("threads poisoned")
                    .remember(&inbound.chat_id, mail.thread);
                link.submit(inbound).await;
            }
            session.mark_seen(uid).await?;
        }
        Ok(())
    }

    fn keep_parts(&self, message_id: &str, parts: Vec<mime::Part>) -> Vec<Attachment> {
        let mut kept = self.parts.lock().expect("parts poisoned");
        parts
            .into_iter()
            .enumerate()
            .map(|(i, part)| {
                let location = format!("email:{message_id}#{i}");
                kept.insert(location.clone(), part.bytes);
                let mut attachment = Attachment::new(part.kind, &location);
                attachment.name = part.name;
                attachment
            })
            .collect()
    }

    async fn deliver(&self, message: &Outgoing) -> Result<Sent, SendError> {
        let mailbox = self
            .mailbox()
            .map_err(|e| SendError::Permanent(e.message))?;
        let password =
            credential(&self.scope, &self.secret).map_err(|e| SendError::Permanent(e.message))?;
        let thread = {
            let threads = self.threads.lock().expect("threads poisoned");
            message
                .reply_to
                .as_ref()
                .and_then(|id| threads.by_message.get(id))
                .or_else(|| threads.latest.get(&message.chat_id))
                .cloned()
        };
        let subject = thread.as_ref().map_or_else(
            || "Message from Prometheus".to_owned(),
            |t| mime::reply_subject(&t.subject),
        );
        let mut files = Vec::new();
        for file in &message.attachments {
            let path = Path::new(&file.path);
            let bytes = std::fs::read(path)
                .map_err(|e| SendError::Permanent(format!("{}: {e}", path.display())))?;
            let name = path
                .file_name()
                .map_or_else(|| "file".to_owned(), |n| n.to_string_lossy().into_owned());
            files.push(File { name, bytes });
        }
        let draft = Draft {
            from: &mailbox.address,
            to: &message.chat_id,
            subject: &subject,
            in_reply_to: thread.as_ref(),
            text: &message.text,
            files,
        };
        let (raw, id) = draft.render();
        let relay = smtp::Relay {
            host: &mailbox.smtp_host,
            port: self.options.smtp_port,
            security: self.options.smtp_security(),
            extra_ca: self.extra_ca.as_deref(),
            user: &mailbox.user,
            password: &password,
        };
        smtp::send(&relay, &mailbox.address, &message.chat_id, &raw).await?;
        Ok(Sent {
            message_id: Some(id),
        })
    }
}

fn lost(err: &ImapError) -> AdapterError {
    AdapterError {
        retryable: true,
        code: "email_unreachable".to_owned(),
        message: format!("IMAP: {err}"),
    }
}

/// A mailbox of one profile.
#[derive(Debug)]
pub struct Email(Arc<Inner>);

impl Email {
    /// The adapter for `setup`.
    ///
    /// # Errors
    ///
    /// `config.invalid` for bad options.
    pub fn new(setup: &AdapterSetup) -> Result<Self, Error> {
        Ok(Self(Arc::new(Inner::new(setup)?)))
    }
}

impl Adapter for Email {
    fn describe(&self) -> Descriptor {
        // Mail has no voice notes: a recorded clip is an audio attachment.
        let files: BTreeSet<MediaKind> = [
            MediaKind::Image,
            MediaKind::Audio,
            MediaKind::Video,
            MediaKind::Document,
        ]
        .into_iter()
        .collect();
        let limit = pmt_config::platforms::first_party(PLATFORM)
            .map_or(50_000, |p| p.max_message_length as usize);
        Descriptor {
            splits_long_messages: true,
            media_in: files.clone(),
            media_out: files,
            stateless_send: true,
            ..Descriptor::new(PLATFORM, limit)
        }
    }

    fn connect(&self, link: Arc<dyn AdapterLink>) -> AdapterFuture<'_, Result<(), AdapterError>> {
        Box::pin(self.0.connect_with(link))
    }

    fn disconnect(&self) -> AdapterFuture<'_, ()> {
        if let Some(stop) = self.0.poller.lock().expect("poller poisoned").take() {
            stop.cancel();
        }
        Box::pin(async {})
    }

    fn send(&self, message: &Outgoing) -> AdapterFuture<'_, Result<Sent, SendError>> {
        let message = message.clone();
        Box::pin(async move { self.0.deliver(&message).await })
    }

    fn chat_info<'a>(&'a self, chat_id: &'a str) -> AdapterFuture<'a, Result<ChatInfo, SendError>> {
        Box::pin(async move {
            Ok(ChatInfo {
                name: chat_id.to_owned(),
                chat_type: ChatType::Dm,
            })
        })
    }

    fn credential_identity(&self) -> Option<String> {
        self.0.options.address.as_ref().map(|a| a.to_lowercase())
    }

    fn open_media<'a>(
        &'a self,
        attachment: &'a Attachment,
    ) -> AdapterFuture<'a, Result<MediaStream, SendError>> {
        Box::pin(async move {
            let bytes = self
                .0
                .parts
                .lock()
                .expect("parts poisoned")
                .remove(&attachment.location)
                .ok_or_else(|| SendError::Permanent("the attachment is gone".to_owned()))?;
            Ok(MediaStream {
                declared_len: Some(bytes.len() as u64),
                chunks: Box::pin(futures_util::stream::iter([Ok::<_, String>(bytes)])),
            })
        })
    }
}
