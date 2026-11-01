//! Secrets a skill declares (spec: skills, Skill secrets and settings).
//!
//! A missing secret never hides the skill. On an interactive surface the
//! user is asked locally, input hidden, and the value goes straight into
//! the profile secrets store; the model only ever sees variable names.
//! Elsewhere the load result lists the missing names with the command
//! that sets them.

use pmt_config::secrets::SecretStore;
use pmt_core::secret::SecretString;

use crate::meta::SecretDecl;
use crate::session::SkillsSession;

/// Asks the user for a secret value (hidden input). `None` = declined.
pub trait SecretPrompt: Send + Sync {
    /// Asks for `name`, showing `prompt` when the skill gave one.
    fn ask(&self, skill: &str, name: &str, prompt: Option<&str>) -> Option<SecretString>;
}

/// The state of a skill's declared secrets after a load.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SecretsStatus {
    /// Already set.
    pub present: Vec<String>,
    /// Asked for and stored now.
    pub collected: Vec<String>,
    /// Still unset.
    pub missing: Vec<String>,
}

impl SecretsStatus {
    /// Every declared name.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        let mut names = self.present.clone();
        names.extend(self.collected.iter().cloned());
        names.extend(self.missing.iter().cloned());
        names
    }

    /// The note appended to the loaded body (names only), if any.
    #[must_use]
    pub fn note(&self) -> Option<String> {
        if self.missing.is_empty() && self.collected.is_empty() {
            return None;
        }
        let mut lines = vec!["[Skill secrets]".to_owned()];
        for name in &self.collected {
            lines.push(format!("{name}: stored in the profile secrets store"));
        }
        for name in &self.missing {
            lines.push(format!(
                "{name}: not set; ask the user to run `prometheus secrets set {name}` (never ask for the value in chat)"
            ));
        }
        Some(lines.join("\n"))
    }
}

/// Checks `declared` for skill `skill`, asking for missing values when the
/// session has a prompt.
pub async fn collect(
    session: &SkillsSession,
    skill: &str,
    declared: &[SecretDecl],
) -> SecretsStatus {
    let mut status = SecretsStatus::default();
    for decl in declared {
        if session.scope().secrets().resolve(&decl.name).is_ok() {
            status.present.push(decl.name.clone());
        } else if ask_and_store(session, skill, decl).await {
            status.collected.push(decl.name.clone());
        } else {
            status.missing.push(decl.name.clone());
        }
    }
    status
}

async fn ask_and_store(session: &SkillsSession, skill: &str, decl: &SecretDecl) -> bool {
    let Some(prompt) = session.setup().secret_prompt.clone() else {
        return false;
    };
    let (skill, name, text) = (skill.to_owned(), decl.name.clone(), decl.prompt.clone());
    let answer = tokio::task::spawn_blocking(move || prompt.ask(&skill, &name, text.as_deref()))
        .await
        .ok()
        .flatten();
    let Some(value) = answer else {
        return false;
    };
    let scope = session.scope();
    let store = SecretStore::open(scope.profile(), scope.config().secrets.backend);
    match store.set(&decl.name, &value) {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!(secret = %decl.name, error = %err, "skill secret not stored");
            false
        }
    }
}
