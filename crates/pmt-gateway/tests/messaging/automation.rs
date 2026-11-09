//! Automated turns in a conversation (spec: session-automation, Heartbeat
//! ownership, recovery and silence on the gateway: a tick reserved at
//! admission is refunded when the attempt is rejected before the turn
//! starts).

use pmt_gateway::messaging::Admission;
use pmt_gateway::messaging::automation::Automated;
use tokio::sync::oneshot;

use crate::support::{Harness, Setup, config, said};

fn heartbeat(started: oneshot::Sender<bool>) -> Automated {
    Automated {
        origin: "heartbeat".to_owned(),
        text: "[heartbeat] anything new?".to_owned(),
        notice: None,
        started: Some(started),
    }
}

/// A person's message reaches the lane after `Gateway::automate` saw the
/// chat idle but before the lane takes the wakeup: the lane drops it and
/// tells the host, which gives the tick back.
#[tokio::test]
async fn a_wakeup_the_lane_drops_after_admission_is_reported_not_started() {
    let h = Harness::start(Setup::new(&config("telegram", "allow_all = true"))).await;
    h.dm("telegram", "1", "hi").await;
    said(&h.adapter("telegram"), "echo: hi").await;
    let session = h.sessions()[0].to_string();
    // On this single-threaded runtime the lane does not run between the
    // two calls: the person's input is in its mailbox, unseen, when the
    // wakeup is admitted behind it.
    h.dm("telegram", "1", "slow:300").await;
    let (tx, rx) = oneshot::channel();
    let admission = h.gateway.automate("default", &session, heartbeat(tx));
    assert_eq!(
        admission,
        Admission::Accepted,
        "admitted on a stale idle view"
    );
    assert!(!rx.await.unwrap(), "the lane reports the drop");
    said(&h.adapter("telegram"), "slow done").await;
    assert_eq!(
        h.agents.texts(),
        vec!["hi", "slow:300"],
        "no automated turn"
    );
}

/// An idle conversation starts the wakeup and says so.
#[tokio::test]
async fn a_wakeup_on_an_idle_chat_reports_its_turn_started() {
    let h = Harness::start(Setup::new(&config("telegram", "allow_all = true"))).await;
    h.dm("telegram", "1", "hi").await;
    said(&h.adapter("telegram"), "echo: hi").await;
    let session = h.sessions()[0].to_string();
    let (tx, rx) = oneshot::channel();
    let admission = h.gateway.automate("default", &session, heartbeat(tx));
    assert_eq!(admission, Admission::Accepted);
    assert!(rx.await.unwrap());
    assert_eq!(h.agents.texts(), vec!["hi", "[heartbeat] anything new?"]);
}
