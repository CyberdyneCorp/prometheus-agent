//! Relays a running update from chat (spec: distribution-and-update,
//! Update from chat): the updater's stage lines (`checking`, `downloading
//! <n>%`, `verifying`, `self-test`, `swapping`, `restarting`) go to the
//! chat that asked, at most one message per 2 s (a newer stage replaces
//! one still waiting); then its result or refusal (another owner, lock
//! held, signature failure). An updater that stopped because the daemon
//! is busy (exit 4) makes the lane ask for a yes. After a handover this
//! daemon is gone; the new one says `updated to <version>`
//! (`messaging::restart`). The gateway's shutdown waits for the relays
//! ([`settle`]): the last stage (`restarting`, written just before the
//! updater stops this daemon) is said, still 2 s after the one before,
//! and queued in the outbox before the daemon exits.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::Msg;
use crate::messaging::Shared;
use crate::messaging::agents::{UpdateEvent, UpdateFeed};
use crate::messaging::key::Routing;

/// The least time between two messages of one update.
pub(crate) const MIN_GAP: Duration = Duration::from_secs(2);

/// How long a stopping gateway waits for one relay: its last gap, and the
/// lane taking the line.
const SETTLE: Duration = Duration::from_secs(5);

/// Exit code of an updater whose busy-daemon restart needs a yes.
const NEEDS_YES: i32 = 4;

/// The relays of a gateway, which its shutdown waits for.
pub(crate) type Relays = Mutex<Vec<JoinHandle<()>>>;

/// Starts relaying `feed` to the lane behind `lane`.
pub(super) fn spawn(
    shared: &Shared,
    lane: mpsc::WeakUnboundedSender<Msg>,
    routing: Routing,
    feed: UpdateFeed,
    strings: Arc<pmt_i18n::Strings>,
) {
    let relay = Relay::new(lane, routing, strings);
    let task = pmt_core::spawn::spawn(run(relay, feed, shared.stop.clone()));
    let mut relays = shared.update_relays.lock().expect("update relays poisoned");
    relays.retain(|task| !task.is_finished());
    relays.push(task);
}

/// Waits until every relay has said its last stage (the gateway stops).
pub(crate) async fn settle(relays: &Relays) {
    let tasks = std::mem::take(&mut *relays.lock().expect("update relays poisoned"));
    for task in tasks {
        if tokio::time::timeout(SETTLE, task).await.is_err() {
            tracing::warn!("an update's last stage was not said before the daemon stopped");
        }
    }
}

/// One update's relay to its chat.
struct Relay {
    lane: mpsc::WeakUnboundedSender<Msg>,
    routing: Routing,
    strings: Arc<pmt_i18n::Strings>,
    /// When the last message was said.
    last: Option<Instant>,
    /// The newest stage not said yet.
    waiting: Option<String>,
}

impl Relay {
    fn new(
        lane: mpsc::WeakUnboundedSender<Msg>,
        routing: Routing,
        strings: Arc<pmt_i18n::Strings>,
    ) -> Self {
        Self {
            lane,
            routing,
            strings,
            last: None,
            waiting: None,
        }
    }

    /// When the next message may be said.
    fn due(&self) -> Instant {
        self.last.map_or_else(Instant::now, |at| at + MIN_GAP)
    }

    fn say_now(&mut self, text: String) {
        say(&self.lane, Msg::Say(text));
        self.last = Some(Instant::now());
    }

    fn progress(&mut self, stage: String) {
        if self.last.is_none_or(|at| at.elapsed() >= MIN_GAP) {
            self.say_now(stage);
        } else {
            self.waiting = Some(stage);
        }
    }

    fn say_waiting(&mut self) {
        if let Some(text) = self.waiting.take() {
            self.say_now(text);
        }
    }

    /// Says the updater's result once the gap allows.
    async fn finish(self, code: i32, last_line: &str) {
        tokio::time::sleep_until(self.due()).await;
        say(
            &self.lane,
            finished(&self.strings, code, last_line, &self.routing),
        );
    }

    /// The daemon stops for the handover: says the newest stage written so
    /// far once the gap allows, and returns when the lane has queued it.
    async fn stopping(mut self, feed: &mut UpdateFeed) {
        (feed.catch_up)();
        while let Ok(event) = feed.events.try_recv() {
            match event {
                UpdateEvent::Progress(stage) => self.waiting = Some(stage),
                UpdateEvent::Finished { code, last_line } => {
                    let lane = self.lane.clone();
                    self.finish(code, &last_line).await;
                    return handled(&lane).await;
                }
            }
        }
        if self.waiting.is_some() {
            tokio::time::sleep_until(self.due()).await;
            self.say_waiting();
        }
        handled(&self.lane).await;
    }
}

async fn run(mut relay: Relay, mut feed: UpdateFeed, stop: CancellationToken) {
    loop {
        let event = tokio::select! {
            () = stop.cancelled() => return relay.stopping(&mut feed).await,
            event = feed.events.recv() => event,
            () = tokio::time::sleep_until(relay.due()), if relay.waiting.is_some() => {
                relay.say_waiting();
                continue;
            }
        };
        match event {
            Some(UpdateEvent::Progress(stage)) => relay.progress(stage),
            Some(UpdateEvent::Finished { code, last_line }) => {
                return relay.finish(code, &last_line).await;
            }
            None => return,
        }
    }
}

/// Returns once the lane has handled every message sent before.
async fn handled(lane: &mpsc::WeakUnboundedSender<Msg>) {
    let (tx, rx) = oneshot::channel();
    say(lane, Msg::Barrier(tx));
    let _ = rx.await;
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
    use tokio::time::Instant;
    use tokio_util::sync::CancellationToken;

    use super::{Msg, Relay, UpdateEvent, UpdateFeed, run};
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

    fn relay(lane: &mpsc::UnboundedSender<Msg>) -> Relay {
        Relay::new(
            lane.downgrade(),
            routing(),
            pmt_i18n::Strings::english(pmt_i18n::Surface::Core),
        )
    }

    /// A feed whose updater has written nothing it has not reported yet.
    fn feed(events: mpsc::UnboundedReceiver<UpdateEvent>) -> UpdateFeed {
        UpdateFeed {
            events,
            catch_up: Box::new(|| {}),
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
        let task =
            pmt_core::spawn::spawn(run(relay(&lane), feed(events), CancellationToken::new()));
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

    /// The handover stops the daemon right after the updater wrote
    /// `restarting`: that stage, waiting for its gap or not yet read from
    /// the updater's log, is still said (2 s after the one before) and
    /// queued by the lane before the relay lets the daemon go.
    #[tokio::test(start_paused = true)]
    async fn the_last_stage_is_said_before_the_daemon_stops() {
        let (lane, mut chat) = mpsc::unbounded_channel();
        let (tx, events) = mpsc::unbounded_channel();
        let unread = tx.clone();
        let feed = UpdateFeed {
            events,
            catch_up: Box::new(move || {
                let _ = unread.send(UpdateEvent::Progress("restarting the daemon".to_owned()));
            }),
        };
        let stop = CancellationToken::new();
        let started = Instant::now();
        let task = pmt_core::spawn::spawn(run(relay(&lane), feed, stop.clone()));
        tx.send(UpdateEvent::Progress("self-test".to_owned()))
            .unwrap();
        tx.send(UpdateEvent::Progress("swapping the binary".to_owned()))
            .unwrap();
        assert_eq!(said(&chat.recv().await.unwrap()), Some("self-test"));
        tokio::time::sleep(Duration::from_millis(300)).await;
        stop.cancel();
        assert_eq!(
            said(&chat.recv().await.unwrap()),
            Some("restarting the daemon")
        );
        assert!(started.elapsed() >= super::MIN_GAP, "the gap still holds");
        assert!(!task.is_finished(), "it waits until the lane took the line");
        let Some(Msg::Barrier(ack)) = chat.recv().await else {
            panic!("expected the barrier");
        };
        ack.send(()).unwrap();
        task.await.unwrap();
        assert!(chat.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn a_stopping_daemon_without_a_lane_is_not_held() {
        let (lane, chat) = mpsc::unbounded_channel();
        let (tx, events) = mpsc::unbounded_channel();
        let relay = relay(&lane);
        drop((lane, chat));
        let stop = CancellationToken::new();
        stop.cancel();
        tx.send(UpdateEvent::Progress("restarting the daemon".to_owned()))
            .unwrap();
        run(relay, feed(events), stop).await;
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
        run(relay(&lane), feed(events), CancellationToken::new()).await;
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
