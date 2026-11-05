//! A session's sudo handling (spec: execution-environments, Sudo password
//! handling): who may be asked for the password, and whether the session
//! may read the `SUDO_PASSWORD` secret.

use std::sync::Arc;

use pmt_config::scope::ProfileScope;
use pmt_core::capability::{Capability, CapabilitySet};
use pmt_core::secret::SecretString;
use pmt_skills::secrets::SecretPrompt;
use pmt_store::Origin;
use pmt_tools::terminal::sudo::{SECRET_NAME, SecretLookup, Sudo, SudoPrompt, Surface};

/// The sudo password asked through a hidden secret prompt (the CLI line).
pub struct HiddenLine(pub Arc<dyn SecretPrompt>);

impl SudoPrompt for HiddenLine {
    fn ask(&self, command: &str) -> Option<SecretString> {
        let why =
            format!("the sudo password for `{command}` (kept in memory for this session only)");
        self.0.ask("sudo", SECRET_NAME, Some(&why))
    }
}

/// The sudo handling of a session from `origin` with `capabilities`;
/// `prompt` is the session's interactive surface, if it has one.
#[must_use]
pub fn handling(
    scope: &ProfileScope,
    capabilities: &CapabilitySet,
    origin: &Origin,
    prompt: Option<Arc<dyn SudoPrompt>>,
) -> Sudo {
    let surface = match (origin, prompt) {
        (Origin::Subagent { .. }, _) => Surface::Subagent,
        (Origin::Gateway { .. }, _) => Surface::Messaging,
        (Origin::Cron { .. }, _) | (Origin::None, None) => Surface::Unattended,
        (Origin::None, Some(prompt)) => Surface::Interactive(prompt),
    };
    let granted = capabilities.allows(&Capability::SecretsRead(SECRET_NAME.to_owned()));
    let secret = granted.then(|| {
        let scope = scope.clone();
        Arc::new(move || scope.secrets().resolve(SECRET_NAME).ok()) as SecretLookup
    });
    Sudo::new(surface, secret)
}
