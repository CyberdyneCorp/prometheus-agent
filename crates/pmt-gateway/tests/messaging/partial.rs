//! An adapter serving some of its accounts and not others (spec:
//! platform-adapters, Slack multiple workspaces): its `degraded` state
//! with `partial_outage` survives sends through the accounts it serves.

use pmt_gateway::messaging::adapter::PARTIAL_OUTAGE;
use pmt_gateway::messaging::{AdapterError, AdapterState};

use crate::support::{Harness, Setup, open, said};

#[tokio::test]
async fn a_partial_outage_is_not_cleared_by_a_successful_send() {
    let h = Harness::start(Setup::new(&open("slack"))).await;
    let link = h.adapter("slack").link.lock().unwrap().clone().unwrap();
    link.report(
        AdapterState::Degraded,
        Some(AdapterError {
            retryable: true,
            code: PARTIAL_OUTAGE.to_owned(),
            message: "Slack workspaces not served: team T0GONE".to_owned(),
        }),
    );
    h.dm("slack", "1", "hello").await;
    said(&h.adapter("slack"), "echo: hello").await;
    // The outbox confirms the adapter right after the send.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let status = h.gateway.statuses().remove(0);
    assert_eq!(status.state, AdapterState::Degraded);
    assert_eq!(status.code.as_deref(), Some(PARTIAL_OUTAGE));
    assert!(status.message.unwrap().contains("T0GONE"));
}
