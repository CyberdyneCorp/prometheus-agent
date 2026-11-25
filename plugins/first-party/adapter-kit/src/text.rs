//! Text helpers: Markdown stripped for plain-text platforms, splitting
//! within a byte limit on line then word boundaries, form encoding.

/// `text` without Markdown emphasis, code fences, headings and link
/// syntax (`[label](url)` becomes `label (url)`).
#[must_use]
pub fn strip_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            continue;
        }
        let line = line.trim_start_matches('#').trim_start_matches(' ');
        out.push_str(&links(line));
        out.push('\n');
    }
    out.pop();
    out.replace("**", "").replace("__", "").replace('`', "")
}

fn links(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(open) = rest.find('[') {
        let Some(close) = rest[open..].find("](").map(|i| open + i) else {
            break;
        };
        let Some(end) = rest[close..].find(')').map(|i| close + i) else {
            break;
        };
        out.push_str(&rest[..open]);
        out.push_str(&rest[open + 1..close]);
        out.push_str(" (");
        out.push_str(&rest[close + 2..end]);
        out.push(')');
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Splits `text` into pieces of at most `max` bytes, breaking at a
/// newline, else a space, else a character boundary; line breaks between
/// pieces are dropped. A bare carriage return is a line break too and NUL
/// is removed, so no piece holds a byte a line protocol (IRC) would read
/// as the end of a line.
#[must_use]
pub fn split_bytes(text: &str, max: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let text = text.replace('\0', "");
    for line in text.split(['\n', '\r']) {
        let mut rest = line;
        while rest.len() > max {
            let mut cut = max;
            while !rest.is_char_boundary(cut) {
                cut -= 1;
            }
            let at = rest[..cut].rfind(' ').filter(|&i| i > 0).unwrap_or(cut);
            pieces.push(rest[..at].to_owned());
            rest = rest[at..].trim_start_matches(' ');
        }
        if !rest.is_empty() {
            pieces.push(rest.to_owned());
        }
    }
    pieces
}

/// Splits `text` into pieces of at most `max` characters, preferring a
/// newline, then a space.
#[must_use]
pub fn split_chars(text: &str, max: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut rest = text;
    while rest.chars().count() > max {
        let cut = rest.char_indices().nth(max).map_or(rest.len(), |(i, _)| i);
        let head = &rest[..cut];
        let at = head
            .rfind('\n')
            .or_else(|| head.rfind(' '))
            .filter(|&i| i > 0)
            .unwrap_or(cut);
        pieces.push(rest[..at].to_owned());
        rest = rest[at..].trim_start_matches([' ', '\n']);
    }
    if !rest.is_empty() {
        pieces.push(rest.to_owned());
    }
    pieces
}

/// Decodes `a=1&b=x+y` (percent and `+` decoding).
#[must_use]
pub fn form_decode(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&text[i + 1..i + 3], 16) {
                Ok(b) => {
                    out.push(b);
                    i += 2;
                }
                Err(_) => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The text of the first `<tag>` element in a flat XML document (the
/// platform callback envelopes): CDATA unwrapped, the five predefined
/// entities decoded. Nothing else of XML is interpreted (no DTDs, no
/// external or custom entities), so a hostile body cannot expand.
#[must_use]
pub fn xml_field(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = start + xml[start..].find(&close)?;
    let inner = xml[start..end].trim();
    if let Some(cdata) = inner
        .strip_prefix("<![CDATA[")
        .and_then(|c| c.strip_suffix("]]>"))
    {
        return Some(cdata.to_owned());
    }
    Some(
        inner
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

/// Encodes form pairs (`application/x-www-form-urlencoded`).
#[must_use]
pub fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-encodes everything but unreserved characters.
#[must_use]
pub fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_splitting_keeps_words_and_the_limit() {
        let text = "word ".repeat(300);
        let pieces = split_bytes(&text, 450);
        assert!(pieces.len() >= 3);
        assert!(pieces.iter().all(|p| p.len() <= 450 && !p.starts_with(' ')));
        assert_eq!(pieces.concat().replace(' ', ""), text.replace(' ', ""));
        let multibyte = "é".repeat(400);
        assert!(split_bytes(&multibyte, 450).iter().all(|p| p.len() <= 450));
    }

    #[test]
    fn byte_pieces_hold_no_carriage_return_or_nul() {
        let pieces = split_bytes("hi\rQUIT :x\r\nnext\0line", 450);
        assert_eq!(pieces, vec!["hi", "QUIT :x", "nextline"]);
    }

    #[test]
    fn char_splitting_prefers_newlines() {
        let pieces = split_chars("aaaa\nbbbb cccc", 9);
        assert_eq!(pieces, vec!["aaaa", "bbbb cccc"]);
    }

    #[test]
    fn markdown_is_stripped() {
        assert_eq!(
            strip_markdown("# Title\n**bold** [site](https://x.y)"),
            "Title\nbold site (https://x.y)"
        );
    }

    #[test]
    fn xml_fields_unwrap_cdata_and_entities() {
        let xml =
            "<xml><ToUserName><![CDATA[corp<1>]]></ToUserName><Content>a &amp; b</Content></xml>";
        assert_eq!(xml_field(xml, "ToUserName").as_deref(), Some("corp<1>"));
        assert_eq!(xml_field(xml, "Content").as_deref(), Some("a & b"));
        assert_eq!(xml_field(xml, "MsgId"), None);
    }

    #[test]
    fn forms_round_trip() {
        let encoded = form_encode(&[("Body", "a b&c"), ("To", "+1555")]);
        assert_eq!(
            form_decode(&encoded),
            vec![
                ("Body".into(), "a b&c".into()),
                ("To".into(), "+1555".into())
            ]
        );
    }
}
