//! The Agent-to-Agent (A2A) protocol (spec: agent-interop): the wire shapes of
//! A2A v1.0 and its pre-1.0 aliases, the Agent Card, push-notification
//! signatures and the outbound client the `a2a_*` tools use. The inbound
//! server lives with the daemon's other HTTP surfaces (`pmt-api`); this
//! crate holds what both directions share and nothing that touches a
//! session.

pub mod card;
pub mod client;
pub mod sign;
pub mod wire;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

/// Characters left unencoded in conversation key components.
const KEEP: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.');

fn encode(text: &str) -> String {
    utf8_percent_encode(text, KEEP).to_string()
}

/// The key of an inbound A2A conversation (spec: agent-interop, A2A
/// conversation identity): `<profile>:a2a:<peer>[@<tenant>]:<context>`,
/// each component percent-encoded so no component can forge another's
/// separators.
#[must_use]
pub fn conversation_key(profile: &str, peer: &str, tenant: Option<&str>, context: &str) -> String {
    let tenant = tenant
        .filter(|t| !t.is_empty())
        .map(|t| format!("@{}", encode(t)))
        .unwrap_or_default();
    format!(
        "{}:a2a:{}{tenant}:{}",
        encode(profile),
        encode(peer),
        encode(context)
    )
}

#[cfg(test)]
mod tests {
    use super::conversation_key;

    /// Scenario: Same context id from two peers.
    #[test]
    fn two_peers_never_share_a_key() {
        let alice = conversation_key("default", "alice", None, "ctx-1");
        let bob = conversation_key("default", "bob", None, "ctx-1");
        assert_ne!(alice, bob);
        assert_eq!(alice, "default:a2a:alice:ctx-1");
        // A peer name cannot forge another peer's context.
        assert_ne!(
            conversation_key("default", "a:b", None, "c"),
            conversation_key("default", "a", None, "b:c")
        );
        assert_eq!(
            conversation_key("default", "alice", Some("t 1"), "c"),
            "default:a2a:alice@t%201:c"
        );
    }
}
