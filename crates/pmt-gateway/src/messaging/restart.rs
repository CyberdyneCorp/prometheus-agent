//! Restart notices (spec: messaging-gateway, Restart and update from
//! chat). A `/restart` leaves `<home>/run/restart_notice.json` naming the
//! chat (and thread) that asked; the next daemon posts "Gateway restarted
//! (<build>)" there once that platform is connected. A restart that cut
//! running turns short leaves `<home>/run/restart_interrupted`; the next
//! daemon then tells every platform's home channel once (unless its
//! `restart_notification` is false). An update from chat leaves
//! `<home>/run/update_notice.json`; the daemon that starts on the new build
//! waits for the update's receipt and says `updated to <version>` to that
//! chat (spec: distribution-and-update, Update from chat).

use std::path::{Path, PathBuf};

use pmt_store::gateway::outbox::{NewOutbound, OutboundKind};
use serde::{Deserialize, Serialize};

use super::Shared;

const NOTICE_FILE: &str = "restart_notice.json";
const INTERRUPTED_FILE: &str = "restart_interrupted";
const UPDATE_NOTICE_FILE: &str = "update_notice.json";

/// How long the new daemon waits for the update that started it to finish.
const UPDATE_WAIT: std::time::Duration = std::time::Duration::from_mins(5);

/// What the home channels hear after a restart that interrupted turns.
pub(crate) const INTERRUPTED: &str = "The gateway restarted while turns were running; \
    the interrupted chats can /retry their last message.";

/// The chat a `/restart` came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Requester {
    pub(crate) profile: String,
    pub(crate) platform: String,
    pub(crate) chat_id: String,
    pub(crate) thread_id: Option<String>,
}

fn run_dir(home: &Path) -> PathBuf {
    home.join("run")
}

/// What the restarted daemon posts to the requester.
pub(crate) fn restarted(version: &str, build_id: &str) -> String {
    let build: String = build_id.chars().take(12).collect();
    format!("Gateway restarted ({version}, build {build})")
}

/// Records the chat a `/restart` came from.
pub(crate) fn remember(home: &Path, requester: &Requester) {
    let path = run_dir(home).join(NOTICE_FILE);
    let text = serde_json::to_string(requester).expect("a requester serializes");
    if let Err(err) = std::fs::write(&path, text) {
        tracing::warn!(path = %path.display(), "restart requester not recorded: {err}");
    }
}

/// Records the chat an update from chat came from, for the daemon the
/// update starts.
///
/// # Panics
///
/// Never: a requester always serializes.
pub fn remember_update(home: &Path, request: &crate::messaging::UpdateFromChat) {
    let requester = Requester {
        profile: request.profile.clone(),
        platform: request.platform.clone(),
        chat_id: request.chat_id.clone(),
        thread_id: request.thread_id.clone(),
    };
    let path = run_dir(home).join(UPDATE_NOTICE_FILE);
    let text = serde_json::to_string(&requester).expect("a requester serializes");
    if let Err(err) = std::fs::write(&path, text) {
        tracing::warn!(path = %path.display(), "update requester not recorded: {err}");
    }
}

/// The update finished without a new daemon: nobody is left to tell.
pub fn forget_update(home: &Path) {
    let _ = std::fs::remove_file(run_dir(home).join(UPDATE_NOTICE_FILE));
}

/// A stop for a restart did not drain: the next start tells the home
/// channels.
pub(crate) fn mark_interrupted(home: &Path) {
    let path = run_dir(home).join(INTERRUPTED_FILE);
    let stamp = jiff::Timestamp::now().as_millisecond().to_string();
    if let Err(err) = std::fs::write(&path, stamp) {
        tracing::warn!(path = %path.display(), "restart marker not written: {err}");
    }
}

/// Posts what the last restart left to say (each file is consumed).
pub(crate) fn announce(shared: &std::sync::Arc<Shared>) {
    let home = &shared.options.home;
    let identity = shared.server.identity();
    let path = run_dir(home).join(NOTICE_FILE);
    if let Ok(text) = std::fs::read_to_string(&path) {
        let _ = std::fs::remove_file(&path);
        if let Ok(requester) = serde_json::from_str::<Requester>(&text) {
            let new = NewOutbound {
                idempotency_key: format!("restarted:{}", identity.build_id),
                session_id: None,
                platform: requester.platform.clone(),
                chat_id: requester.chat_id.clone(),
                thread_id: requester.thread_id.clone(),
                reply_to: None,
                edit_of: None,
                kind: OutboundKind::System,
                body: restarted(&identity.version, &identity.build_id),
                attachments: Vec::new(),
                notify: None,
            };
            shared.deliver(&requester.profile, &new);
        }
    }
    announce_update(shared);
    let path = run_dir(home).join(INTERRUPTED_FILE);
    if let Ok(stamp) = std::fs::read_to_string(&path) {
        let _ = std::fs::remove_file(&path);
        for profile in shared.server.profiles() {
            tell_home_channels(shared, &profile, stamp.trim());
        }
    }
}

/// Says `updated to <version>` to the chat an update came from, once the
/// update's receipt says it succeeded.
fn announce_update(shared: &std::sync::Arc<Shared>) {
    let home = shared.options.home.clone();
    let path = run_dir(&home).join(UPDATE_NOTICE_FILE);
    let Some(requester) = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<Requester>(&text).ok())
    else {
        return;
    };
    let _ = std::fs::remove_file(&path);
    let shared = std::sync::Arc::clone(shared);
    pmt_core::spawn::spawn(async move {
        let deadline = tokio::time::Instant::now() + UPDATE_WAIT;
        let version = loop {
            match finished_update(&home) {
                Finished::Succeeded(version) => break version,
                Finished::Not => return,
                Finished::Running if tokio::time::Instant::now() >= deadline => return,
                Finished::Running => {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        };
        let new = NewOutbound {
            idempotency_key: format!("updated:{version}"),
            session_id: None,
            platform: requester.platform.clone(),
            chat_id: requester.chat_id.clone(),
            thread_id: requester.thread_id.clone(),
            reply_to: None,
            edit_of: None,
            kind: OutboundKind::System,
            body: format!("updated to {version}"),
            attachments: Vec::new(),
            notify: None,
        };
        shared.deliver(&requester.profile, &new);
    });
}

/// Where the latest update stands.
enum Finished {
    /// Still running (or its receipt not readable yet).
    Running,
    /// It installed this version.
    Succeeded(String),
    /// It ended without installing anything.
    Not,
}

fn finished_update(home: &Path) -> Finished {
    let path = home.join("updates").join("receipts").join("latest.json");
    let Some(receipt) = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
    else {
        return Finished::Running;
    };
    match (receipt["outcome"].as_str(), receipt["to"].as_str()) {
        (Some("running") | None, _) => Finished::Running,
        (Some("success"), Some(to)) => Finished::Succeeded(to.to_owned()),
        _ => Finished::Not,
    }
}

fn tell_home_channels(shared: &Shared, profile: &str, stamp: &str) {
    let Some(scope) = shared.agents.scope(profile) else {
        return;
    };
    for (platform, table) in &scope.config().gateway.platforms {
        let (true, true, Some(home)) = (
            table.enabled,
            table.restart_notification_on(),
            table.home_channel.as_ref(),
        ) else {
            continue;
        };
        let new = NewOutbound {
            idempotency_key: format!("restart-interrupted:{stamp}:{platform}"),
            session_id: None,
            platform: platform.clone(),
            chat_id: home.chat_id.clone(),
            thread_id: home.thread_id.clone(),
            reply_to: None,
            edit_of: None,
            kind: OutboundKind::System,
            body: INTERRUPTED.to_owned(),
            attachments: Vec::new(),
            notify: None,
        };
        shared.deliver(profile, &new);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_is_named_short() {
        assert_eq!(
            restarted("1.4.1", "0123456789abcdef0123"),
            "Gateway restarted (1.4.1, build 0123456789ab)"
        );
    }
}
