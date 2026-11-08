//! What leaves for a provider (spec: memory, Provider egress redaction and
//! write mirroring): every text passes secret redaction (known values and
//! credential-shaped tokens), and a skill invocation keeps only the user's
//! instruction, not the skill text the host wrapped around it. The stored
//! transcript is never touched.

use pmt_core::secret::Redactor;

/// The marker before the user's own words in a skill invocation.
const USER_INSTRUCTION: &str = "\n\nUser instruction: ";
/// Blocks the host adds to a skill invocation.
const SCAFFOLDING: &[&str] = &["[Skill config]", "[Skill secrets]"];

/// `text` without skill-invocation scaffolding.
#[must_use]
pub fn strip_scaffolding(text: &str) -> String {
    if let Some(at) = text.rfind(USER_INSTRUCTION) {
        return text[at + USER_INSTRUCTION.len()..].to_owned();
    }
    text.split("\n\n")
        .filter(|block| !SCAFFOLDING.iter().any(|s| block.starts_with(s)))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// `text` as a provider may receive it.
#[must_use]
pub fn outbound(redactor: &Redactor, text: &str) -> String {
    redactor.redact_all(&strip_scaffolding(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_redacted_and_scaffolding_dropped() {
        let out = outbound(
            &Redactor::default(),
            "deploy notes\n\nUser instruction: use sk-live-abcdef1234567890 for the API",
        );
        assert!(!out.contains("sk-live-abcdef1234567890"), "{out}");
        assert!(!out.contains("deploy notes"), "{out}");
        assert!(out.starts_with("use "), "{out}");
        assert_eq!(strip_scaffolding("[Skill config]\na=1\n\nhello"), "hello");
    }
}
