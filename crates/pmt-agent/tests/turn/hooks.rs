//! Plugin `pre_llm_call` context (spec: plugin-system, `pre_llm_call` context
//! preserves the prompt cache): on this turn's user message on the wire
//! only, never in the system prompt or the stored history.

use std::sync::Arc;

use pmt_agent::turn::TurnHooks;
use pmt_agent::turn::model::BoxFuture;
use pmt_providers::request::{Part, Role};
use pmt_store::Source;

use crate::support::{FakeTools, Profile, agent, mock_model, system_text};

struct Context;

impl TurnHooks for Context {
    fn pre_llm_call<'a>(&'a self, _: &'a str, user: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move { Some(format!("[hook saw: {user}]")) })
    }
}

fn user_texts(request: &pmt_providers::request::Request) -> Vec<String> {
    request
        .messages
        .iter()
        .filter(|m| m.role == Role::User)
        .filter_map(|m| match m.parts.as_slice() {
            [Part::Text { text }] => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn hook_context_rides_on_the_current_user_message_only() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, "[[rule]]\ntext = \"ok\"\n", "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |setup| {
        setup.turn_hooks = Some(Arc::new(Context));
    });
    run.turn(&profile, "first").await;
    run.turn(&profile, "second").await;
    let requests = model.requests();
    assert_eq!(user_texts(&requests[0]), vec!["first\n\n[hook saw: first]"]);
    // The previous turn's message goes out as stored, without its context.
    assert_eq!(
        user_texts(&requests[1]),
        vec!["first", "second\n\n[hook saw: second]"]
    );
    assert!(!system_text(&requests[1]).contains("hook saw"));
    let stored = profile
        .store
        .messages(s, pmt_store::Visibility::All)
        .unwrap();
    assert!(
        stored
            .iter()
            .all(|m| !m.content.as_deref().unwrap_or("").contains("hook saw"))
    );
}

/// Records observer firings; holds the stream when asked to.
#[derive(Default)]
struct Watcher {
    held: bool,
    seen: std::sync::Mutex<Vec<(String, String)>>,
}

impl TurnHooks for Watcher {
    fn pre_llm_call<'a>(&'a self, _: &'a str, _: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async { None })
    }

    fn holds_stream(&self) -> bool {
        self.held
    }

    fn transform_llm_output<'a>(
        &'a self,
        reply: pmt_agent::turn::FinalReply<'a>,
        held: bool,
    ) -> BoxFuture<'a, pmt_agent::turn::Transformed> {
        let text = reply.text.to_owned();
        Box::pin(async move {
            if held {
                pmt_agent::turn::Transformed::Replaced(format!("checked: {text}"))
            } else {
                pmt_agent::turn::Transformed::Unchanged
            }
        })
    }

    fn observe(&self, hook: &'static str, payload: serde_json::Map<String, serde_json::Value>) {
        let detail = payload
            .get("delta")
            .or_else(|| payload.get("text"))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();
        self.seen.lock().unwrap().push((hook.to_owned(), detail));
    }
}

/// The text a surface received.
#[derive(Default)]
struct Screen(std::sync::Mutex<String>);

impl pmt_agent::turn::DeltaSink for Screen {
    fn text(&self, text: &str) {
        self.0.lock().unwrap().push_str(text);
    }
}

async fn watched(held: bool) -> (Arc<Watcher>, Arc<Screen>, pmt_agent::turn::TurnResult) {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, "[[rule]]\ntext = \"streamed words\"\n", "mock").await;
    let watcher = Arc::new(Watcher {
        held,
        ..Watcher::default()
    });
    let hooks = Arc::clone(&watcher);
    let run = agent(&profile, s, model, FakeTools::new(), move |setup| {
        setup.turn_hooks = Some(hooks);
    });
    let screen = Arc::new(Screen::default());
    let mut options = crate::support::options();
    options.deltas = Arc::clone(&screen) as _;
    let result = run
        .turn_with(&profile, pmt_agent::turn::TurnInput::text("hi"), options)
        .await;
    (watcher, screen, result)
}

#[tokio::test]
async fn stream_observers_see_every_delta_and_cannot_change_them() {
    // Spec scenario "Stream observer cannot change output": observers get
    // immutable copies; the surface gets the model's tokens unchanged.
    let (watcher, screen, result) = watched(false).await;
    assert_eq!(*screen.0.lock().unwrap(), "streamed words");
    assert_eq!(result.final_response.as_deref(), Some("streamed words"));
    let seen = watcher.seen.lock().unwrap().clone();
    assert_eq!(seen.first().unwrap().0, "on_stream_start");
    let deltas: String = seen
        .iter()
        .filter(|(hook, _)| hook == "on_stream_delta")
        .map(|(_, d)| d.as_str())
        .collect();
    assert_eq!(deltas, "streamed words");
    assert_eq!(
        seen.last().unwrap(),
        &("on_stream_end".to_owned(), "streamed words".to_owned())
    );
}

#[tokio::test]
async fn a_held_turn_streams_nothing_before_the_hook_rewrote_it() {
    let (_, screen, result) = watched(true).await;
    assert_eq!(*screen.0.lock().unwrap(), "checked: streamed words");
    assert_eq!(
        result.final_response.as_deref(),
        Some("checked: streamed words")
    );
}
