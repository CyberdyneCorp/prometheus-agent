//! Exactly-once prompt submission (spec: acp-adapter, Exactly-once submit
//! across lost acknowledgements): when `prompt.submit` fails before its
//! acknowledgement, the prompt's identity (content fingerprint, input id
//! and staged content) is kept for its session, at most 64 sessions; the
//! next prompt of that session with the same fingerprint reuses the input
//! id, so the daemon answers with the original admission instead of
//! running the work again. Any other prompt or control retires it.
//!
//! A kept identity is only good against the daemon instance that may have
//! admitted it: a daemon that restarted forgot its admissions, so the
//! identity names the instance and a resend to another one is refused as
//! an unknown execution instead.

use std::collections::VecDeque;

use pmt_rpc::contracts::{PromptImage, SubmitParams};
use sha2::{Digest as _, Sha256};

/// The most sessions with a kept identity.
pub(crate) const MAX_KEPT: usize = 64;

/// One kept prompt identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Kept {
    /// The content fingerprint.
    pub(crate) fingerprint: String,
    /// The input id the daemon may have admitted.
    pub(crate) input_id: String,
    /// The prompt as submitted (media already converted).
    pub(crate) params: SubmitParams,
    /// The daemon instance it was submitted to.
    pub(crate) instance: String,
}

/// The fingerprint of a prompt's content.
pub(crate) fn fingerprint(text: &str, images: &[PromptImage]) -> String {
    let mut hash = Sha256::new();
    hash.update(text.as_bytes());
    for image in images {
        hash.update([0]);
        hash.update(serde_json::to_vec(image).unwrap_or_default());
    }
    hash.finalize().iter().fold(String::new(), |mut hex, b| {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
        hex
    })
}

/// Kept identities by editor session, oldest first.
#[derive(Debug, Default)]
pub(crate) struct KeptPrompts {
    entries: VecDeque<(String, Kept)>,
}

impl KeptPrompts {
    /// Keeps `kept` for `session` (replacing its previous one); the
    /// oldest session's is dropped past [`MAX_KEPT`].
    pub(crate) fn keep(&mut self, session: &str, kept: Kept) {
        self.retire(session);
        if self.entries.len() == MAX_KEPT {
            self.entries.pop_front();
        }
        self.entries.push_back((session.to_owned(), kept));
    }

    /// The identity kept for `session`, when it has the same fingerprint.
    pub(crate) fn matching(&self, session: &str, fingerprint: &str) -> Option<&Kept> {
        self.entries
            .iter()
            .find(|(s, k)| s == session && k.fingerprint == fingerprint)
            .map(|(_, k)| k)
    }

    /// Drops `session`'s identity.
    pub(crate) fn retire(&mut self, session: &str) {
        self.entries.retain(|(s, _)| s != session);
    }
}

#[cfg(test)]
mod tests {
    use pmt_rpc::contracts::SubmitParams;

    use super::{Kept, KeptPrompts, MAX_KEPT, fingerprint};

    fn kept(text: &str) -> Kept {
        Kept {
            fingerprint: fingerprint(text, &[]),
            input_id: format!("in-{text}"),
            params: SubmitParams {
                session_id: "s".into(),
                text: text.into(),
                images: Vec::new(),
                files: Vec::new(),
                run_budget_secs: None,
                input_id: None,
                author: None,
                voice: false,
            },
            instance: "i1".into(),
        }
    }

    #[test]
    fn only_the_same_content_reuses_the_identity_and_the_oldest_session_goes_first() {
        let mut prompts = KeptPrompts::default();
        prompts.keep("a", kept("fix the build"));
        let same = fingerprint("fix the build", &[]);
        assert_eq!(
            prompts.matching("a", &same).unwrap().input_id,
            "in-fix the build"
        );
        assert!(prompts.matching("a", &fingerprint("other", &[])).is_none());
        assert!(prompts.matching("b", &same).is_none());
        for n in 0..MAX_KEPT {
            prompts.keep(&format!("s{n}"), kept("x"));
        }
        assert!(prompts.matching("a", &same).is_none(), "evicted");
        prompts.retire("s1");
        assert!(prompts.matching("s1", &fingerprint("x", &[])).is_none());
    }
}
