//! Outbound media: a file is uploaded to the chat's `files` endpoint and
//! sent as a rich-media message naming the `file_info` QQ answers. Up to
//! 10 MB the bytes go inline (base64); a larger file is uploaded in parts
//! (`upload_prepare` grants one pre-signed URL per part, each part is
//! PUT there and confirmed with `upload_part_finish`, then `files` with
//! the upload id completes it). Guild channels and DMs take no files.

use adapter_kit::message::OutFile;
use adapter_kit::{net, sign};
use md5::Digest as _;
use serde_json::{Value, json};

use crate::{QQ, path_of};

const INLINE_MAX: usize = 10 * 1024 * 1024;

fn file_type(file: &OutFile) -> u8 {
    match file.kind.as_str() {
        _ if file.as_document => 4,
        "image" => 1,
        "video" => 2,
        "voice" | "audio" => 3,
        _ => 4,
    }
}

fn md5_hex(data: &[u8]) -> String {
    sign::hex(&md5::Md5::digest(data))
}

fn file_info(answer: &Value) -> Result<Value, String> {
    let info = &answer["file_info"];
    let info = if info.is_null() {
        &answer["data"]["file_info"]
    } else {
        info
    };
    if info.is_null() {
        Err(format!("server: QQ answered no file_info: {answer}"))
    } else {
        Ok(info.clone())
    }
}

/// Uploads `file` for `chat`: its `file_info`.
pub(crate) fn upload(qq: &mut QQ, chat: &str, file: &OutFile) -> Result<Value, String> {
    if !(chat.starts_with("c2c:") || chat.starts_with("group:")) {
        return Err("permanent: QQ guild channels and DMs take no files".to_owned());
    }
    let path = path_of(chat, "files")?;
    if file.data.len() <= INLINE_MAX {
        let body = json!({
            "file_type": file_type(file),
            "file_data": sign::base64(&file.data),
            "srv_send_msg": false,
        });
        return file_info(&qq.rest("POST", &path, &body)?);
    }
    let prepare = qq.rest(
        "POST",
        &path_of(chat, "upload_prepare")?,
        &json!({
            "file_type": file_type(file),
            "file_name": file.name,
            "file_size": file.data.len(),
            "md5": md5_hex(&file.data),
            "sha1": sign::hex(&sign::sha1(&file.data)),
            "md5_10m": md5_hex(&file.data[..file.data.len().min(10_002_432)]),
        }),
    )?;
    let granted = if prepare["upload_id"].is_null() {
        &prepare["data"]
    } else {
        &prepare
    };
    let upload_id = granted["upload_id"]
        .as_str()
        .ok_or("server: QQ answered no upload_id")?
        .to_owned();
    let block = usize::try_from(granted["block_size"].as_u64().unwrap_or(0)).unwrap_or(0);
    let parts = granted["parts"].as_array().cloned().unwrap_or_default();
    if block == 0 || parts.is_empty() {
        return Err(format!("server: QQ granted no upload parts: {prepare}"));
    }
    for (n, part) in parts.iter().enumerate() {
        let start = n * block;
        let piece = &file.data[start.min(file.data.len())..(start + block).min(file.data.len())];
        let url = part["presigned_url"]
            .as_str()
            .or(part["url"].as_str())
            .ok_or("server: a part has no URL")?;
        let put = net::send(
            "PUT",
            url,
            &[("content-type", "application/octet-stream")],
            Some(piece.to_vec()),
        )?;
        net::classify(&put)?;
        qq.rest(
            "POST",
            &path_of(chat, "upload_part_finish")?,
            &json!({"upload_id": upload_id, "part_index": n + 1, "block_size": piece.len(), "md5": md5_hex(piece)}),
        )?;
    }
    file_info(&qq.rest("POST", &path, &json!({"upload_id": upload_id}))?)
}
