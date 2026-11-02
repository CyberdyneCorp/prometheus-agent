//! A mail server mock: IMAP (LOGIN, SELECT, UID SEARCH UNSEEN, UID FETCH
//! BODY.PEEK[], UID STORE +FLAGS, LOGOUT) over a mailbox whose messages
//! stay unseen until a client marks them, and SMTP (EHLO, AUTH PLAIN,
//! MAIL, RCPT, DATA) recording what was sent, with scripted refusals. Both
//! speak plain text on loopback ports.

// A fixture: a mock that cannot serve should fail loudly.
#![allow(clippy::missing_panics_doc)]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use mail_parser::MessageParser;
use pmt_gateway::messaging::conformance::{Failure, MockPlatform, MockSent};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpStream;
use tokio::net::tcp::OwnedReadHalf;
use tokio::sync::watch;

/// The mailbox the adapter answers as.
pub const MAILBOX: &str = "agent@kit.example";
/// Its password.
pub const PASSWORD: &str = "kit-password";
/// The person the kit writes as.
pub const USER: &str = "kit@kit.example";

#[derive(Default)]
struct State {
    /// (uid, raw message, seen)
    inbox: Vec<(u32, Vec<u8>, bool)>,
    last: Option<Vec<u8>>,
    next: u32,
    sent: Vec<MockSent>,
    raw_sent: Vec<Vec<u8>>,
    failures: VecDeque<Failure>,
}

/// The mock.
pub struct MailMock {
    state: Mutex<State>,
    /// Bumped to drop every IMAP connection.
    generation: watch::Sender<u64>,
    imap: SocketAddr,
    smtp: SocketAddr,
}

impl MailMock {
    /// Starts IMAP and SMTP on free loopback ports.
    pub async fn start() -> Arc<Self> {
        let imap = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let smtp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mock = Arc::new(Self {
            state: Mutex::new(State {
                next: 1,
                ..State::default()
            }),
            generation: watch::channel(0).0,
            imap: imap.local_addr().unwrap(),
            smtp: smtp.local_addr().unwrap(),
        });
        let server = Arc::clone(&mock);
        tokio::spawn(async move {
            while let Ok((stream, _)) = imap.accept().await {
                tokio::spawn(Arc::clone(&server).imap_session(stream));
            }
        });
        let server = Arc::clone(&mock);
        tokio::spawn(async move {
            while let Ok((stream, _)) = smtp.accept().await {
                tokio::spawn(Arc::clone(&server).smtp_session(stream));
            }
        });
        mock
    }

    /// The table lines that point the adapter at this mock.
    pub fn table(&self) -> String {
        format!(
            "options = {{ address = \"{MAILBOX}\", imap_host = \"127.0.0.1\", imap_port = {}, imap_security = \"plain\", smtp_host = \"127.0.0.1\", smtp_port = {}, smtp_security = \"plain\", poll_interval_secs = 1 }}",
            self.imap.port(),
            self.smtp.port()
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Delivers a raw message into the inbox.
    pub fn deliver(&self, raw: &[u8]) {
        let mut state = self.lock();
        let uid = state.next;
        state.next += 1;
        state.inbox.push((uid, raw.to_vec(), false));
        state.last = Some(raw.to_vec());
    }

    /// Delivers a mail from `USER` with `headers` added; its Message-ID.
    pub fn receive(&self, subject: &str, extra_headers: &str, body: &str) -> String {
        let n = self.lock().next;
        let id = format!("<kit-{n}@kit.example>");
        let raw = format!(
            "From: Kit <{USER}>\r\nTo: {MAILBOX}\r\nSubject: {subject}\r\nMessage-ID: {id}\r\n{extra_headers}Content-Type: text/plain; charset=utf-8\r\n\r\n{body}\r\n"
        );
        self.deliver(raw.as_bytes());
        id
    }

    /// Every raw message sent over SMTP.
    pub fn raw_sent(&self) -> Vec<Vec<u8>> {
        self.lock().raw_sent.clone()
    }

    /// Unseen messages in the inbox.
    pub fn unseen(&self) -> usize {
        self.lock()
            .inbox
            .iter()
            .filter(|(_, _, seen)| !seen)
            .count()
    }

    async fn imap_session(self: Arc<Self>, stream: TcpStream) {
        let mut dropped = self.generation.subscribe();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        if writer
            .write_all(b"* OK [CAPABILITY IMAP4rev1] kit ready\r\n")
            .await
            .is_err()
        {
            return;
        }
        loop {
            let mut line = String::new();
            let read = tokio::select! {
                _ = dropped.changed() => return,
                read = reader.read_line(&mut line) => read,
            };
            if read.unwrap_or(0) == 0 {
                return;
            }
            let answer = self.imap_command(line.trim_end());
            if writer.write_all(&answer).await.is_err() || line.contains("LOGOUT") {
                return;
            }
        }
    }

    fn imap_command(&self, line: &str) -> Vec<u8> {
        let mut words = line.splitn(3, ' ');
        let (tag, command, rest) = (
            words.next().unwrap_or("*"),
            words.next().unwrap_or_default().to_uppercase(),
            words.next().unwrap_or_default(),
        );
        let ok = format!("{tag} OK done\r\n");
        let mut state = self.lock();
        let untagged = match (command.as_str(), rest) {
            ("LOGIN", args) => {
                let expected = format!("\"{MAILBOX}\" \"{PASSWORD}\"");
                if args != expected {
                    return format!("{tag} NO [AUTHENTICATIONFAILED] Invalid credentials\r\n")
                        .into_bytes();
                }
                Vec::new()
            }
            ("SELECT", _) => format!("* {} EXISTS\r\n", state.inbox.len()).into_bytes(),
            ("UID", args) if args.starts_with("SEARCH") => {
                let ids: Vec<String> = state
                    .inbox
                    .iter()
                    .filter(|(_, _, seen)| !seen)
                    .map(|(uid, _, _)| uid.to_string())
                    .collect();
                let mut search = String::from("* SEARCH");
                for id in ids {
                    search.push(' ');
                    search.push_str(&id);
                }
                search.push_str("\r\n");
                search.into_bytes()
            }
            ("UID", args) if args.starts_with("FETCH") => {
                let uid: u32 = args
                    .split_whitespace()
                    .nth(1)
                    .and_then(|u| u.parse().ok())
                    .unwrap_or(0);
                match state.inbox.iter().position(|(u, _, _)| *u == uid) {
                    Some(seq) => {
                        let raw = &state.inbox[seq].1;
                        let mut out = format!(
                            "* {} FETCH (UID {uid} BODY[] {{{}}}\r\n",
                            seq + 1,
                            raw.len()
                        )
                        .into_bytes();
                        out.extend_from_slice(raw);
                        out.extend_from_slice(b")\r\n");
                        out
                    }
                    None => Vec::new(),
                }
            }
            ("UID", args) if args.starts_with("STORE") => {
                let uid: u32 = args
                    .split_whitespace()
                    .nth(1)
                    .and_then(|u| u.parse().ok())
                    .unwrap_or(0);
                for entry in state.inbox.iter_mut().filter(|(u, _, _)| *u == uid) {
                    entry.2 = true;
                }
                Vec::new()
            }
            ("LOGOUT", _) => b"* BYE kit\r\n".to_vec(),
            ("NOOP", _) => Vec::new(),
            _ => return format!("{tag} BAD unknown command\r\n").into_bytes(),
        };
        let mut out = untagged;
        out.extend_from_slice(ok.as_bytes());
        out
    }

    async fn smtp_session(self: Arc<Self>, stream: TcpStream) {
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let _ = writer.write_all(b"220 kit ESMTP\r\n").await;
        while let Some(line) = read_line(&mut reader).await {
            let upper = line.to_uppercase();
            let reply = if upper == "DATA" {
                let _ = writer.write_all(b"354 Go ahead\r\n").await;
                let Some(message) = read_data(&mut reader).await else {
                    return;
                };
                self.record(&message);
                "250 2.0.0 Queued\r\n".to_owned()
            } else {
                self.smtp_reply(&line, &upper)
            };
            if writer.write_all(reply.as_bytes()).await.is_err() || upper == "QUIT" {
                return;
            }
        }
    }

    /// The reply to one SMTP command other than DATA.
    fn smtp_reply(&self, line: &str, upper: &str) -> String {
        if upper.starts_with("EHLO") {
            return "250-kit\r\n250 AUTH PLAIN\r\n".to_owned();
        }
        if let Some(auth) = line.strip_prefix("AUTH PLAIN ") {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(auth)
                .unwrap_or_default();
            return if decoded == format!("\0{MAILBOX}\0{PASSWORD}").into_bytes() {
                "235 2.7.0 Authentication successful\r\n".to_owned()
            } else {
                "535 5.7.8 Authentication failed\r\n".to_owned()
            };
        }
        if upper.starts_with("MAIL FROM") {
            return match self.lock().failures.pop_front() {
                Some(Failure::RateLimited(_)) => "451 4.7.1 Rate limited, try again later\r\n",
                Some(Failure::ServerError) => "451 4.3.0 Temporary server error\r\n",
                Some(Failure::Markup | Failure::Forbidden) => "550 5.7.1 Sender rejected\r\n",
                None => "250 2.1.0 OK\r\n",
            }
            .to_owned();
        }
        match upper {
            u if u.starts_with("RCPT TO") => "250 2.1.5 OK\r\n",
            "QUIT" => "221 2.0.0 Bye\r\n",
            _ => "502 5.5.2 Unknown command\r\n",
        }
        .to_owned()
    }

    fn record(&self, raw: &[u8]) {
        let parsed = MessageParser::default().parse(raw);
        let field = |f: &dyn Fn(&mail_parser::Message<'_>) -> Option<String>| {
            parsed.as_ref().and_then(f).unwrap_or_default()
        };
        let sent = MockSent {
            chat_id: field(&|m| Some(m.to()?.first()?.address()?.to_owned())),
            message_id: field(&|m| Some(format!("<{}>", m.message_id()?))),
            text: field(&|m| Some(m.body_text(0)?.trim_end().to_owned())),
            buttons: Vec::new(),
        };
        let mut state = self.lock();
        state.sent.push(sent);
        state.raw_sent.push(raw.to_vec());
    }
}

async fn read_line(reader: &mut BufReader<OwnedReadHalf>) -> Option<String> {
    let mut line = String::new();
    match reader.read_line(&mut line).await {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(line.trim_end().to_owned()),
    }
}

/// The DATA section up to the lone dot, un-dot-stuffed.
async fn read_data(reader: &mut BufReader<OwnedReadHalf>) -> Option<Vec<u8>> {
    let mut message = Vec::new();
    loop {
        let line = read_line(reader).await?;
        if line == "." {
            return Some(message);
        }
        let line = line.strip_prefix('.').unwrap_or(&line);
        message.extend_from_slice(line.as_bytes());
        message.extend_from_slice(b"\r\n");
    }
}

impl MockPlatform for MailMock {
    fn dm(&self) -> (String, String) {
        (USER.to_owned(), USER.to_owned())
    }

    fn inject(&self, text: &str) -> String {
        // A reply's subject: the body is the whole input.
        self.receive("Re: kit", "", text)
    }

    fn redeliver(&self) {
        let last = self.lock().last.clone();
        if let Some(raw) = last {
            self.deliver(&raw);
        }
    }

    fn drop_connection(&self) {
        self.generation.send_modify(|g| *g += 1);
    }

    fn press(&self, _message_id: &str, _data: &str) {}

    fn fail_next(&self, failure: Failure) {
        self.lock().failures.push_back(failure);
    }

    fn reports_markup_errors(&self) -> bool {
        false
    }

    fn sent(&self) -> Vec<MockSent> {
        self.lock().sent.clone()
    }

    fn typing(&self) -> usize {
        0
    }

    fn reactions(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    fn hints_retry_after(&self) -> bool {
        false
    }
}

/// The adapter, built for a fresh profile of the home at `home` holding
/// the mailbox password, and the mock: what
/// `prometheus plugins test-adapter email` and the crate's tests run the
/// conformance kit on.
///
/// # Errors
///
/// The profile cannot be set up.
pub async fn kit(
    home: &std::path::Path,
) -> Result<
    (
        Arc<dyn pmt_gateway::messaging::Adapter>,
        Arc<dyn MockPlatform>,
    ),
    pmt_core::error::Error,
> {
    let mock = MailMock::start().await;
    let setup = pmt_gateway::messaging::conformance::kit_setup(
        home,
        crate::PLATFORM,
        &mock.table(),
        &[(crate::PASSWORD_SECRET, PASSWORD)],
    )?;
    let adapter = crate::Email::new(&setup)?;
    Ok((Arc::new(adapter), mock))
}
