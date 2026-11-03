//! Native platform streaming, task cards and the working status line
//! (spec: messaging-gateway, Native platform streaming and task cards).

use pmt_gateway::messaging::{Descriptor, Interactive};

use crate::support::{Did, Harness, Setup, eventually, open};

const ANSWER: &str = "hello world, this is a streamed answer";

fn native(cards: bool, status: bool) -> Setup {
    let mut setup = Setup::new(&format!(
        "{}\n[gateway.streaming]\nenabled = true\nbuffer_threshold = 1\nedit_interval_ms = 100\n",
        open("slack")
    ));
    setup.descriptors.push(Descriptor {
        supports_edit: true,
        native_stream: true,
        task_cards: cards,
        status_line: status,
        interactive: Interactive::Native,
        ..Descriptor::new("slack", 4096)
    });
    setup
}

/// The last text message `id` shows (sent or edited).
fn shown(h: &Harness, id: &str) -> Option<String> {
    h.adapter("slack")
        .all()
        .iter()
        .filter_map(|d| match d {
            Did::Edit(edited, m) if edited == id => Some(m.text.clone()),
            _ => None,
        })
        .next_back()
}

#[tokio::test]
async fn an_answer_streams_natively_and_ends_in_the_same_message() {
    let h = Harness::start(native(false, false)).await;
    h.dm("slack", "u1", "stream").await;
    eventually("the final answer edits the stream", || {
        shown(&h, "s1").is_some_and(|t| t.contains(ANSWER))
    })
    .await;
    let ops = h.adapter("slack").streamed.lock().unwrap().clone();
    assert_eq!(ops.first().map(String::as_str), Some("start s1"));
    assert!(ops.iter().any(|o| o == "append hello "), "{ops:?}");
    assert!(ops.iter().any(|o| o == "stop s1"), "{ops:?}");
    assert!(
        !h.adapter("slack")
            .texts()
            .iter()
            .any(|t| t.contains(ANSWER)),
        "no second message"
    );
}

/// Spec scenario: Native stream fallback.
#[tokio::test]
async fn a_stream_failing_mid_turn_goes_on_by_editing_the_same_message() {
    let h = Harness::start(native(false, false)).await;
    *h.adapter("slack").appends_before_failure.lock().unwrap() = Some(1);
    h.dm("slack", "u1", "stream").await;
    eventually("the final answer", || {
        shown(&h, "s1").is_some_and(|t| t.contains(ANSWER))
    })
    .await;
    let ops = h.adapter("slack").streamed.lock().unwrap().clone();
    assert_eq!(
        ops.iter().filter(|o| o.starts_with("append")).count(),
        1,
        "{ops:?}"
    );
    let all = h.adapter("slack").all();
    let edits: Vec<&str> = all
        .iter()
        .filter_map(|d| match d {
            Did::Edit(id, m) if id == "s1" => Some(m.text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        edits.first().is_some_and(|t| t.starts_with("hello wor")),
        "the fallback shows all the text so far: {edits:?}"
    );
    let everywhere = all
        .iter()
        .filter(|d| matches!(d, Did::Send(m) if m.text.contains(ANSWER)))
        .count();
    assert_eq!(everywhere, 0, "the final text appears exactly once, in s1");
}

#[tokio::test]
async fn tool_calls_are_task_cards_and_the_status_line_shows_while_working() {
    let h = Harness::start(native(true, true)).await;
    h.dm("slack", "u1", "tools:2").await;
    eventually("the answer ends the stream", || {
        h.adapter("slack")
            .all()
            .iter()
            .any(|d| matches!(d, Did::Edit(id, m) if id.starts_with('s') && m.text == "tools done"))
    })
    .await;
    eventually("the status is cleared", || {
        h.adapter("slack")
            .streamed
            .lock()
            .unwrap()
            .iter()
            .any(|o| o == "status none")
    })
    .await;
    let ops = h.adapter("slack").streamed.lock().unwrap().clone();
    assert_eq!(ops[0], "status is thinking…");
    for card in [
        "task c0 Running",
        "task c0 Done",
        "task c1 Running",
        "task c1 Done",
    ] {
        assert!(ops.iter().any(|o| o == card), "{card}: {ops:?}");
    }
    assert!(
        !h.adapter("slack")
            .texts()
            .iter()
            .any(|t| t.contains("\u{2699} terminal")),
        "no progress bubbles"
    );
}
