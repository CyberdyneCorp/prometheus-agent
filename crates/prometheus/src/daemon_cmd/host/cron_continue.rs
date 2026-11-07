//! Continuable scheduled deliveries (spec: scheduled-jobs, Continuable
//! deliveries): the delivered text becomes the first user turn of the
//! conversation a reply continues. Where the adapter opens threads, each
//! delivery opens a new one whose new session is seeded and parked for
//! it, so the first reply there continues it. On a thread-less adapter,
//! an explicit thread, or an `in_channel` surface (an adapter declaring
//! it and `cron.continuable_surface.<platform> = "in_channel"`), the seed
//! is appended to the chat's session (a new one parked for the chat when
//! it has none).

use pmt_config::schema::ContinuableSurface as Configured;
use pmt_core::error::Error;
use pmt_gateway::messaging::descriptor::ContinuableSurface;
use pmt_gateway::messaging::target::Resolved;
use pmt_gateway::messaging::{Agents as _, Conversation, Gateway};

use super::DaemonHost;

/// The seed's message kind (a user-role notice).
const SEED_KIND: &str = "cron_delivery";

impl DaemonHost {
    /// Links a delivery of job `job_name`'s `body` to `to` to a
    /// conversation; where it is sent (a newly opened thread, else `to`).
    ///
    /// # Errors
    ///
    /// The adapter failing to open a thread; store errors.
    pub(super) async fn continue_delivery(
        &self,
        gateway: &Gateway,
        profile: &str,
        to: Resolved,
        (job_name, body): (&str, &str),
    ) -> Result<Resolved, Error> {
        let configured = self
            .profile(profile)?
            .scope()
            .config()
            .cron
            .continuable_surface
            .get(&to.platform)
            .copied()
            .unwrap_or_default();
        let in_channel = configured == Configured::InChannel
            && gateway.continuable_surface(profile, &to.platform) == ContinuableSurface::InChannel;
        let named_thread = to.thread_id.is_some();
        let to = if in_channel {
            to
        } else {
            gateway.thread_for(profile, to, job_name).await?
        };
        let seed = pmt_agent::turn::transcript::notice_row(
            &pmt_cron::deliver::continuation_seed(job_name, body),
            SEED_KIND,
        );
        let opened = !named_thread && to.thread_id.is_some();
        if !opened && gateway.append_to_chat(profile, &to, seed.clone(), None)? {
            return Ok(to);
        }
        let session = self
            .create_conversation(
                profile,
                Conversation {
                    platform: to.platform.clone(),
                    chat_id: to.chat_id.clone(),
                    thread_id: to.thread_id.clone(),
                    user_id: "cron".to_owned(),
                    parent: None,
                    background: false,
                    policy: None,
                },
            )
            .await?;
        let store = &self.profile(profile)?.store;
        let id = session.parse()?;
        store.append_message(id, &seed)?;
        store.park_handoff(&to.platform, &to.chat_id, to.thread_id.as_deref(), id)?;
        tracing::info!(
            destination = to.label(),
            session,
            "continuable delivery seeded its conversation"
        );
        Ok(to)
    }
}
