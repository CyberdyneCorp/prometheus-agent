//! Inbound pushes (`InboundMessagePush`): who wrote where, and the text
//! of the message body: text, stickers (`[sticker: name]`), mentions,
//! files and media as markers or downloads, and forwarded chat-history
//! bundles (`ext_map["wexin_forward_msg_*"]`, a base64 `ForwardMsgData`)
//! decoded into their senders' lines, nested bundles included.

use adapter_kit::message::InFile;
use adapter_kit::{net, sign};
use serde_json::Value;

use crate::proto::Fields;

/// One decoded push.
#[derive(Debug, Default)]
pub(crate) struct Push {
    pub command: String,
    pub from: String,
    pub nickname: String,
    pub group_code: String,
    pub msg_id: String,
    pub body: Vec<Element>,
}

/// One `MsgBodyElement`.
#[derive(Debug, Default)]
pub(crate) struct Element {
    pub kind: String,
    pub content: Fields,
}

impl Push {
    pub(crate) fn decode(data: &[u8]) -> Option<Self> {
        let push = Fields::parse(data)?;
        let body = push
            .messages(13)
            .into_iter()
            .map(|element| Element {
                kind: element.string(1),
                content: element.message(2),
            })
            .collect();
        Some(Self {
            command: push.string(1),
            from: push.string(2),
            nickname: push.string(4),
            group_code: push.string(6),
            msg_id: push.string(12),
            body,
        })
    }
}

/// A sticker's name from its `TIMFaceElem` data.
fn sticker_name(content: &Fields) -> String {
    let data: Value = serde_json::from_str(&content.string(4)).unwrap_or_default();
    data["name"]
        .as_str()
        .map_or_else(|| format!("#{}", content.int(9)), str::to_owned)
}

fn custom_text(content: &Fields) -> String {
    let data: Value = serde_json::from_str(&content.string(4)).unwrap_or_default();
    match data["elem_type"].as_u64() {
        Some(1002) => data["text"].as_str().unwrap_or_default().to_owned(),
        Some(1009) => String::new(),
        _ => data["text"]
            .as_str()
            .or(data["title"].as_str())
            .map_or_else(String::new, |t| format!("[{t}]")),
    }
}

/// The text of a body, element by element.
pub(crate) fn text_of(body: &[Element]) -> String {
    let mut parts = Vec::new();
    for element in body {
        let c = &element.content;
        let part = match element.kind.as_str() {
            "TIMTextElem" => c.string(1),
            "TIMFaceElem" => format!("[sticker: {}]", sticker_name(c)),
            "TIMCustomElem" => custom_text(c),
            // A file with a URL arrives as the file itself; one without
            // (a platform-only reference) is named so the agent knows.
            "TIMFileElem" if c.string(10).is_empty() => format!("[file: {}]", c.string(12)),
            "TIMFileElem" => String::new(),
            "TIMImageElem" | "TIMSoundElem" | "TIMVideoFileElem" => String::new(),
            other => format!("[{other}]"),
        };
        if !part.trim().is_empty() {
            parts.push(part);
        }
    }
    parts.join(" ")
}

/// The forwarded bundles of a body, rendered as `sender: text` lines.
pub(crate) fn forwarded(body: &[Element]) -> Vec<String> {
    let mut lines = Vec::new();
    for element in body {
        for entry in element.content.messages(999) {
            let (key, value) = (entry.string(1), entry.string(2));
            if !key.starts_with("wexin_forward_msg_") {
                continue;
            }
            if let Some(bundle) = sign::base64_decode(&value)
                .ok()
                .and_then(|b| Fields::parse(&b))
            {
                render_bundle(&bundle, 0, &mut lines);
            }
        }
    }
    lines
}

/// `ForwardMsgData {sub_type = 1, ..., nick_name = 4, repeated ForwardMsg
/// msg = 5}`, `ForwardMsg {sender = 1, time = 2, plainText = 3, repeated
/// MsgContent msgContent = 4}`, `MsgContent {type = 1 (1 text, 2 media,
/// 3 nested), text = 2, repeated Multimedia multimedia = 3}`. A nested
/// bundle is any length-delimited field of a type-3 content that parses
/// as a bundle with messages; its lines are indented.
fn render_bundle(bundle: &Fields, depth: usize, lines: &mut Vec<String>) {
    let indent = "  ".repeat(depth);
    for message in bundle.messages(5) {
        let sender = message.string(1);
        let mut parts = Vec::new();
        let mut nested = Vec::new();
        for content in message.messages(4) {
            match content.int(1) {
                1 => parts.push(content.string(2)),
                2 => {
                    for media in content.messages(3) {
                        parts.push(format!("[{}: {}]", media.string(1), media.string(4)));
                    }
                }
                3 => {
                    parts.push("[forwarded chat history]".to_owned());
                    nested.extend(nested_bundles(&content));
                }
                _ => {}
            }
        }
        let text = parts
            .into_iter()
            .filter(|p| !p.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let text = if text.is_empty() {
            message.string(3)
        } else {
            text
        };
        lines.push(format!("{indent}{sender}: {text}"));
        for inner in nested {
            render_bundle(&inner, depth + 1, lines);
        }
    }
}

fn nested_bundles(content: &Fields) -> Vec<Fields> {
    content
        .0
        .iter()
        .filter_map(|(_, value)| match value {
            crate::proto::Val::Bytes(bytes) => {
                let direct = Fields::parse(bytes);
                let encoded = std::str::from_utf8(bytes)
                    .ok()
                    .and_then(|t| sign::base64_decode(t).ok())
                    .and_then(|b| Fields::parse(&b));
                direct.into_iter().chain(encoded).find(|f| {
                    !f.messages(5).is_empty()
                        && f.messages(5).iter().all(|m| !m.string(1).is_empty())
                })
            }
            crate::proto::Val::Int(_) => None,
        })
        .collect()
}

/// Whether `url` may be fetched: HTTPS to a named host, never a literal
/// private, loopback or link-local address (a push must not make the
/// adapter reach into the local network).
pub(crate) fn public_https(url: &str, allow: &[String]) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return allow.iter().any(|a| url.starts_with(a.as_str()));
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    let host = host
        .trim_start_matches('[')
        .split(']')
        .next()
        .unwrap_or_default();
    let host = host
        .rsplit_once(':')
        .filter(|(h, p)| !h.contains(':') && p.parse::<u16>().is_ok())
        .map_or(host, |(h, _)| h);
    if host.eq_ignore_ascii_case("localhost")
        || host.ends_with(".localhost")
        || host.ends_with(".internal")
    {
        return false;
    }
    !is_private_literal(host)
}

fn is_private_literal(host: &str) -> bool {
    if let Ok(v4) = host.parse::<std::net::Ipv4Addr>() {
        return v4.is_private()
            || v4.is_loopback()
            || v4.is_link_local()
            || v4.is_unspecified()
            || v4.octets()[0] == 100 && v4.octets()[1] & 0xc0 == 64;
    }
    if let Ok(v6) = host.parse::<std::net::Ipv6Addr>() {
        let first = v6.segments()[0];
        return v6.is_loopback()
            || v6.is_unspecified()
            || first & 0xfe00 == 0xfc00
            || first & 0xffc0 == 0xfe80;
    }
    false
}

/// The images and files of a body, downloaded when their URL may be
/// fetched (`allow`: extra URL prefixes, the tests' mock).
pub(crate) fn files_of(body: &[Element], allow: &[String]) -> Vec<InFile> {
    let mut files = Vec::new();
    for element in body {
        let c = &element.content;
        let (kind, url, name): (&'static str, String, String) = match element.kind.as_str() {
            "TIMImageElem" => {
                let url = c
                    .messages(8)
                    .first()
                    .map(|i| i.string(5))
                    .unwrap_or_default();
                ("image", url, "image.jpg".to_owned())
            }
            "TIMFileElem" => ("document", c.string(10), c.string(12)),
            "TIMSoundElem" => ("voice", c.string(10), "voice.amr".to_owned()),
            "TIMVideoFileElem" => ("video", c.string(10), "video.mp4".to_owned()),
            _ => continue,
        };
        if url.is_empty() || !public_https(&url, allow) {
            continue;
        }
        let Ok(response) = net::send("GET", &url, &[], None) else {
            continue;
        };
        if net::classify(&response).is_ok() {
            files.push(InFile::bytes(
                kind,
                if name.is_empty() { "file" } else { &name },
                response.body,
            ));
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Writer;

    fn forward(sender: &str, text: &str) -> Writer {
        let content = Writer::default().int(1, 1).string(2, text);
        Writer::default().string(1, sender).message(4, &content)
    }

    #[test]
    fn a_forwarded_bundle_keeps_its_senders_in_order_with_nested_ones() {
        let inner = Writer::default()
            .int(1, 1)
            .message(5, &forward("dan", "deep"));
        let nested = Writer::default().int(1, 3).bytes(8, &inner.0);
        let outer = Writer::default()
            .int(1, 1)
            .message(5, &forward("ann", "one"))
            .message(5, &Writer::default().string(1, "bob").message(4, &nested));
        let ext = Writer::default()
            .string(1, "wexin_forward_msg_1_u")
            .string(2, &sign::base64(&outer.0));
        let element = Element {
            kind: "TIMCustomElem".into(),
            content: Fields::parse(&Writer::default().message(999, &ext).0).unwrap(),
        };
        assert_eq!(
            forwarded(&[element]),
            ["ann: one", "bob: [forwarded chat history]", "  dan: deep"]
        );
    }

    #[test]
    fn private_and_plain_urls_are_not_fetched() {
        for url in [
            "http://cdn.example/a",
            "https://127.0.0.1/a",
            "https://10.0.0.5/x",
            "https://[::1]/x",
            "https://localhost/x",
        ] {
            assert!(!public_https(url, &[]), "{url}");
        }
        assert!(public_https("https://cdn.yuanbao.tencent.com/a.png", &[]));
        assert!(public_https(
            "http://127.0.0.1:9/a",
            &["http://127.0.0.1:9/".to_owned()]
        ));
    }
}
