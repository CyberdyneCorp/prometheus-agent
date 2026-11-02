//! The approval card model's callback data (spec: platform-adapters,
//! Interactive approval cards on every platform): a button carries
//! `appr:<id>:<choice>:<mac>`, where `<mac>` is an HMAC-SHA256 over the
//! request id and choice under a key drawn at every daemon start, so a
//! forged or replayed-across-restarts callback never decides anything.
//! The same choices render as a numbered list (`numbered` adapters) or as
//! `/approve <id> <choice>` instructions (`none`).

use sha2::{Digest as _, Sha256};

use super::descriptor::Interactive;

/// The approval choices, in card order.
pub(crate) const CHOICES: [&str; 4] = ["once", "session", "always", "deny"];

/// Hex characters of the MAC kept in callback data (128 bits).
const MAC_HEX: usize = 32;

/// The per-boot key.
pub(crate) struct CardKey([u8; 32]);

impl std::fmt::Debug for CardKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CardKey(..)")
    }
}

impl CardKey {
    /// A fresh random key.
    pub(crate) fn random() -> Self {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).expect("the OS random source works");
        Self(key)
    }

    /// The callback data of `choice` on request `id`.
    pub(crate) fn data(&self, id: &str, choice: &str) -> String {
        format!("appr:{id}:{choice}:{}", self.mac(id, choice))
    }

    /// The request id and choice of valid callback data; `None` for
    /// anything malformed or with a MAC that does not verify.
    pub(crate) fn verify<'a>(&self, data: &'a str) -> Option<(&'a str, &'a str)> {
        let rest = data.strip_prefix("appr:")?;
        let (rest, mac) = rest.rsplit_once(':')?;
        let (id, choice) = rest.rsplit_once(':')?;
        let expected = self.mac(id, choice);
        constant_time_eq(expected.as_bytes(), mac.as_bytes()).then_some((id, choice))
    }

    fn mac(&self, id: &str, choice: &str) -> String {
        let digest = hmac_sha256(&self.0, format!("{id}\u{1f}{choice}").as_bytes());
        crate::tickets::hex(&digest)[..MAC_HEX].to_owned()
    }
}

/// HMAC-SHA256 (RFC 2104) with a 32-byte key.
fn hmac_sha256(key: &[u8; 32], message: &[u8]) -> [u8; 32] {
    let mut inner_pad = [0x36u8; 64];
    let mut outer_pad = [0x5cu8; 64];
    for (i, byte) in key.iter().enumerate() {
        inner_pad[i] ^= byte;
        outer_pad[i] ^= byte;
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

/// Compares without an early exit, so timing tells nothing of the MAC.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// How a card's choices are offered in its text, per interactive tier.
pub(crate) fn instructions(tier: Interactive, id: &str) -> String {
    match tier {
        Interactive::Native => String::new(),
        Interactive::Numbered => format!(
            "\n1 Once / 2 Session / 3 Always / 4 Deny\nReply with the number (or the word), optionally followed by {id}."
        ),
        Interactive::None => format!(
            "\nReply /approve {id} once, /approve {id} session, /approve {id} always or /deny {id}."
        ),
    }
}

/// The choice a numbered reply stands for (`1`..`4` or the word,
/// optionally followed by the prompt id) and the id it quotes.
pub(crate) fn numbered_reply(text: &str) -> Option<(&'static str, Option<&str>)> {
    let mut words = text.split_whitespace();
    let first = words.next()?.to_lowercase();
    let quoted = words.next();
    if words.next().is_some() {
        return None;
    }
    let choice = match first.as_str() {
        "1" => "once",
        "2" => "session",
        "3" => "always",
        "4" => "deny",
        word => CHOICES.into_iter().find(|c| *c == word)?,
    };
    Some((choice, quoted))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_data_signed_with_this_boots_key_verifies() {
        let key = CardKey::random();
        let data = key.data("srq-42", "always");
        assert_eq!(key.verify(&data), Some(("srq-42", "always")));
        assert_eq!(key.verify("appr:srq-42:always:deadbeef"), None);
        let tampered = data.replace(":always:", ":once:");
        assert_eq!(key.verify(&tampered), None);
        assert_eq!(CardKey::random().verify(&data), None);
    }

    #[test]
    fn hmac_matches_the_rfc_4231_vector() {
        // RFC 4231 test case 2 uses a 4-byte key; pad it to 32 bytes the
        // way HMAC does (zeros) and the result is the same.
        let mut key = [0u8; 32];
        key[..4].copy_from_slice(b"Jefe");
        let mac = hmac_sha256(&key, b"what do ya want for nothing?");
        let hex = crate::tickets::hex(&mac);
        assert_eq!(
            hex,
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn numbered_replies_name_a_choice() {
        assert_eq!(numbered_reply("1"), Some(("once", None)));
        assert_eq!(numbered_reply("deny srq-9"), Some(("deny", Some("srq-9"))));
        assert_eq!(numbered_reply("5"), None);
        assert_eq!(numbered_reply("1 is my answer"), None);
    }
}
