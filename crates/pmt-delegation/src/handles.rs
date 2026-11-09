//! Handles of the plugin `subagents` interface (spec: subagent-delegation,
//! Subagent lifecycle interface for plugins).
//!
//! A handle names a child and the root conversation it belongs to, signed
//! with HMAC-SHA256 under a per-process key: a forged, altered or foreign
//! handle verifies to nothing and the caller sees `unknown_handle`, so a
//! plugin can only reach children it launched for the conversation it
//! acts in. The states a handle reports are [`HandleState`].

use std::sync::LazyLock;

use sha2::{Digest as _, Sha256};

/// The per-process signing key.
static KEY: LazyLock<[u8; 32]> = LazyLock::new(|| {
    let mut key = [0u8; 32];
    if getrandom::fill(&mut key).is_err() {
        // No OS randomness: derive from the clock and address space; still
        // unguessable to a sandboxed plugin that cannot read either.
        let seed = format!("{:?}{:p}", std::time::SystemTime::now(), &key);
        key.copy_from_slice(&Sha256::digest(seed.as_bytes()));
    }
    key
});

/// The lifecycle states a plugin sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleState {
    /// Waiting for a slot.
    Queued,
    /// Running.
    Running,
    /// Ended with an answer.
    Completed,
    /// Failed.
    Failed,
    /// Stopped by its conversation (not by the plugin).
    Interrupted,
    /// Stalled.
    Stalled,
    /// The plugin asked it to stop; it has not ended yet.
    CancelRequested,
    /// Ended after the plugin's cancel.
    Cancelled,
}

impl HandleState {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::Stalled => "stalled",
            Self::CancelRequested => "cancel_requested",
            Self::Cancelled => "cancelled",
        }
    }
}

/// What a valid handle names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claims {
    /// The child.
    pub subagent_id: String,
    /// Its root conversation.
    pub root_session_id: String,
    /// Creation, Unix milliseconds.
    pub created_ms: i64,
}

fn hmac(message: &[u8]) -> [u8; 32] {
    let key: &[u8; 32] = &KEY;
    let mut inner_pad = [0x36u8; 64];
    let mut outer_pad = [0x5cu8; 64];
    for (i, k) in key.iter().enumerate() {
        inner_pad[i] ^= k;
        outer_pad[i] ^= k;
    }
    let inner = Sha256::new()
        .chain_update(inner_pad)
        .chain_update(message)
        .finalize();
    Sha256::new()
        .chain_update(outer_pad)
        .chain_update(inner)
        .finalize()
        .into()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

fn message(claims: &Claims) -> String {
    format!(
        "{}\n{}\n{}",
        claims.subagent_id, claims.root_session_id, claims.created_ms
    )
}

/// Signs `claims` into a handle.
#[must_use]
pub fn issue(claims: &Claims) -> String {
    let mac = hex(&hmac(message(claims).as_bytes()));
    format!(
        "{}.{}.{}.{mac}",
        claims.subagent_id, claims.root_session_id, claims.created_ms
    )
}

/// The claims of `handle`, when its signature holds.
#[must_use]
pub fn verify(handle: &str) -> Option<Claims> {
    let (rest, mac) = handle.rsplit_once('.')?;
    let mut fields = rest.splitn(3, '.');
    let claims = Claims {
        subagent_id: fields.next()?.to_owned(),
        root_session_id: fields.next()?.to_owned(),
        created_ms: fields.next()?.parse().ok()?,
    };
    let expected = hex(&hmac(message(&claims).as_bytes()));
    constant_time_eq(expected.as_bytes(), mac.as_bytes()).then_some(claims)
}

/// Compares in time independent of where the inputs differ.
#[must_use]
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims() -> Claims {
        Claims {
            subagent_id: "sa-01J0000000000000000000000".into(),
            root_session_id: "ses_abc".into(),
            created_ms: 1_700_000_000_000,
        }
    }

    /// Scenario: Forged handle.
    #[test]
    fn an_altered_handle_does_not_verify() {
        let handle = issue(&claims());
        assert_eq!(verify(&handle), Some(claims()));
        let mut forged = handle.clone();
        let last = forged.pop().unwrap();
        forged.push(if last == '0' { '1' } else { '0' });
        assert_eq!(verify(&forged), None);
        let foreign = handle.replace("ses_abc", "ses_xyz");
        assert_eq!(verify(&foreign), None);
        assert_eq!(verify("garbage"), None);
    }
}
