//! Mail as the gateway's events and replies as mail (spec:
//! platform-adapters, Inbound event normalization; Email adapter
//! behaviour). Inbound: the `Message-ID` is the message id, the From
//! address (lower case) both the sender and the chat, the subject is
//! prefixed as `[Subject: …]` unless it is already a reply, HTML-only mail
//! is read as text, and mail from the mailbox itself, from bounce or
//! no-reply senders, or marked automatic or bulk is not a message to
//! answer. Outbound: plain UTF-8 text (attachments as a multipart),
//! threaded with `In-Reply-To`/`References` and one `Re:` prefix.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;
use jiff::Timestamp;
use mail_parser::{HeaderValue, MessageParser, MimeHeaders as _};
use pmt_gateway::messaging::{ChatType, Inbound, MediaKind};

/// What the adapter keeps of an inbound mail to answer it in its thread.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Thread {
    pub(crate) subject: String,
    pub(crate) message_id: String,
    pub(crate) references: Vec<String>,
}

/// One inbound attachment, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Part {
    pub(crate) kind: MediaKind,
    pub(crate) name: Option<String>,
    pub(crate) bytes: Vec<u8>,
}

/// A parsed inbound mail.
#[derive(Debug, Clone)]
pub(crate) struct Mail {
    pub(crate) inbound: Inbound,
    pub(crate) thread: Thread,
    pub(crate) parts: Vec<Part>,
}

fn bracketed(id: &str) -> String {
    format!("<{}>", id.trim_matches(['<', '>']))
}

fn ids(value: &HeaderValue<'_>) -> Vec<String> {
    match value {
        HeaderValue::Text(id) => vec![bracketed(id)],
        HeaderValue::TextList(list) => list.iter().map(|id| bracketed(id)).collect(),
        _ => Vec::new(),
    }
}

/// Senders whose mail is never a message to answer.
fn automatic_sender(address: &str) -> bool {
    let local = address.split('@').next().unwrap_or_default();
    [
        "noreply",
        "no-reply",
        "donotreply",
        "do-not-reply",
        "mailer-daemon",
        "postmaster",
        "bounce",
        "bounces",
    ]
    .iter()
    .any(|name| {
        local == *name
            || local.starts_with(&format!("{name}+"))
            || local.starts_with(&format!("{name}-"))
    })
}

fn kind(ctype: &str) -> MediaKind {
    match ctype {
        "image" => MediaKind::Image,
        "audio" => MediaKind::Audio,
        "video" => MediaKind::Video,
        _ => MediaKind::Document,
    }
}

/// The MIME type an outbound file is labelled with, by extension, so the
/// recipient's client shows images, audio and video as such.
fn content_type(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("mp3") => "audio/mpeg",
        Some("ogg" | "oga" | "opus") => "audio/ogg",
        Some("m4a") => "audio/mp4",
        Some("wav") => "audio/wav",
        Some("flac") => "audio/flac",
        Some("mp4") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("webm") => "video/webm",
        Some("pdf") => "application/pdf",
        Some("txt" | "md") => "text/plain",
        Some("csv") => "text/csv",
        Some("json") => "application/json",
        Some("zip") => "application/zip",
        _ => "application/octet-stream",
    }
}

/// Whether the subject already marks a reply.
fn is_reply(subject: &str) -> bool {
    subject
        .trim_start()
        .get(..3)
        .is_some_and(|p| p.eq_ignore_ascii_case("re:"))
}

/// The inbound event of a raw mail sent to `mailbox`; `None` for mail
/// that is not a message to answer.
pub(crate) fn parse(raw: &[u8], mailbox: &str) -> Option<Mail> {
    let message = MessageParser::default().parse(raw)?;
    let from = message.from()?.first()?.address()?.to_lowercase();
    let automatic = message
        .header("Auto-Submitted")
        .and_then(HeaderValue::as_text)
        .is_some_and(|v| !v.eq_ignore_ascii_case("no"))
        || message
            .header("Precedence")
            .and_then(HeaderValue::as_text)
            .is_some_and(|v| ["bulk", "list", "junk"].contains(&v.to_lowercase().as_str()))
        || message.header("List-Unsubscribe").is_some();
    if from == mailbox.to_lowercase() || automatic_sender(&from) || automatic {
        return None;
    }
    let message_id = bracketed(message.message_id()?);
    let subject = message.subject().unwrap_or_default().to_owned();
    let body = message
        .body_text(0)
        .unwrap_or_default()
        .trim_end()
        .to_owned();
    let text = if subject.is_empty() || is_reply(&subject) {
        body
    } else {
        format!("[Subject: {subject}]\n\n{body}")
    };
    let mut references = ids(message.references());
    let in_reply_to = ids(message.in_reply_to()).into_iter().next();
    if let Some(parent) = &in_reply_to
        && !references.contains(parent)
    {
        references.push(parent.clone());
    }
    let parts = message
        .attachments()
        .map(|part| Part {
            kind: kind(part.content_type().map_or("application", |c| c.ctype())),
            name: part.attachment_name().map(str::to_owned),
            bytes: part.contents().to_vec(),
        })
        .collect();
    let mut inbound = Inbound::text(ChatType::Dm, &from, &from, &text);
    inbound.message_id.clone_from(&message_id);
    inbound.sender_name = message
        .from()
        .and_then(|f| f.first())
        .and_then(|a| a.name())
        .map(str::to_owned);
    inbound.timestamp = message
        .date()
        .and_then(|d| Timestamp::from_second(d.to_timestamp()).ok())
        .unwrap_or_else(Timestamp::now);
    inbound.reply_to = in_reply_to;
    Some(Mail {
        inbound,
        thread: Thread {
            subject,
            message_id,
            references,
        },
        parts,
    })
}

/// `Re: <subject>` with exactly one prefix.
pub(crate) fn reply_subject(subject: &str) -> String {
    let mut rest = subject.trim();
    while is_reply(rest) {
        rest = rest[3..].trim_start();
    }
    if rest.is_empty() {
        "Re: your message".to_owned()
    } else {
        format!("Re: {rest}")
    }
}

/// A header value, RFC 2047-encoded when it is not plain ASCII.
fn header_text(text: &str) -> String {
    if text.chars().all(|c| c.is_ascii() && !c.is_ascii_control()) {
        text.to_owned()
    } else {
        format!(
            "=?utf-8?b?{}?=",
            base64::engine::general_purpose::STANDARD.encode(text)
        )
    }
}

/// Base64 in 76-character lines.
fn base64_lines(bytes: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let mut out = String::new();
    for chunk in encoded.as_bytes().chunks(76) {
        out.push_str(&String::from_utf8_lossy(chunk));
        out.push_str("\r\n");
    }
    out
}

/// One outbound file.
pub(crate) struct File {
    pub(crate) name: String,
    pub(crate) bytes: Vec<u8>,
}

/// An outbound mail.
pub(crate) struct Draft<'a> {
    pub(crate) from: &'a str,
    pub(crate) to: &'a str,
    pub(crate) subject: &'a str,
    pub(crate) in_reply_to: Option<&'a Thread>,
    pub(crate) text: &'a str,
    pub(crate) files: Vec<File>,
}

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A new `Message-ID` in the sender's domain.
pub(crate) fn new_message_id(from: &str) -> String {
    let domain = from.rsplit_once('@').map_or("prometheus.local", |(_, d)| d);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!(
        "<pmt.{}.{}.{seq}@{domain}>",
        Timestamp::now().as_nanosecond(),
        std::process::id()
    )
}

impl Draft<'_> {
    /// The RFC 5322 message and its `Message-ID`.
    pub(crate) fn render(&self) -> (Vec<u8>, String) {
        let id = new_message_id(self.from);
        let mut out = String::new();
        let date = jiff::Zoned::now().strftime("%a, %d %b %Y %H:%M:%S %z");
        let _ = write!(
            out,
            "From: {}\r\nTo: {}\r\nSubject: {}\r\nDate: {date}\r\nMessage-ID: {id}\r\n",
            self.from,
            self.to,
            header_text(self.subject)
        );
        if let Some(thread) = self.in_reply_to {
            let mut references = thread.references.clone();
            references.push(thread.message_id.clone());
            let _ = write!(
                out,
                "In-Reply-To: {}\r\nReferences: {}\r\n",
                thread.message_id,
                references.join(" ")
            );
        }
        // RFC 3834: an automatic reply, so other robots do not answer it.
        out.push_str("Auto-Submitted: auto-replied\r\nMIME-Version: 1.0\r\n");
        let text_part = format!(
            "Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: base64\r\n\r\n{}",
            base64_lines(self.text.as_bytes())
        );
        if self.files.is_empty() {
            out.push_str(&text_part);
            return (out.into_bytes(), id);
        }
        let boundary = format!(
            "pmt-{}",
            id.trim_matches(['<', '>']).replace(['@', '.'], "-")
        );
        let _ = write!(
            out,
            "Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n\r\n--{boundary}\r\n{text_part}"
        );
        for file in &self.files {
            let _ = write!(
                out,
                "--{boundary}\r\nContent-Type: {2}; name=\"{0}\"\r\nContent-Disposition: attachment; filename=\"{0}\"\r\nContent-Transfer-Encoding: base64\r\n\r\n{1}",
                file.name.replace('"', "'"),
                base64_lines(&file.bytes),
                content_type(&file.name)
            );
        }
        let _ = write!(out, "--{boundary}--\r\n");
        (out.into_bytes(), id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAILBOX: &str = "agent@example.org";

    fn mail(headers: &str, body: &str) -> Vec<u8> {
        format!("{headers}\r\n\r\n{body}").into_bytes()
    }

    #[test]
    fn a_mail_is_a_dm_from_its_sender_keyed_by_message_id() {
        let raw = mail(
            "From: Kit <Kit@Example.org>\r\nTo: agent@example.org\r\nSubject: Weekly report\r\nMessage-ID: <a@x>\r\nDate: Thu, 1 Jan 2026 10:00:00 +0000",
            "Please summarize.\r\n",
        );
        let parsed = parse(&raw, MAILBOX).unwrap();
        assert_eq!(parsed.inbound.message_id, "<a@x>");
        assert_eq!(parsed.inbound.sender_id, "kit@example.org");
        assert_eq!(parsed.inbound.chat_id, "kit@example.org");
        assert_eq!(parsed.inbound.chat_type, ChatType::Dm);
        assert_eq!(
            parsed.inbound.text,
            "[Subject: Weekly report]\n\nPlease summarize."
        );
        assert_eq!(parsed.inbound.sender_name.as_deref(), Some("Kit"));
    }

    #[test]
    fn replies_keep_their_thread_without_a_subject_prefix() {
        let raw = mail(
            "From: kit@example.org\r\nSubject: Re: Weekly report\r\nMessage-ID: <b@x>\r\nIn-Reply-To: <a@x>\r\nReferences: <root@x>",
            "Thanks",
        );
        let parsed = parse(&raw, MAILBOX).unwrap();
        assert_eq!(parsed.inbound.text, "Thanks");
        assert_eq!(parsed.inbound.reply_to.as_deref(), Some("<a@x>"));
        assert_eq!(parsed.thread.references, ["<root@x>", "<a@x>"]);
    }

    #[test]
    fn automatic_and_self_sent_mail_is_not_answered() {
        for headers in [
            "From: agent@example.org\r\nMessage-ID: <c@x>",
            "From: MAILER-DAEMON@example.org\r\nMessage-ID: <c@x>",
            "From: noreply@shop.example\r\nMessage-ID: <c@x>",
            "From: kit@example.org\r\nAuto-Submitted: auto-replied\r\nMessage-ID: <c@x>",
            "From: kit@example.org\r\nPrecedence: bulk\r\nMessage-ID: <c@x>",
            "From: kit@example.org\r\nList-Unsubscribe: <mailto:u@x>\r\nMessage-ID: <c@x>",
        ] {
            assert!(parse(&mail(headers, "x"), MAILBOX).is_none(), "{headers}");
        }
    }

    #[test]
    fn a_threaded_reply_has_one_re_prefix_and_the_thread_headers() {
        assert_eq!(reply_subject("Re: RE: re:Weekly"), "Re: Weekly");
        let thread = Thread {
            subject: "Re: Weekly".into(),
            message_id: "<a@x>".into(),
            references: vec!["<root@x>".into()],
        };
        let draft = Draft {
            from: MAILBOX,
            to: "kit@example.org",
            subject: &reply_subject(&thread.subject),
            in_reply_to: Some(&thread),
            text: "Résumé attached",
            files: vec![File {
                name: "r.txt".into(),
                bytes: b"hi".to_vec(),
            }],
        };
        let (raw, id) = draft.render();
        let text = String::from_utf8(raw.clone()).unwrap();
        assert!(text.contains("In-Reply-To: <a@x>\r\n"), "{text}");
        assert!(text.contains("References: <root@x> <a@x>\r\n"), "{text}");
        assert!(text.contains("Subject: Re: Weekly\r\n"), "{text}");
        let parsed = MessageParser::default().parse(&raw).unwrap();
        assert_eq!(parsed.body_text(0).unwrap().trim(), "Résumé attached");
        assert_eq!(parsed.attachments().count(), 1);
        assert_eq!(bracketed(parsed.message_id().unwrap()), id);
    }
}
