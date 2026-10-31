//! The frozen system prompt stored with its session (spec: agent-turn-loop,
//! System prompt frozen per session: bytes, SHA-256 and tool order are
//! persisted at the first turn and replayed after a restart).

use jiff::Timestamp;
use pmt_store::{PromptTier, SessionPrompt};
use serde_json::json;

use crate::support::{TestHome, session};

fn prompt(text: &str) -> SessionPrompt {
    SessionPrompt {
        tiers: vec![
            PromptTier {
                tier: "static".into(),
                text: text.into(),
            },
            PromptTier {
                tier: "session".into(),
                text: "Session started.".into(),
            },
        ],
        sha256: format!("sha256:{text}"),
        tools: json!([{"name": "read_file"}, {"name": "todo"}]),
        provider: Some("mock".into()),
        model: Some("mock".into()),
        built_at: Timestamp::from_millisecond(1_700_000_000_000).unwrap(),
        reason: "new_session".into(),
    }
}

#[test]
fn the_prompt_survives_reopening_and_goes_with_the_session() {
    let home = TestHome::new();
    let s = {
        let store = home.store("default");
        let s = session(&store);
        assert_eq!(store.session_prompt(s.id).unwrap(), None);
        store
            .save_session_prompt(s.id, &prompt("You are Prometheus."))
            .unwrap();
        s
    };
    let store = home.store("default");
    let back = store.session_prompt(s.id).unwrap().unwrap();
    assert_eq!(back, prompt("You are Prometheus."));
    assert_eq!(
        store.session(s.id).unwrap().system_prompt_hash.as_deref(),
        Some("sha256:You are Prometheus.")
    );

    let mut rebuilt = prompt("You are a pirate.");
    rebuilt.reason = "model_changed".into();
    store.save_session_prompt(s.id, &rebuilt).unwrap();
    assert_eq!(store.session_prompt(s.id).unwrap().unwrap(), rebuilt);

    store.delete_session(s.id, false).unwrap();
    assert_eq!(store.session_prompt(s.id).unwrap(), None);
}
