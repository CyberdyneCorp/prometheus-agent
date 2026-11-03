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
