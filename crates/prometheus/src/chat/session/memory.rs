//! A session's memory: the built-in store's session plus the profile's
//! external memory provider started for it (spec: memory, External memory
//! provider contract; Provider tools and prompt block gating).

use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_config::schema::MemoryNotifications;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_memory::notices::Notices;
use pmt_memory::session::{MemorySession, MemorySetup};
use pmt_store::Source;

use super::Factory;
use super::model::SwitchableModel;
use super::plugins::stored_surface;

/// The memory notifier of `session` (spec: memory, Per-turn memory
/// notifications): `memory.notifications`, or a messaging conversation's
/// `gateway.platforms.<platform>.memory_notifications` when set. The
/// surface binds where the notices go.
#[must_use]
pub(super) fn notices_for(scope: &ProfileScope, session: &pmt_store::Session) -> Notices {
    let config = scope.config();
    let platform = match &session.origin {
        pmt_store::Origin::Gateway { platform, .. } => config
            .gateway
            .platforms
            .get(platform)
            .and_then(|p| p.memory_notifications),
        _ => None,
    };
    let mode: MemoryNotifications = platform.unwrap_or(config.memory.notifications);
    Notices::new(mode)
}

impl Factory {
    /// The memory of session `id`, with the profile's external provider
    /// when one runs (a disabled, unavailable or failing one is warned
    /// about once and left out).
    ///
    /// # Errors
    ///
    /// Store errors.
    pub(super) async fn memory_for(
        &self,
        id: SessionId,
        model: &Arc<SwitchableModel>,
        notices: Notices,
    ) -> Result<Arc<MemorySession>, Error> {
        let mut setup = self.memory_setup(id, model)?;
        setup.notices = notices;
        let surface = stored_surface(&self.store, id);
        let started = crate::memory_provider::start(
            &self.scope,
            &self.plugins,
            (&id.to_string(), &surface),
            &self.redactor,
            &self.plugin_call(id),
        )
        .await;
        setup.provider = started;
        Ok(Arc::new(MemorySession::new(setup)))
    }

    /// The model of a background memory review (spec: memory, Background
    /// review controls): auxiliary task `memory` when `[auxiliary.memory]`
    /// names a provider or model (the review then gets a digest of the
    /// conversation, `true`), else the session's own model (the whole
    /// transcript, `false`).
    pub async fn review_model(
        &self,
        main: &Arc<SwitchableModel>,
    ) -> (Arc<dyn pmt_agent::turn::model::ModelPort>, bool) {
        let aux = self.scope.config().auxiliary.memory.as_ref();
        let Some(aux) = aux.filter(|a| a.provider.is_some() || a.model.is_some()) else {
            return (Arc::clone(main) as _, false);
        };
        let mut target = main.target();
        if let Some(provider) = &aux.provider {
            target.provider.clone_from(provider);
        }
        target.model.clone_from(&aux.model);
        match self.models.port(&target).await {
            Ok(port) => (port as _, true),
            Err(err) => {
                tracing::warn!(
                    "auxiliary memory route unavailable ({err}); the review uses the session's model"
                );
                (Arc::clone(main) as _, false)
            }
        }
    }

    fn memory_setup(
        &self,
        id: SessionId,
        model: &Arc<SwitchableModel>,
    ) -> Result<MemorySetup, Error> {
        let mut setup = MemorySetup::new(
            self.scope.clone(),
            Arc::clone(&self.store),
            Some(id),
            Source::Cli,
        );
        setup.redactor = self.redactor.clone();
        setup.approver.clone_from(&self.approver);
        setup.embedder = self.models.embedder(Some(model));
        let prior = self
            .store
            .messages(id, pmt_store::Visibility::Active)?
            .iter()
            .filter(|m| m.role == pmt_store::Role::User)
            .count();
        setup.prior_user_turns = u32::try_from(prior).unwrap_or(u32::MAX);
        Ok(setup)
    }
}
