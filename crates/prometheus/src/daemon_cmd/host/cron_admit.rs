//! Scheduled output admitted into a profile's canonical Bot Chat (spec:
//! scheduled-jobs, Bot Chat delivery target). The target profile must be
//! served by this daemon (no fallback turn elsewhere); its Bot Chat is
//! obtained as `bot.chat.ensure` does (created on first delivery); the
//! sending outbox row's id is the receipt, recorded in the target's store
//! before the turn is submitted, so a retried delivery finds it and never
//! admits a second turn. The turn runs under the target profile's own
//! scope, queued behind any running turn of that chat.

use std::sync::Arc;

use pmt_config::schema::BusyInputMode;
use pmt_core::error::{Error, codes};
use pmt_gateway::Author;
use pmt_gateway::messaging::Gateway;
use pmt_rpc::contracts::{SessionSetup, SubmitParams};
use pmt_store::{NewSession, Source};

use super::{DaemonHost, Profile, setup};

/// One Bot Chat delivery.
pub(super) struct BotChatDelivery<'a> {
    /// The profile whose Bot Chat receives it.
    pub(super) target: &'a str,
    /// The receipt: the sending profile's outbox row id.
    pub(super) receipt: &'a str,
    /// The sending profile.
    pub(super) from: &'a str,
    /// The job that produced it.
    pub(super) job_id: &'a str,
    /// `[Scheduled: <job name>]` and the output.
    pub(super) text: &'a str,
}

impl DaemonHost {
    /// Admits `delivery` as one turn of the target profile's Bot Chat;
    /// the Bot Chat session (the one a retried receipt already went to).
    ///
    /// # Errors
    ///
    /// `profile.not_found` when the daemon does not serve the target;
    /// the turn's refusal (paused home, exhausted budget); store errors.
    pub(super) async fn admit_bot_chat(
        &self,
        gateway: &Gateway,
        delivery: &BotChatDelivery<'_>,
    ) -> Result<String, Error> {
        let profile = self.profile(delivery.target).map_err(|_| {
            Error::new(
                &codes::PROFILE_NOT_FOUND,
                format!("profile '{}' is not served by this daemon", delivery.target),
            )
        })?;
        let session = self.ensure_bot_chat(&profile).await?;
        let admitted = profile.store.cron_admit_receipt(
            delivery.receipt,
            delivery.from,
            delivery.job_id,
            &session,
        )?;
        if let Some(existing) = admitted {
            tracing::info!(
                receipt = delivery.receipt,
                "bot chat delivery already admitted; no second turn"
            );
            return Ok(existing);
        }
        let params = SubmitParams {
            session_id: session.clone(),
            text: delivery.text.to_owned(),
            images: Vec::new(),
            run_budget_secs: None,
            input_id: None,
            author: None,
        };
        let author = Author {
            client_kind: "cron".to_owned(),
            client_id: format!("{}/{}", delivery.from, delivery.job_id),
            principal: delivery.target.to_owned(),
            sent_at: Some(jiff::Timestamp::now().to_string()),
            prefix: None,
            origin: None,
        };
        let link = gateway.link(&session);
        self.submit_turn(&profile, params, (author, link), Some(BusyInputMode::Queue))
            .await?;
        Ok(session)
    }

    /// The profile's canonical Bot Chat, created (with the profile's
    /// primary model) when it has none.
    async fn ensure_bot_chat(&self, profile: &Arc<Profile>) -> Result<String, Error> {
        let cwd = Self::cwd_of(profile, None);
        std::fs::create_dir_all(&cwd).map_err(|e| Error::io(&codes::HOME_IO, &cwd, &e))?;
        let defaults = SessionSetup::default();
        let factory = self.factory(profile, cwd.clone(), &defaults)?;
        let target = factory.models.primary(&setup::flags(&defaults)).await?;
        // The desktop's conversation with the bot; `-c` (CLI and TUI
        // sessions) never resumes it by accident.
        let mut new = NewSession::new(Source::Desktop);
        new.provider = Some(target.provider.clone());
        new.model.clone_from(&target.model);
        new.cwd = Some(cwd.to_string_lossy().into_owned());
        let (session, created) = profile.store.ensure_bot_chat(&new)?;
        if created {
            tracing::info!(session = %session.id, "canonical bot chat created");
        }
        Ok(session.id.to_string())
    }
}
