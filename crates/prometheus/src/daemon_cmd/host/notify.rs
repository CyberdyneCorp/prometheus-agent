//! Background notifications of daemon sessions to their chat (spec:
//! messaging-gateway, Background and internal notifications): a gateway
//! session's terminal hands each background process notice (an exit, a
//! watched line, a heartbeat) to [`completions`], which injects it through
//! `Gateway::notify` (filtered
//! there by `gateway.notifications.background`). A dropped admission (a
//! human follow-up waits, or the conversation's lane is not up yet) is
//! retried with backoff and is not a delivery attempt. Background
//! subagents (subagent-delegation 38.3) deliver through [`deliver`] too.

use std::time::Duration;

use pmt_gateway::messaging::{Admission, Background, BackgroundKind};
use pmt_tools::terminal::process::Notice;
use tokio::sync::mpsc;
use tracing::Instrument as _;

use super::send_tool::GatewayCell;

/// First wait after a dropped admission; doubles up to [`MAX_WAIT`].
const FIRST_WAIT: Duration = Duration::from_secs(1);
const MAX_WAIT: Duration = Duration::from_secs(30);
/// How long a notification keeps being retried before it is given up
/// (logged): a conversation that stays unreachable this long has moved on.
const GIVE_UP_AFTER: Duration = Duration::from_hours(1);

/// The sender a gateway session's terminal reports its background
/// processes to; its task ends when the session's terminal is dropped.
pub(super) fn completions(
    gateway: GatewayCell,
    profile: String,
    session: String,
) -> mpsc::UnboundedSender<Notice> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Notice>();
    tokio::spawn(
        async move {
            while let Some(notice) = rx.recv().await {
                deliver(&gateway, &profile, &session, of_notice(&notice)).await;
            }
        }
        .in_current_span(),
    );
    tx
}

/// The chat notification of a background process notice: an exit is a
/// result (exit 0) or an error, a watched line or heartbeat is progress.
pub(crate) fn of_notice(notice: &Notice) -> Background {
    let kind = match notice {
        Notice::Exited(done) if done.exit_code == 0 => BackgroundKind::Result,
        Notice::Exited(_) => BackgroundKind::Error,
        Notice::Matched(_) | Notice::Heartbeat(_) => BackgroundKind::Progress,
    };
    Background {
        kind,
        text: notice.text(),
    }
}

/// Injects `event` into the chat of `session`, retrying while the
/// admission is dropped (or the gateway is not up yet).
pub(crate) async fn deliver(
    gateway: &GatewayCell,
    profile: &str,
    session: &str,
    event: Background,
) {
    let started = tokio::time::Instant::now();
    let mut wait = FIRST_WAIT;
    loop {
        let admission = gateway
            .get()
            .map(|g| g.notify(profile, session, event.clone()));
        if !matches!(admission, None | Some(Admission::Dropped)) {
            return;
        }
        if started.elapsed() >= GIVE_UP_AFTER {
            tracing::warn!(
                profile,
                session,
                "background notification given up: the conversation stayed unreachable"
            );
            return;
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(MAX_WAIT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exit_code_picks_the_kind() {
        use pmt_tools::terminal::process::Completion;
        let done = |exit_code| {
            Notice::Exited(Completion {
                process_id: "proc_1".into(),
                command: "make test".into(),
                exit_code,
            })
        };
        let ok = of_notice(&done(0));
        assert_eq!(ok.kind, BackgroundKind::Result);
        assert_eq!(ok.text, "Background process proc_1 finished: make test");
        let failed = of_notice(&done(2));
        assert_eq!(failed.kind, BackgroundKind::Error);
        assert!(failed.text.contains("failed (exit 2)"), "{}", failed.text);
    }
}
