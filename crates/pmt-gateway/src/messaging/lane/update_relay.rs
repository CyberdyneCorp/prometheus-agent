//! Relays a running update from chat (spec: distribution-and-update,
//! Update from chat): the updater's stage lines (`checking`, `downloading
//! <n>%`, `verifying`, `self-test`, `swapping`, `restarting`) go to the
//! chat that asked, at most one message per 2 s (a newer stage replaces
//! one still waiting); then its result or refusal (another owner, lock
//! held, signature failure). An updater that stopped because the daemon
//! is busy (exit 4) makes the lane ask for a yes. After a handover this
//! daemon is gone; the new one says `updated to <version>`
//! (`messaging::restart`).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use super::Msg;
use crate::messaging::agents::UpdateEvent;
use crate::messaging::key::Routing;

/// The least time between two messages of one update.
pub(crate) const MIN_GAP: Duration = Duration::from_secs(2);

/// Exit code of an updater whose busy-daemon restart needs a yes.
const NEEDS_YES: i32 = 4;

/// Starts relaying `events` to the lane behind `lane`.
pub(super) fn spawn(
    lane: mpsc::WeakUnboundedSender<Msg>,
    routing: Routing,
    events: mpsc::UnboundedReceiver<UpdateEvent>,
    strings: Arc<pmt_i18n::Strings>,
) {
    pmt_core::spawn::spawn(relay(lane, routing, events, strings));
}

async fn relay(
    lane: mpsc::WeakUnboundedSender<Msg>,
    routing: Routing,
    mut events: mpsc::UnboundedReceiver<UpdateEvent>,
    strings: Arc<pmt_i18n::Strings>,
) {
    let mut last: Option<Instant> = None;
    let mut waiting: Option<String> = None;
    loop {
        let due = last.map_or_else(Instant::now, |at| at + MIN_GAP);
        let event = tokio::select! {
            event = events.recv() => event,
            () = tokio::time::sleep_until(due), if waiting.is_some() => {
                if let Some(text) = waiting.take() {
                    say(&lane, Msg::Say(text));
                    last = Some(Instant::now());
                }
                continue;
            }
        };
        match event {
            Some(UpdateEvent::Progress(stage)) => {
                if last.is_none_or(|at| at.elapsed() >= MIN_GAP) {
                    say(&lane, Msg::Say(stage));
                    last = Some(Instant::now());
                } else {
                    waiting = Some(stage);
                }
            }
            Some(UpdateEvent::Finished { code, last_line }) => {
                tokio::time::sleep_until(due).await;
                say(&lane, finished(&strings, code, &last_line, &routing));
                return;
            }
            None => return,
        }
    }
}

/// The final message, or the busy-daemon question.
fn finished(strings: &pmt_i18n::Strings, code: i32, last_line: &str, routing: &Routing) -> Msg {
    let line = last_line.trim().trim_start_matches("error: ").to_owned();
    match code {
        0 => Msg::Say(line),
        NEEDS_YES => Msg::UpdateBusy {
            routing: Box::new(routing.clone()),
            question: line,
        },
        _ => Msg::Say(pmt_i18n::t!(
            strings,
            "gateway.update_not_applied",
            line = line
        )),
    }
}

fn say(lane: &mpsc::WeakUnboundedSender<Msg>, msg: Msg) {
    if let Some(lane) = lane.upgrade() {
        let _ = lane.send(msg);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::mpsc;

    use super::{Msg, UpdateEvent, relay};
    use crate::messaging::key::{ChatType, Routing};

    fn routing() -> Routing {
        Routing {
            adapter: "test".to_owned(),
            profile: "default".to_owned(),
            platform: "test".to_owned(),
            scope_id: None,
            chat_id: "c1".to_owned(),
            chat_type: ChatType::Dm,
            thread_id: None,
            sender_id: "admin".to_owned(),
            person_id: None,
        }
    }

    fn said(msg: &Msg) -> Option<&str> {
        match msg {
            Msg::Say(text) => Some(text),
            _ => None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn stages_are_relayed_at_most_every_two_seconds() {
        let (lane, mut chat) = mpsc::unbounded_channel();
        let (tx, events) = mpsc::unbounded_channel();
        let task = pmt_core::spawn::spawn(relay(
            lane.downgrade(),
            routing(),
            events,
            pmt_i18n::Strings::english(pmt_i18n::Surface::Core),
        ));
        for line in [
            "checking the stable channel",
            "downloading 10%",
            "downloading 20%",
        ] {
            tx.send(UpdateEvent::Progress(line.to_owned())).unwrap();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            said(&chat.recv().await.unwrap()),
            Some("checking the stable channel")
        );
        assert!(chat.try_recv().is_err(), "the next waits 2 s");
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(said(&chat.recv().await.unwrap()), Some("downloading 20%"));
        tx.send(UpdateEvent::Finished {
            code: 69,
            last_line: "error: owner: homebrew: this install is updated by its package manager"
                .to_owned(),
        })
        .unwrap();
        task.await.unwrap();
        assert_eq!(
            said(&chat.recv().await.unwrap()),
            Some(
                "update not applied: owner: homebrew: this install is updated by its package manager"
            )
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_busy_daemon_is_asked_about_in_the_chat() {
        let (lane, mut chat) = mpsc::unbounded_channel();
        let (tx, events) = mpsc::unbounded_channel();
        tx.send(UpdateEvent::Finished {
            code: 4,
            last_line: "error: the daemon (pid 7) has 1 running turn(s); restart it?".to_owned(),
        })
        .unwrap();
        relay(
            lane.downgrade(),
            routing(),
            events,
            pmt_i18n::Strings::english(pmt_i18n::Surface::Core),
        )
        .await;
        match chat.recv().await.unwrap() {
            Msg::UpdateBusy { question, .. } => {
                assert_eq!(
                    question,
                    "the daemon (pid 7) has 1 running turn(s); restart it?"
                );
            }
            _ => panic!("expected the busy question"),
        }
    }
}
