//! The consolidation review on the `auxiliary.curator` route (default: the
//! profile's main model), in a hidden session of its own so its usage is
//! accounted and no conversation's prompt cache is touched.

use std::path::PathBuf;
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_core::env::LaunchEnv;
use pmt_core::error::Error;
use pmt_skills::curator::consolidate::{self, Candidate, Consolidator, Merge};
use pmt_store::Store;
use pmt_store::model::Source;
use pmt_store::sessions::NewSession;

use crate::chat::session::model::{Models, RouteFlags};

/// The review of one profile.
pub(crate) struct ModelReview<'a> {
    scope: &'a ProfileScope,
    store: &'a Arc<Store>,
    launch: &'a LaunchEnv,
}

impl<'a> ModelReview<'a> {
    pub(crate) fn new(
        scope: &'a ProfileScope,
        store: &'a Arc<Store>,
        launch: &'a LaunchEnv,
    ) -> Self {
        Self {
            scope,
            store,
            launch,
        }
    }
}

impl Consolidator for ModelReview<'_> {
    fn propose(&self, candidates: &[Candidate]) -> Result<Vec<Merge>, Error> {
        let session = self.store.create_session(&NewSession {
            source_tag: Some("curator".to_owned()),
            title: Some("Skill curator review".to_owned()),
            hidden: true,
            ..NewSession::new(Source::Cli)
        })?;
        let models = Models::open(
            self.scope,
            self.store,
            self.launch.user_home().map(PathBuf::as_path),
            &RouteFlags::default(),
            None,
        )?;
        let prompt = consolidate::prompt(candidates);
        let reply = crate::model_cmd::block_on(models.ask_auxiliary(
            pmt_providers::auxiliary::AuxTask::Curator,
            session.id,
            consolidate::INSTRUCTIONS,
            &prompt,
            8_000,
        ))??;
        match reply {
            Some(text) => consolidate::parse(&text),
            None => Ok(Vec::new()),
        }
    }
}
