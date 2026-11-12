//! What the daemon's scheduled jobs and session automation need of the
//! gateway beyond sending (spec: scheduled-jobs, Continuable deliveries;
//! Bot Chat delivery target; session-automation, Goal messages on
//! messaging platforms; Heartbeat ownership, recovery and silence on the
//! gateway): where an adapter lets a continuation land, a link for a turn
//! the daemon itself starts, and automated turns and status lines in a
//! conversation (its lane runs them, so they arbitrate with the chat's
//! own input and are delivered like any reply, without quoting).

use pmt_store::gateway::outbox::{NewOutbound, OutboundKind};
use tokio::sync::oneshot;

use crate::Link;

use super::Gateway;
use super::adapter::Admission;
use super::descriptor::ContinuableSurface;
use super::key::{KeyParts, Routing, SessionKey};
use super::lane::{self, Msg};

/// An automated turn for a conversation.
#[derive(Debug)]
pub struct Automated {
    /// `goal`, `loop` or `heartbeat` (the stored message's `origin`).
    pub origin: String,
    /// The user-role text.
    pub text: String,
    /// A status line said first (`Continuing toward goal (1/20): …`).
    pub notice: Option<String>,
    /// Told whether its turn started: the lane can still drop it after
    /// [`Gateway::automate`] accepted it (a person's input reached the
    /// chat in between), and the host then gives back what it reserved.
    pub started: Option<oneshot::Sender<bool>>,
}

/// What the host decided at the end of a conversation's turn: status lines
/// to say after the answer, and the automated turn to run next.
#[derive(Debug, Default)]
pub struct AfterTurn {
    /// Status lines (`Goal achieved`, `Goal paused …`).
    pub notices: Vec<String>,
    /// A goal continuation.
    pub turn: Option<Automated>,
}

impl Automated {
    /// Heartbeat turns are quiet: no typing indicator, tool progress or
    /// streamed draft, and a `NO_REPLY` answer sends nothing.
    #[must_use]
    pub fn quiet(&self) -> bool {
        self.origin == "heartbeat"
    }
}

impl Gateway {
    /// Where `platform`'s adapter of `profile` lets a continuable delivery
    /// land: `in_channel` only where it declares that surface.
    #[must_use]
    pub fn continuable_surface(&self, profile: &str, platform: &str) -> ContinuableSurface {
        self.shared
            .adapters
            .descriptor(profile, platform)
            .map(|d| d.continuable_surface)
            .unwrap_or_default()
    }

    /// A link to `session_id`'s attached clients, for a turn the daemon
    /// starts on its own (a scheduled delivery admitted into a Bot Chat):
    /// whoever watches the session sees it run.
    #[must_use]
    pub fn link(&self, session_id: &str) -> Link {
        self.shared.server.link(session_id)
    }

    /// Starts `automated` in the conversation of `session_id` when it is
    /// idle: `Accepted`, or `Dropped` while it runs a turn, a person's
    /// input waits, or the session is no conversation's (the caller gives
    /// back what it reserved). An accepted one can still be dropped by the
    /// lane: its `started` sender says so. A conversation without a lane
    /// (after a restart) gets one from its stored route.
    #[must_use]
    pub fn automate(&self, profile: &str, session_id: &str, automated: Automated) -> Admission {
        let handle = self
            .shared
            .lane_of_session(session_id)
            .or_else(|| self.lane_from_route(profile, session_id));
        let Some(handle) = handle else {
            return Admission::Dropped;
        };
        if handle.running() || handle.human_pending() {
            return Admission::Dropped;
        }
        handle.send(Msg::Automated(automated));
        Admission::Accepted
    }

    /// Resumes the turn a restart interrupted in the conversation of
    /// `session_id` as its lane's turn, so the answer reaches the chat
    /// (spec: messaging-gateway, Crash recovery and clean shutdown marker).
    pub(super) fn resume_interrupted(&self, profile: &str, session_id: &str) {
        let resume = Automated {
            origin: super::RESUME_ORIGIN.to_owned(),
            text: String::new(),
            notice: Some(pmt_i18n::t!(
                self.shared.strings(profile),
                "gateway.restarted_resuming"
            )),
            started: None,
        };
        if self.automate(profile, session_id, resume) == Admission::Dropped {
            tracing::warn!(profile, session_id, "the interrupted turn was not resumed");
        }
    }

    /// Says `text` in the chat of `session_id` (a status line such as
    /// `Goal achieved`); nothing when the session is no conversation's.
    pub fn say_in_session(&self, profile: &str, session_id: &str, text: &str) {
        if let Some(handle) = self.shared.lane_of_session(session_id) {
            handle.send(Msg::Say(text.to_owned()));
            return;
        }
        let Some(route) = self.route_of(profile, session_id) else {
            return;
        };
        let new = NewOutbound {
            idempotency_key: format!("automation:{}", crate::tickets::random_hex(8)),
            session_id: Some(session_id.to_owned()),
            platform: route.platform,
            chat_id: route.chat_id,
            thread_id: route.thread_id,
            reply_to: None,
            edit_of: None,
            kind: OutboundKind::System,
            body: text.to_owned(),
            attachments: Vec::new(),
            notify: None,
        };
        self.shared.deliver(profile, &new);
    }

    /// The route the conversation of `session_id` is bound to now.
    fn route_of(
        &self,
        profile: &str,
        session_id: &str,
    ) -> Option<pmt_store::gateway::routes::Route> {
        let store = self.shared.store(profile)?;
        let id = session_id.parse().ok()?;
        let routed = store.routed_session(id).ok()??;
        store
            .route(&routed.route_key)
            .ok()?
            .filter(|r| r.session_id == id)
    }

    /// A lane for the conversation `session_id` is routed from (none when
    /// the key routes elsewhere now).
    fn lane_from_route(&self, profile: &str, session_id: &str) -> Option<lane::Handle> {
        let route = self.route_of(profile, session_id)?;
        let parts = KeyParts::parse(&route.key)?;
        let routing = Routing {
            adapter: format!("{profile}/{}", parts.platform),
            profile: profile.to_owned(),
            platform: parts.platform,
            scope_id: parts.scope_id,
            sender_id: parts
                .participant
                .clone()
                .unwrap_or_else(|| parts.chat_id.clone()),
            chat_id: parts.chat_id,
            chat_type: parts.chat_type,
            thread_id: parts.thread_id,
            person_id: None,
        };
        Some(lane::open(
            &self.shared,
            &SessionKey::from_stored(route.key),
            &routing,
        ))
    }
}
