//! Inbound media of a Buzz event: its NIP-92 `imeta` tags (`url`, `x` =
//! SHA-256 hex, `size`, `m` = MIME type, `filename`). A file is fetched
//! only from the relay's own origin or an `attachment_hosts` host over
//! HTTPS, at most 4 per event and 20 MB in all, within its declared size,
//! and kept only when its digest matches; every refusal becomes a note
//! for the agent. Also the HTTP `Date` parser the relay clock uses.

use adapter_kit::message::{InFile, kind_of_name};
use adapter_kit::{net, sign};
use serde_json::Value;

const MAX_FILES: usize = 4;
const MAX_BYTES: u64 = 20 * 1024 * 1024;

/// One declared attachment.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Declared {
    pub url: String,
    pub sha256: String,
    pub size: u64,
    pub name: String,
}

/// The `imeta` attachments of `event`, and how many were malformed.
pub(crate) fn declared(event: &Value) -> (Vec<Declared>, usize) {
    let (mut found, mut rejected, mut total) = (Vec::new(), 0, 0);
    for tag in event["tags"].as_array().into_iter().flatten() {
        if tag[0] != "imeta" {
            continue;
        }
        let field = |key: &str| {
            tag.as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .find_map(|f| f.strip_prefix(key).and_then(|r| r.strip_prefix(' ')))
                .map(|v| v.trim().to_owned())
                .unwrap_or_default()
        };
        let (url, sha256) = (field("url"), field("x").to_lowercase());
        let size: u64 = field("size").parse().unwrap_or(0);
        let digest_ok = sha256.len() == 64 && sha256.bytes().all(|b| b.is_ascii_hexdigit());
        if found.len() >= MAX_FILES || !digest_ok || size == 0 || total + size > MAX_BYTES {
            rejected += 1;
            continue;
        }
        total += size;
        let name = field("filename");
        let name = name
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or_default()
            .to_owned();
        let name = if name.is_empty() || name == ".." {
            url.rsplit('/').next().unwrap_or("attachment").to_owned()
        } else {
            name
        };
        found.push(Declared {
            url,
            sha256,
            size,
            name,
        });
    }
    (found, rejected)
}

/// The `scheme://host[:port]` of `url`.
fn origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    (!host.is_empty() && !host.contains('@'))
        .then(|| format!("{}://{}", scheme.to_lowercase(), host.to_lowercase()))
}

/// Whether `url` may be fetched: the relay's origin, or HTTPS to a host of
/// `hosts`.
pub(crate) fn allowed(url: &str, relay_origin: &str, hosts: &[String]) -> bool {
    let Some(origin) = origin(url) else {
        return false;
    };
    if origin == relay_origin.to_lowercase() {
        return true;
    }
    let Some(host) = origin.strip_prefix("https://") else {
        return false;
    };
    let name = host.rsplit_once(':').map_or(host, |(h, _)| h);
    hosts
        .iter()
        .any(|h| h.eq_ignore_ascii_case(host) || h.eq_ignore_ascii_case(name))
}

fn fetch(file: &Declared, relay_origin: &str, hosts: &[String]) -> Result<InFile, String> {
    if !allowed(&file.url, relay_origin, hosts) {
        return Err(format!(
            "[attachment {} was not fetched: its host is not allowed]",
            file.name
        ));
    }
    let response = net::send("GET", &file.url, &[], None)
        .map_err(|_| format!("[attachment {} could not be downloaded]", file.name))?;
    net::classify(&response)
        .map_err(|_| format!("[attachment {} could not be downloaded]", file.name))?;
    if response.body.len() as u64 > file.size {
        return Err(format!(
            "[attachment {} is larger than declared and was discarded]",
            file.name
        ));
    }
    if sign::hex(&sign::sha256(&response.body)) != file.sha256 {
        return Err(format!(
            "[attachment {} failed verification (SHA-256 mismatch) and was discarded]",
            file.name
        ));
    }
    Ok(InFile::bytes(
        kind_of_name(&file.name),
        &file.name,
        response.body,
    ))
}

/// The verified files of `event` and notes on the ones refused.
pub(crate) fn fetch_all(
    event: &Value,
    relay_origin: &str,
    hosts: &[String],
) -> (Vec<InFile>, Vec<String>) {
    let (files, rejected) = declared(event);
    let mut notes = Vec::new();
    if rejected > 0 {
        notes.push(format!(
            "[{rejected} Buzz attachment(s) rejected as malformed or over limits]"
        ));
    }
    let mut got = Vec::new();
    for file in &files {
        match fetch(file, relay_origin, hosts) {
            Ok(file) => got.push(file),
            Err(note) => notes.push(note),
        }
    }
    (got, notes)
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Seconds since the epoch of an IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`).
pub(crate) fn parse_http_date(text: &str) -> Option<u64> {
    let mut parts = text.split_whitespace().skip(1);
    let day: i64 = parts.next()?.parse().ok()?;
    let name = parts.next()?;
    let month = i64::try_from(MONTHS.iter().position(|m| *m == name)?).ok()? + 1;
    let year: i64 = parts.next()?.parse().ok()?;
    let mut hms = parts.next()?.split(':').map(|p| p.parse::<i64>().ok());
    let (h, m, s) = (hms.next()??, hms.next()??, hms.next()??);
    // Days since 1970-01-01 (H. Hinnant's days_from_civil).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + m * 60 + s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn imeta_attachments_are_bounded_and_checked() {
        let x = "a".repeat(64);
        let event = json!({"tags": [
            ["imeta", "url https://relay.example/m/1.png", format!("x {x}"), "size 10", "m image/png"],
            ["imeta", "url https://relay.example/m/2.png", "x nothex", "size 10"],
        ]});
        let (files, rejected) = declared(&event);
        assert_eq!(rejected, 1);
        assert_eq!(files[0].name, "1.png");
        assert!(allowed(
            "https://relay.example/m/1.png",
            "https://relay.example",
            &[]
        ));
        assert!(!allowed(
            "http://relay.example/m/1.png",
            "https://relay.example",
            &[]
        ));
        assert!(allowed(
            "https://cdn.example/a",
            "https://relay.example",
            &["cdn.example".into()]
        ));
        assert!(!allowed(
            "http://cdn.example/a",
            "https://relay.example",
            &["cdn.example".into()]
        ));
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
    }
}
