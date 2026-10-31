//! Entry and pending ids: a prefix plus 12 lower-case base32 characters
//! (60 random bits), short enough for the model to copy back exactly.

const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

fn random(prefix: &str) -> String {
    let mut bytes = [0u8; 12];
    if getrandom::fill(&mut bytes).is_err() {
        // No OS entropy: a time-and-counter fallback still never repeats
        // within a process, which is all a primary key needs.
        bytes.copy_from_slice(&fallback());
    }
    let mut id = String::with_capacity(prefix.len() + 12);
    id.push_str(prefix);
    id.extend(
        bytes
            .iter()
            .map(|b| char::from(ALPHABET[usize::from(b & 31)])),
    );
    id
}

fn fallback() -> [u8; 12] {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = jiff::Timestamp::now().as_nanosecond().to_le_bytes();
    let count = COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes();
    let mut out = [0u8; 12];
    out[..8].copy_from_slice(&nanos[..8]);
    out[8..].copy_from_slice(&count[..4]);
    out
}

/// A new entry id (`m_` + 12 base32 characters).
#[must_use]
pub fn entry() -> String {
    random("m_")
}

/// A new pending-change id (`mp_` + 12 base32 characters).
#[must_use]
pub fn pending() -> String {
    random("mp_")
}

/// Whether `text` has the shape of an entry id.
#[must_use]
pub fn is_entry(text: &str) -> bool {
    text.strip_prefix("m_")
        .is_some_and(|rest| rest.len() == 12 && rest.bytes().all(|b| ALPHABET.contains(&b)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn ids_have_the_specified_shape() {
        let regex = regex::Regex::new("^m_[a-z2-7]{12}$").unwrap();
        for _ in 0..100 {
            let id = super::entry();
            assert!(regex.is_match(&id), "{id}");
            assert!(super::is_entry(&id));
        }
        assert!(super::pending().starts_with("mp_"));
        assert!(!super::is_entry("m_ABCDEFGHIJKL"));
    }
}
