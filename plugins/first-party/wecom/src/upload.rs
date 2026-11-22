//! Outbound media: the per-kind limits (a kind over its limit goes as a
//! file with a notice, anything over 20 MB is refused with one) and the
//! chunked upload (`aibot_upload_media_init`, 512 KB base64 chunks,
//! `aibot_upload_media_finish`) that yields a `media_id`.

use adapter_kit::message::OutFile;
use adapter_kit::sign;
use md5::Digest as _;
use serde_json::json;

use crate::WeCom;

const MB: usize = 1024 * 1024;
const CHUNK: usize = 512 * 1024;
const FILE_MAX: usize = 20 * MB;

/// What to send for one file.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    /// `image`, `voice`, `video` or `file`; `None` when refused.
    pub kind: Option<&'static str>,
    /// Told to the chat first (a downgrade or a refusal).
    pub notice: Option<String>,
}

/// The kind `file` goes out as, within the limits.
pub(crate) fn plan(file: &OutFile) -> Plan {
    let size = file.data.len();
    if size > FILE_MAX {
        return Plan {
            kind: None,
            notice: Some(format!(
                "{} was not sent: it is {} MB, over WeCom's 20 MB limit.",
                file.name,
                size.div_ceil(MB)
            )),
        };
    }
    let (kind, limit) = match file.kind.as_str() {
        _ if file.as_document => ("file", FILE_MAX),
        "image" => ("image", 10 * MB),
        // WeCom plays AMR voice only; other audio goes as a file.
        "voice" | "audio" if file.mime == "audio/amr" => ("voice", 2 * MB),
        "video" => ("video", 10 * MB),
        _ => ("file", FILE_MAX),
    };
    if size > limit {
        return Plan {
            kind: Some("file"),
            notice: Some(format!(
                "{} is over WeCom's {} MB {kind} limit, so it is sent as a file.",
                file.name,
                limit / MB
            )),
        };
    }
    Plan {
        kind: Some(kind),
        notice: None,
    }
}

/// Uploads `file` as `kind`: its `media_id`.
pub(crate) fn upload(wc: &mut WeCom, kind: &str, file: &OutFile) -> Result<String, String> {
    let chunks: Vec<&[u8]> = file.data.chunks(CHUNK).collect();
    let md5 = sign::hex(&md5::Md5::digest(&file.data));
    let init = wc.request(
        "aibot_upload_media_init",
        &json!({
            "type": kind,
            "filename": file.name,
            "total_size": file.data.len(),
            "total_chunks": chunks.len(),
            "md5": md5,
        }),
    )?;
    let upload_id = init["upload_id"]
        .as_str()
        .ok_or("server: WeCom answered no upload_id")?
        .to_owned();
    for (index, chunk) in chunks.iter().enumerate() {
        wc.request(
            "aibot_upload_media_chunk",
            &json!({"upload_id": upload_id, "chunk_index": index, "base64_data": sign::base64(chunk)}),
        )?;
    }
    let done = wc.request(
        "aibot_upload_media_finish",
        &json!({"upload_id": upload_id}),
    )?;
    done["media_id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "server: WeCom answered no media_id".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(kind: &str, mime: &str, size: usize) -> OutFile {
        OutFile {
            name: "x".into(),
            kind: kind.into(),
            mime: mime.into(),
            data: vec![0; size],
            ..OutFile::default()
        }
    }

    #[test]
    fn oversized_media_is_downgraded_or_refused() {
        assert_eq!(plan(&file("image", "image/png", MB)).kind, Some("image"));
        let big = plan(&file("image", "image/png", 11 * MB));
        assert_eq!(big.kind, Some("file"));
        assert!(big.notice.unwrap().contains("10 MB"));
        assert_eq!(plan(&file("voice", "audio/ogg", 10)).kind, Some("file"));
        assert_eq!(plan(&file("voice", "audio/amr", 3 * MB)).kind, Some("file"));
        let huge = plan(&file("document", "application/pdf", 21 * MB));
        assert_eq!(huge.kind, None);
        assert!(huge.notice.unwrap().contains("20 MB"));
    }
}
