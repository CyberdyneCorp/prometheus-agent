//! A session's memory: the built-in store's session plus the profile's
//! external memory provider started for it (spec: memory, External memory
//! provider contract; Provider tools and prompt block gating).

use std::sync::Arc;

use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_memory::session::{MemorySession, MemorySetup};
use pmt_store::Source;

use super::Factory;
use super::model::SwitchableModel;
use super::plugins::stored_surface;

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
    ) -> Result<Arc<MemorySession>, Error> {
        let mut setup = self.memory_setup(id, model)?;
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
