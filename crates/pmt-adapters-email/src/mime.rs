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

/// How inbound mail is read.
#[derive(Debug, Clone, Default)]
pub(crate) struct Reading<'a> {
    /// The receiving server whose `Authentication-Results` must
    /// authenticate the From domain; `None` accepts unauthenticated mail.
    pub(crate) authserv_id: Option<&'a str>,
    /// Attachment parts are ignored.
    pub(crate) skip_attachments: bool,
}

/// The `method=result` pairs and `ptype.property=value` facts of one
/// result in an `Authentication-Results` value.
fn result_of(part: &str) -> (&str, &str, Vec<(&str, &str)>) {
    let mut words = part.split_whitespace();
    let (method, result) = words
        .next()
        .and_then(|w| w.split_once('='))
        .unwrap_or_default();
    let facts = words.filter_map(|w| w.split_once('=')).collect();
    (method, result, facts)
}

/// Whether the topmost `Authentication-Results` header comes from
/// `authserv_id` and authenticates `domain`: DMARC when it was evaluated,
/// else an aligned DKIM or SPF pass.
pub(crate) fn authenticated(
    message: &mail_parser::Message<'_>,
    authserv_id: &str,
    domain: &str,
) -> bool {
    let Some(value) = message
        .headers()
        .iter()
        .find(|h| h.name().eq_ignore_ascii_case("Authentication-Results"))
        .and_then(|h| {
            message
                .raw_message()
                .get(h.offset_start() as usize..h.offset_end() as usize)
        })
        .map(|raw| String::from_utf8_lossy(raw).replace(['\r', '\n'], " "))
    else {
        return false;
    };
    let mut parts = value.split(';');
    let server = parts
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .next()
        .unwrap_or_default();
    if !server.eq_ignore_ascii_case(authserv_id) {
        return false;
    }
    let results: Vec<_> = parts.map(result_of).collect();
    let aligned = |facts: &[(&str, &str)], keys: &[&str]| {
        facts.iter().any(|(k, v)| {
            keys.contains(&k.to_ascii_lowercase().as_str())
                && v.trim_matches('"')
                    .rsplit('@')
                    .next()
                    .is_some_and(|d| d.eq_ignore_ascii_case(domain))
        })
    };
    if let Some((_, result, facts)) = results
        .iter()
        .find(|(m, ..)| m.eq_ignore_ascii_case("dmarc"))
    {
        return result.eq_ignore_ascii_case("pass")
            && (facts.is_empty() || aligned(facts, &["header.from"]));
    }
    results.iter().any(|(method, result, facts)| {
        result.eq_ignore_ascii_case("pass")
            && match method.to_ascii_lowercase().as_str() {
                "dkim" => aligned(facts, &["header.d", "header.i"]),
                "spf" => aligned(facts, &["smtp.mailfrom"]),
                _ => false,
            }
    })
}

/// The inbound event of a raw mail sent to `mailbox`; `None` for mail
/// that is not a message to answer (or, when authentication is required,
/// whose sender is not authenticated).
pub(crate) fn parse(raw: &[u8], mailbox: &str, reading: &Reading<'_>) -> Option<Mail> {
    let message = MessageParser::default().parse(raw)?;
    let from = message.from()?.first()?.address()?.to_lowercase();
    if let Some(server) = reading.authserv_id {
        let domain = from.rsplit('@').next().unwrap_or_default();
        if !authenticated(&message, server, domain) {
            tracing::warn!(
                platform = "email",
                "dropped mail claiming to be from {from}: {server} did not authenticate the sender (Authentication-Results)"
            );
            return None;
        }
    }
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
        .filter(|_| !reading.skip_attachments)
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

    fn authenticated_mail(results: &str) -> Vec<u8> {
        mail(
            &format!(
                "{results}From: Boss <boss@example.org>\r\nTo: agent@example.org\r\nSubject: Pay\r\nMessage-ID: <s@x>"
            ),
            "Wire the money.\r\n",
        )
    }

    const GOOGLE: Reading<'static> = Reading {
        authserv_id: Some("mx.google.com"),
        skip_attachments: false,
    };

    #[test]
    fn a_spoofed_from_failing_dmarc_is_dropped() {
        let spoofed = authenticated_mail(
            "Authentication-Results: mx.google.com;\r\n dkim=none; spf=pass smtp.mailfrom=evil.example; dmarc=fail (p=REJECT) header.from=example.org\r\n",
        );
        assert!(parse(&spoofed, MAILBOX, &GOOGLE).is_none());
        let genuine = authenticated_mail(
            "Authentication-Results: mx.google.com; dkim=pass header.i=@example.org; dmarc=pass (p=NONE) header.from=example.org\r\n",
        );
        assert!(parse(&genuine, MAILBOX, &GOOGLE).is_some());
    }

    #[test]
    fn only_the_topmost_header_of_the_trusted_server_counts() {
        // A forged result below the receiving server's own header.
        let forged = authenticated_mail(
            "Authentication-Results: mx.google.com; spf=fail smtp.mailfrom=example.org\r\nAuthentication-Results: mx.google.com; dmarc=pass header.from=example.org\r\n",
        );
        assert!(parse(&forged, MAILBOX, &GOOGLE).is_none());
        let other_server = authenticated_mail(
            "Authentication-Results: mail.evil.example; dmarc=pass header.from=example.org\r\n",
        );
        assert!(parse(&other_server, MAILBOX, &GOOGLE).is_none());
        let missing = authenticated_mail("");
        assert!(parse(&missing, MAILBOX, &GOOGLE).is_none());
        assert!(parse(&missing, MAILBOX, &Reading::default()).is_some());
        let aligned_dkim = authenticated_mail(
            "Authentication-Results: mx.google.com; dkim=pass header.d=example.org\r\n",
        );
        assert!(parse(&aligned_dkim, MAILBOX, &GOOGLE).is_some());
    }

    #[test]
    fn skipped_attachments_are_not_kept() {
        let raw = mail(
            "From: a@example.org\r\nTo: agent@example.org\r\nSubject: f\r\nMessage-ID: <f@x>\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=B",
            "--B\r\nContent-Type: text/plain\r\n\r\nsee file\r\n--B\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=a.pdf\r\n\r\n%PDF\r\n--B--\r\n",
        );
        assert_eq!(
            parse(&raw, MAILBOX, &Reading::default())
                .unwrap()
                .parts
                .len(),
            1
        );
        let skip = Reading {
            skip_attachments: true,
            ..Reading::default()
        };
        let parsed = parse(&raw, MAILBOX, &skip).unwrap();
        assert!(parsed.parts.is_empty());
        assert_eq!(parsed.inbound.text, "[Subject: f]\n\nsee file");
    }

    #[test]
    fn html_only_mail_is_read_as_text() {
        let raw = mail(
            "From: a@example.org\r\nTo: agent@example.org\r\nSubject: h\r\nMessage-ID: <h@x>\r\nContent-Type: text/html",
            "<html><body><p>Hello <b>there</b></p></body></html>\r\n",
        );
        let text = parse(&raw, MAILBOX, &Reading::default())
            .unwrap()
            .inbound
            .text;
        assert!(
            text.contains("Hello there") && !text.contains('<'),
            "{text}"
        );
    }

    #[test]
    fn a_mail_is_a_dm_from_its_sender_keyed_by_message_id() {
        let raw = mail(
            "From: Kit <Kit@Example.org>\r\nTo: agent@example.org\r\nSubject: Weekly report\r\nMessage-ID: <a@x>\r\nDate: Thu, 1 Jan 2026 10:00:00 +0000",
            "Please summarize.\r\n",
        );
        let parsed = parse(&raw, MAILBOX, &Reading::default()).unwrap();
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
        let parsed = parse(&raw, MAILBOX, &Reading::default()).unwrap();
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
            assert!(
                parse(&mail(headers, "x"), MAILBOX, &Reading::default()).is_none(),
                "{headers}"
            );
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
