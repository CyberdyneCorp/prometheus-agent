//! iLink message items: text and voice transcripts in, CDN media both
//! ways. CDN files are AES-128-ECB: an inbound item names its download
//! parameter and key; an outbound file is encrypted under a fresh key,
//! posted to the upload URL `getuploadurl` grants, and sent as an item
//! naming the parameter the CDN answers.

use adapter_kit::message::{InFile, OutFile};
use adapter_kit::{crypt, net, sign, text::percent_encode};
use md5::Digest as _;
use serde_json::{Value, json};

use crate::Weixin;

const TEXT: i64 = 1;
const IMAGE: i64 = 2;
const VOICE: i64 = 3;
const FILE: i64 = 4;
const VIDEO: i64 = 5;

/// The text of a message: its text items and voice transcripts.
pub(crate) fn text_of(items: &[Value]) -> String {
    items
        .iter()
        .filter_map(|item| match item["type"].as_i64()? {
            TEXT => item["text_item"]["text"].as_str(),
            VOICE => item["voice_item"]["text"].as_str(),
            _ => None,
        })
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A CDN key: 16 raw bytes, or 32 hex characters, base64-encoded.
fn key_of(text: &str) -> Option<Vec<u8>> {
    let decoded = sign::base64_decode(text).ok()?;
    match decoded.len() {
        16 => Some(decoded),
        32 => {
            let hex = std::str::from_utf8(&decoded).ok()?;
            (0..16)
                .map(|i| u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok())
                .collect()
        }
        _ => None,
    }
}

fn download(cdn: &str, media: &Value) -> Option<Vec<u8>> {
    let param = media["encrypt_query_param"].as_str()?;
    let url = format!(
        "{cdn}/download?encrypted_query_param={}",
        percent_encode(param)
    );
    let response = net::send("GET", &url, &[], None).ok()?;
    net::classify(&response).ok()?;
    match media["aes_key"].as_str().and_then(key_of) {
        Some(key) => crypt::aes128_ecb_decrypt(&key, &response.body).ok(),
        None => Some(response.body),
    }
}

/// The files of a message, downloaded and decrypted.
pub(crate) fn files_of(cdn: &str, items: &[Value]) -> Vec<InFile> {
    items
        .iter()
        .filter_map(|item| {
            let (kind, key, name): (&'static str, &str, &str) = match item["type"].as_i64()? {
                IMAGE => ("image", "image_item", "image.jpg"),
                VIDEO => ("video", "video_item", "video.mp4"),
                FILE => ("document", "file_item", "file"),
                VOICE
                    if item["voice_item"]["text"]
                        .as_str()
                        .is_none_or(str::is_empty) =>
                {
                    ("voice", "voice_item", "voice.silk")
                }
                _ => return None,
            };
            let entry = &item[key];
            let bytes = download(cdn, &entry["media"])?;
            let name = entry["file_name"].as_str().unwrap_or(name);
            Some(InFile::bytes(kind, name, bytes))
        })
        .collect()
}

/// A key no one outside can guess: keyed by the account token over the
/// file's digest and a counter (the sandbox has no random source).
fn fresh_key(wx: &mut Weixin, digest: &[u8]) -> Vec<u8> {
    wx.seq += 1;
    let mut seed = digest.to_vec();
    seed.extend(wx.seq.to_be_bytes());
    seed.extend(adapter_kit::clock::now_ms().to_be_bytes());
    sign::hmac_sha256(wx.token.as_bytes(), &seed)[..16].to_vec()
}

/// Uploads `file` for `chat`: the message item that sends it.
pub(crate) fn upload(wx: &mut Weixin, chat: &str, file: &OutFile) -> Result<Value, String> {
    let (media_type, item_type, item_key) = match file.kind.as_str() {
        "image" if !file.as_document => (1, IMAGE, "image_item"),
        "video" if !file.as_document => (2, VIDEO, "video_item"),
        _ => (3, FILE, "file_item"),
    };
    let digest = md5::Md5::digest(&file.data).to_vec();
    let key = fresh_key(wx, &digest);
    let sealed = crypt::aes128_ecb_encrypt(&key, &file.data)?;
    let filekey = sign::hex(&sign::sha256(&[digest.as_slice(), &key].concat())[..16]);
    let grant = wx.post(
        "ilink/bot/getuploadurl",
        &json!({
            "filekey": filekey, "media_type": media_type, "to_user_id": chat,
            "rawsize": file.data.len(), "rawfilemd5": sign::hex(&digest),
            "filesize": sealed.len(), "no_need_thumb": true, "aeskey": sign::hex(&key),
        }),
    )?;
    let param = grant["upload_param"]
        .as_str()
        .ok_or_else(|| format!("server: iLink granted no upload: {grant}"))?;
    let url = format!(
        "{}/upload?encrypted_query_param={}&filekey={filekey}",
        wx.cdn,
        percent_encode(param)
    );
    let answer = net::send(
        "POST",
        &url,
        &[("content-type", "application/octet-stream")],
        Some(sealed.clone()),
    )?;
    net::classify(&answer)?;
    let stored = answer
        .header("x-encrypted-param")
        .ok_or("server: the Weixin CDN answered no x-encrypted-param")?;
    let media = json!({
        "encrypt_query_param": stored,
        "aes_key": sign::base64(sign::hex(&key).as_bytes()),
        "encrypt_type": 1,
    });
    let mut entry = json!({"media": media});
    match item_type {
        IMAGE => entry["mid_size"] = json!(sealed.len()),
        VIDEO => entry["video_size"] = json!(sealed.len()),
        _ => {
            entry["file_name"] = json!(file.name);
            entry["len"] = json!(file.data.len().to_string());
        }
    }
    Ok(json!({"type": item_type, item_key: entry}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_come_raw_or_as_hex() {
        let raw = [7u8; 16];
        assert_eq!(key_of(&sign::base64(&raw)).unwrap(), raw);
        let hex = sign::base64(sign::hex(&raw).as_bytes());
        assert_eq!(key_of(&hex).unwrap(), raw);
        assert!(key_of(&sign::base64(b"short")).is_none());
    }

    #[test]
    fn text_and_voice_transcripts_make_the_text() {
        let items = [
            json!({"type": 1, "text_item": {"text": "hi"}}),
            json!({"type": 3, "voice_item": {"text": "spoken"}}),
        ];
        assert_eq!(text_of(&items), "hi\nspoken");
    }
}
