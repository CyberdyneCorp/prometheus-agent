//! Outbound media through the platform object store: `genUploadInfo`
//! grants temporary COS credentials and a key, the file is PUT with a
//! COS signature (HMAC-SHA1 over the request, valid for the window the
//! grant names) and sent as an image or file element naming its URL.

use adapter_kit::message::OutFile;
use adapter_kit::text::percent_encode;
use adapter_kit::{net, sign};
use md5::Digest as _;
use serde_json::json;

use crate::Yuanbao;
use crate::proto::Writer;

fn hmac_sha1_hex(key: &str, text: &str) -> String {
    sign::hex(&sign::hmac_sha1(key.as_bytes(), text.as_bytes()))
}

/// The COS `Authorization` of a PUT of `path` with `headers` (lower-case
/// names, sorted), valid from `start` to `end`.
pub(crate) fn cos_authorization(
    secret_id: &str,
    secret_key: &str,
    path: &str,
    headers: &[(&str, &str)],
    (start, end): (u64, u64),
) -> String {
    let window = format!("{start};{end}");
    let sign_key = hmac_sha1_hex(secret_key, &window);
    let mut pairs: Vec<(String, String)> = headers
        .iter()
        .map(|(k, v)| (k.to_lowercase(), percent_encode(v)))
        .collect();
    pairs.sort();
    let joined = pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let http = format!("put\n{path}\n\n{joined}\n");
    let to_sign = format!(
        "sha1\n{window}\n{}\n",
        sign::hex(&sign::sha1(http.as_bytes()))
    );
    let names = pairs
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    format!(
        "q-sign-algorithm=sha1&q-ak={secret_id}&q-sign-time={window}&q-key-time={window}&q-header-list={names}&q-url-param-list=&q-signature={}",
        hmac_sha1_hex(&sign_key, &to_sign)
    )
}

/// Uploads `file`: the message element that shows it.
pub(crate) fn upload(yb: &mut Yuanbao, file: &OutFile) -> Result<Writer, String> {
    let auth = yb.auth.as_ref().ok_or("server: not signed in")?;
    let (token, bot_id) = (auth.token.clone(), auth.bot_id.clone());
    let digest = sign::hex(&md5::Md5::digest(&file.data));
    let grant = net::json(
        "POST",
        &format!("{}/api/resource/genUploadInfo", yb.api),
        &[("x-token", &token), ("x-id", &bot_id), ("x-source", "web")],
        &json!({"fileName": file.name, "fileId": digest, "docFrom": "localDoc", "docOpenId": ""}),
    )?;
    let data = if grant["data"].is_object() {
        &grant["data"]
    } else {
        &grant
    };
    let field = |k: &str| data[k].as_str().unwrap_or_default().to_owned();
    let (bucket, key) = (field("bucketName"), field("location"));
    if bucket.is_empty() || key.is_empty() {
        return Err(format!("server: genUploadInfo granted no bucket: {grant}"));
    }
    let path = format!(
        "/{}",
        key.trim_start_matches('/')
            .split('/')
            .map(percent_encode)
            .collect::<Vec<_>>()
            .join("/")
    );
    let base = yb
        .cos_base
        .clone()
        .unwrap_or_else(|| format!("https://{bucket}.cos.accelerate.myqcloud.com"));
    let host = base.split("://").nth(1).unwrap_or_default().to_owned();
    let mime = if file.mime.is_empty() {
        "application/octet-stream"
    } else {
        &file.mime
    };
    let security = field("encryptToken");
    let start = data["startTime"].as_u64().unwrap_or(0);
    let end = data["expiredTime"].as_u64().unwrap_or(start + 3600);
    let authorization = cos_authorization(
        &field("encryptTmpSecretId"),
        &field("encryptTmpSecretKey"),
        &path,
        &[
            ("host", &host),
            ("content-type", mime),
            ("x-cos-security-token", &security),
        ],
        (start, end),
    );
    let put = net::send(
        "PUT",
        &format!("{base}{path}"),
        &[
            ("authorization", &authorization),
            ("content-type", mime),
            ("x-cos-security-token", &security),
        ],
        Some(file.data.clone()),
    )?;
    net::classify(&put)?;
    let url = Some(field("resourceUrl"))
        .filter(|u| !u.is_empty())
        .unwrap_or(format!("{base}{path}"));
    let size = file.data.len() as u64;
    Ok(if file.kind == "image" && !file.as_document {
        let info = Writer::default().int(1, 1).int(2, size).string(5, &url);
        let content = Writer::default()
            .string(2, &digest)
            .int(3, 255)
            .message(8, &info);
        crate::element("TIMImageElem", &content)
    } else {
        let content = Writer::default()
            .string(2, &digest)
            .string(10, &url)
            .int(11, size)
            .string(12, &file.name);
        crate::element("TIMFileElem", &content)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cos_signature_names_its_headers_in_order() {
        let auth = cos_authorization(
            "AKID",
            "secret",
            "/a/b.png",
            &[("Host", "b.cos.example"), ("content-type", "image/png")],
            (1, 2),
        );
        assert!(auth.starts_with("q-sign-algorithm=sha1&q-ak=AKID&q-sign-time=1;2&q-key-time=1;2&q-header-list=content-type;host&"));
        assert_eq!(auth.rsplit_once("q-signature=").unwrap().1.len(), 40);
    }
}
