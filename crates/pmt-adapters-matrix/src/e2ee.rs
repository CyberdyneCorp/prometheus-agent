//! How the adapter uses end-to-end encryption (spec: platform-adapters,
//! Matrix end-to-end encryption): it starts with the connection (failing
//! closed as `matrix_e2ee_unavailable` when `options.e2ee = "required"`),
//! events sent into an encrypted room are encrypted (reactions stay clear,
//! as clients send them), files sent there are encrypted attachments, and
//! nothing goes into an encrypted room in the clear: without encryption
//! such a send is refused.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use pmt_config::platforms::MatrixE2ee;
use pmt_gateway::messaging::{AdapterError, MediaStream, SendError};
use serde_json::Value;

use crate::api::{Api, segment};
use crate::crypto::{Crypto, attachment};
use crate::{Matrix, PLATFORM};

/// The error code of required encryption that cannot start.
pub const E2EE_UNAVAILABLE: &str = "matrix_e2ee_unavailable";

/// Whether each room is encrypted, as learned (shared with the sync loop,
/// which sees `m.room.encryption` events).
pub(crate) type Encrypted = Arc<Mutex<HashMap<String, bool>>>;

/// The joined members of `room`.
async fn members(api: &Api, room: &str) -> Result<Vec<String>, SendError> {
    let joined = api
        .get(&format!("/rooms/{}/joined_members", segment(room)))
        .await?;
    Ok(joined["joined"]
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default())
}

impl Matrix {
    /// Starts encryption for this connection (a reconnect keeps the
    /// running one); `None` when it is off or, `optional`, unavailable.
    pub(crate) async fn start_crypto(
        &self,
        api: &Api,
        user: &str,
        device: Option<&str>,
    ) -> Result<Option<Arc<Crypto>>, AdapterError> {
        if self.e2ee == MatrixE2ee::Off {
            return Ok(None);
        }
        if let Some(running) = self.crypto.lock().expect("crypto poisoned").clone() {
            return Ok(Some(running));
        }
        match Crypto::start(&self.scope, api, user, device).await {
            Ok(crypto) => {
                let crypto = Arc::new(crypto);
                *self.crypto.lock().expect("crypto poisoned") = Some(Arc::clone(&crypto));
                Ok(Some(crypto))
            }
            Err(why) if self.e2ee == MatrixE2ee::Required => Err(AdapterError {
                retryable: false,
                code: E2EE_UNAVAILABLE.to_owned(),
                message: format!(
                    "Matrix end-to-end encryption is required (options.e2ee) but cannot start: {why}"
                ),
            }),
            Err(why) => {
                tracing::warn!(
                    platform = PLATFORM,
                    "end-to-end encryption cannot start, so encrypted rooms are neither read nor \
                     written: {why}"
                );
                Ok(None)
            }
        }
    }

    fn running_crypto(&self) -> Option<Arc<Crypto>> {
        self.crypto.lock().expect("crypto poisoned").clone()
    }

    /// Whether `room` is encrypted: as learned, else its
    /// `m.room.encryption` state (a room whose state cannot be read counts
    /// as encrypted, so nothing leaks).
    pub(crate) async fn room_encrypted(&self, room: &str) -> bool {
        if let Some(known) = self.encrypted.lock().expect("encrypted poisoned").get(room) {
            return *known;
        }
        let Ok(api) = self.api() else { return true };
        let path = format!("/rooms/{}/state/m.room.encryption", segment(room));
        let encrypted = match api.get(&path).await {
            Ok(_) => true,
            Err(SendError::Permanent(why)) if why.starts_with("404") => false,
            Err(_) => return true,
        };
        self.encrypted
            .lock()
            .expect("encrypted poisoned")
            .insert(room.to_owned(), encrypted);
        encrypted
    }

    /// The event to send for `kind`/`content` in `room`: encrypted into an
    /// encrypted room.
    pub(crate) async fn sealed(
        &self,
        room: &str,
        kind: &str,
        content: &Value,
    ) -> Result<(String, Value), SendError> {
        if kind == "m.reaction" || !self.room_encrypted(room).await {
            return Ok((kind.to_owned(), content.clone()));
        }
        let crypto = self.running_crypto().ok_or_else(|| {
            SendError::Permanent(format!(
                "room {room} is end-to-end encrypted and encryption is not running (options.e2ee)"
            ))
        })?;
        let api = self.api()?;
        let members = members(&api, room).await?;
        let sealed = crypto.encrypt(room, kind, content, &members).await?;
        Ok(("m.room.encrypted".to_owned(), sealed))
    }

    /// Uploads the file at `path` for `room`: as an encrypted attachment
    /// into an encrypted room (its `file` object), else in the clear (its
    /// `url`). The content member to set and its value.
    pub(crate) async fn upload_for(
        &self,
        room: &str,
        path: &Path,
        mime: &str,
    ) -> Result<(&'static str, Value), SendError> {
        let api = self.api()?;
        if !self.room_encrypted(room).await {
            return Ok(("url", Value::String(api.upload(path, mime).await?)));
        }
        if self.running_crypto().is_none() {
            return Err(SendError::Permanent(format!(
                "room {room} is end-to-end encrypted and encryption is not running (options.e2ee)"
            )));
        }
        let plain = std::fs::read(path)
            .map_err(|e| SendError::Permanent(format!("{}: {e}", path.display())))?;
        let (ciphertext, mut file) = attachment::seal(&plain).map_err(SendError::Permanent)?;
        let name = path
            .file_name()
            .map_or_else(|| "file".to_owned(), |n| n.to_string_lossy().into_owned());
        let uri = api
            .upload_bytes(ciphertext, &name, "application/octet-stream")
            .await?;
        file["url"] = Value::String(uri);
        Ok(("file", file))
    }

    /// Opens an inbound attachment, decrypting an encrypted one.
    pub(crate) async fn open_attachment(&self, location: &str) -> Result<MediaStream, SendError> {
        let (url, file) = attachment::split(location);
        let stream = self.api()?.download(url).await?;
        let Some(file) = file else {
            return Ok(stream);
        };
        let mut chunks = stream.chunks;
        let mut ciphertext = Vec::new();
        while let Some(chunk) = futures_util::StreamExt::next(&mut chunks).await {
            ciphertext.extend(chunk.map_err(SendError::Network)?);
        }
        let plain = attachment::open(&ciphertext, &file).map_err(SendError::Permanent)?;
        let len = u64::try_from(plain.len()).ok();
        Ok(MediaStream {
            declared_len: len,
            chunks: Box::pin(futures_util::stream::once(async move { Ok(plain) })),
        })
    }
}
