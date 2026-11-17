//! `--now` harness activations (spec: harness-evolution, Harness loaded
//! once per session).
//!
//! An activation with `--now` writes `<profile>/harness/NOW` (the version
//! and when) and marks every open session for a prompt rebuild. At its
//! next turn such a session rebuilds, and its prompt refresher swaps in
//! the `NOW` version's layer when the activation came after the session
//! started on another version, logging the cache break at WARN. Without
//! `--now` nothing reaches an open session.

use std::sync::Arc;

use jiff::Timestamp;
use pmt_agent::turn::{PromptSetup, RefreshPrompt};
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_store::Store;

const NOW_FILE: &str = "NOW";

/// The last `--now` activation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Now {
    /// The version it activated.
    pub version: String,
    /// When.
    pub at: Timestamp,
}

/// The last `--now` activation of the profile, if any.
#[must_use]
pub fn read(scope: &ProfileScope) -> Option<Now> {
    let path = pmt_config::harness::dir(scope.dir()).join(NOW_FILE);
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    let version = lines.next()?.trim().to_owned();
    let at = lines.next()?.trim().parse().ok()?;
    Some(Now { version, at })
}

/// Records a `--now` activation of `version` and marks the open sessions
/// of `store` for a rebuild; the number marked.
///
/// # Errors
///
/// `harness.io` writing the marker; store errors.
pub fn activate(scope: &ProfileScope, store: &Store, version: &str) -> Result<usize, Error> {
    let path = pmt_config::harness::dir(scope.dir()).join(NOW_FILE);
    let text = format!("{version}\n{}\n", Timestamp::now());
    pmt_core::private_fs::write_atomic(&path, text.as_bytes())
        .map_err(|e| Error::io(&codes::HARNESS_IO, &path, &e))?;
    let open = store.open_sessions_with_prompt()?;
    for session in &open {
        store.mark_prompt_refresh(*session)?;
    }
    Ok(open.len())
}

/// `base` followed by the `--now` check of session `id`.
#[must_use]
pub fn refresher(
    base: RefreshPrompt,
    scope: ProfileScope,
    store: Arc<Store>,
    id: SessionId,
) -> RefreshPrompt {
    Arc::new(move |prompt: &mut PromptSetup| {
        base(prompt);
        apply(&scope, &store, id, prompt);
    })
}

fn apply(scope: &ProfileScope, store: &Store, id: SessionId, prompt: &mut PromptSetup) {
    let Some(now) = read(scope) else {
        return;
    };
    let current = store.session_harness(id).ok().flatten();
    if now.at <= prompt.started_at || current.as_deref() == Some(now.version.as_str()) {
        return;
    }
    prompt.harness = super::layer(scope, &now.version);
    if let Err(err) = store.set_session_harness(id, &now.version) {
        tracing::warn!(session = %id, error = %err, "harness version not recorded");
    }
    tracing::warn!(
        session = %id,
        from = current.as_deref().unwrap_or("none"),
        to = %now.version,
        "prompt cache invalidated by harness change"
    );
}
