//! How the bot presents itself (spec: platform-adapters, Telegram
//! presentation options):
//!
//! - `notifications = "important"` (the default) sends silently all but
//!   what the gateway marks as notifying — final replies, approvals and
//!   command confirmations; `all` notifies unless a send asks for silence.
//! - `pin_during_turn` pins the message that started a turn, silently,
//!   and unpins it when the turn ends.
//! - `status_indicator` shows `status_online` as the bot's short
//!   description while connected and `status_offline` after a clean stop.
//! - `drop_pending_on_cold_boot` drops the updates queued while the
//!   daemon was stopped on its first start only: a reconnect, or a restart
//!   of the adapter in the same daemon, keeps them.
//! - `disable_link_previews` sends messages without link previews.
//!
//! Status messages (the progress bubble, the long-running notice) are
//! edited in place by the gateway, which sends a new message when an edit
//! fails.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use pmt_config::platforms::TelegramNotifications;
use pmt_gateway::messaging::{Outgoing, TurnState};
use serde_json::json;

use crate::api::Api;
use crate::{PLATFORM, Telegram, chat};

/// The profiles whose bot this daemon process has already started.
static STARTED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

/// Whether this is the first start of the bot of the profile at `dir` in
/// this daemon process (later calls answer `false`).
pub(crate) fn cold_boot(dir: PathBuf) -> bool {
    STARTED
        .get_or_init(Mutex::default)
        .lock()
        .expect("started poisoned")
        .insert(dir)
}

impl Telegram {
    /// `message` with its notification settled by `options.notifications`.
    pub(crate) fn presented(&self, message: &Outgoing) -> Outgoing {
        let notifies = match self.options.notifications {
            TelegramNotifications::Important => message.notify == Some(true),
            TelegramNotifications::All => message.notify != Some(false),
        };
        Outgoing {
            notify: Some(notifies),
            ..message.clone()
        }
    }

    /// Whether links in messages are previewed.
    pub(crate) fn previews(&self) -> bool {
        !self.options.disable_link_previews
    }

    /// Whether this connect drops the updates queued while the daemon was
    /// stopped.
    pub(crate) fn drops_pending(&self) -> bool {
        // Counted on every connect, so a later one is never cold.
        let cold = cold_boot(self.scope.dir().to_path_buf());
        cold && self.options.drop_pending_on_cold_boot
    }

    /// Shows the bot online (or offline) as its short description, when
    /// `status_indicator` asks for it.
    pub(crate) async fn show_status(&self, api: &Api, online: bool) {
        if !self.options.status_indicator {
            return;
        }
        let text = if online {
            &self.options.status_online
        } else {
            &self.options.status_offline
        };
        let body = json!({"short_description": text});
        if let Err(err) = api.call("setMyShortDescription", &body).await {
            tracing::warn!(
                platform = PLATFORM,
                "status indicator not set: {}",
                err.describe()
            );
        }
    }

    /// Pins the message that started a turn while it runs, and unpins it
    /// when it ends (`pin_during_turn`).
    pub(crate) async fn pin_during_turn(&self, turn: &TurnState) {
        let Some(trigger) = turn
            .trigger
            .as_deref()
            .filter(|_| self.options.pin_during_turn)
        else {
            return;
        };
        let Ok(api) = self.api() else { return };
        let key = (turn.chat_id.clone(), trigger.to_owned());
        let body = json!({"chat_id": chat(&turn.chat_id), "message_id": chat(trigger)});
        if turn.running {
            let mut pin = body;
            pin["disable_notification"] = json!(true);
            match api.call("pinChatMessage", &pin).await {
                Ok(_) => {
                    self.pinned.lock().expect("pinned poisoned").insert(key);
                }
                Err(err) => tracing::debug!(
                    platform = PLATFORM,
                    "message not pinned during the turn: {}",
                    err.describe()
                ),
            }
            return;
        }
        // Only what this turn pinned is unpinned.
        if !self.pinned.lock().expect("pinned poisoned").remove(&key) {
            return;
        }
        if let Err(err) = api.call("unpinChatMessage", &body).await {
            tracing::debug!(
                platform = PLATFORM,
                "message not unpinned after the turn: {}",
                err.describe()
            );
        }
    }
}
