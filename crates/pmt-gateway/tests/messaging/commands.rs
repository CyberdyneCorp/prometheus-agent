//! Gateway commands (spec: messaging-gateway, Gateway slash commands,
//! quick commands and hooks; Slash command access tiers; Gateway
//! lifecycle hooks are typed; Single-flight session routing).

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use pmt_core::error::Error;
use pmt_gateway::messaging::{ChatType, Hook, Hooks, Inbound};
use pmt_store::gateway::routes::ResetReason;
use serde_json::{Value, json};

use crate::support::{Harness, Setup, config, eventually, open, said};

/// A hook that records its calls and answers a fixed value.
struct Recorder {
    calls: Mutex<Vec<(String, Value)>>,
    answer: Value,
}

impl Recorder {
    fn new(answer: Value) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::default(),
            answer,
        })
    }

    fn names(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(n, _)| n.clone())
            .collect()
    }
}

impl Hook for Recorder {
    fn call(
        &self,
        name: &str,
        payload: &Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, Error>> + Send + '_>> {
        self.calls
            .lock()
            .unwrap()
            .push((name.to_owned(), payload.clone()));
        let answer = self.answer.clone();
        Box::pin(async move { Ok(answer) })
    }
}

#[tokio::test]
async fn a_hook_denying_a_command_replies_and_skips_the_handler() {
    let hooks = Arc::new(Hooks::default());
    let deny = Recorder::new(json!({"action": "deny", "message": "not here"}));
    hooks
        .register("default", "pre_command", deny.clone())
        .unwrap();
    let h = Harness::start(Setup {
        hooks,
        ..Setup::new(&open("telegram"))
    })
    .await;
    h.dm("telegram", "1", "/model").await;
    said(&h.adapter("telegram"), "not here").await;
    assert!(h.agents.commands.lock().unwrap().is_empty());
    assert_eq!(deny.calls.lock().unwrap()[0].1["command"], "model");
}

#[tokio::test]
async fn a_rewriting_hook_turns_the_command_into_input() {
    let hooks = Arc::new(Hooks::default());
    let rewrite = Recorder::new(json!({"action": "rewrite", "text": "summarise the day"}));
    hooks.register("default", "pre_command", rewrite).unwrap();
    let h = Harness::start(Setup {
        hooks,
        ..Setup::new(&open("telegram"))
    })
    .await;
    h.dm("telegram", "1", "/usage").await;
    said(&h.adapter("telegram"), "echo: summarise the day").await;
}

#[tokio::test]
async fn a_non_admin_keeps_only_the_user_tier() {
    let table = "allow_all = true\nadmins = [\"111\"]";
    let h = Harness::start(Setup::new(&config("telegram", table))).await;
    h.dm("telegram", "222", "/model").await;
    said(
        &h.adapter("telegram"),
        "This command is restricted to admins.",
    )
    .await;
    h.dm("telegram", "222", "/help").await;
    said(&h.adapter("telegram"), "ran /help").await;
    h.dm("telegram", "111", "/model").await;
    said(&h.adapter("telegram"), "model switched").await;
    let lines: Vec<String> = h
        .agents
        .commands
        .lock()
        .unwrap()
        .iter()
        .map(|(_, l)| l.clone())
        .collect();
    assert_eq!(lines, vec!["/help", "/model"]);
}

#[tokio::test]
async fn a_command_not_on_this_surface_says_so() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    h.dm("telegram", "1", "/theme dark").await;
    said(&h.adapter("telegram"), "/theme is not available here").await;
}

#[test]
fn a_misspelled_hook_name_is_refused_by_name() {
    let hooks = Hooks::default();
    let err = hooks
        .register("default", "on_sesion_start", Recorder::new(json!({})))
        .unwrap_err();
    assert!(
        err.message().contains("unknown hook 'on_sesion_start'"),
        "{}",
        err.message()
    );
}

#[tokio::test]
async fn a_hook_of_another_profile_never_runs() {
    let hooks = Arc::new(Hooks::default());
    let work = Recorder::new(json!({}));
    hooks
        .register("work", "on_session_start", work.clone())
        .unwrap();
    let mine = Recorder::new(json!({}));
    hooks
        .register("default", "on_session_start", mine.clone())
        .unwrap();
    let h = Harness::start(Setup {
        hooks,
        profiles: vec![("default", open("telegram")), ("work", open("telegram"))],
        ..Setup::new("")
    })
    .await;
    h.dm("telegram", "1", "hello").await;
    said(&h.adapter("telegram"), "echo: hello").await;
    assert_eq!(mine.names(), vec!["on_session_start"]);
    assert!(work.names().is_empty());
}

#[tokio::test]
async fn lifecycle_hooks_fire_around_a_turn() {
    let hooks = Arc::new(Hooks::default());
    let recorder = Recorder::new(json!({}));
    for name in [
        "pre_gateway_dispatch",
        "post_gateway_admission",
        "on_session_start",
        "on_turn_start",
        "on_turn_end",
        "post_gateway_delivery",
    ] {
        hooks.register("default", name, recorder.clone()).unwrap();
    }
    let h = Harness::start(Setup {
        hooks,
        ..Setup::new(&open("telegram"))
    })
    .await;
    h.dm("telegram", "1", "hello").await;
    said(&h.adapter("telegram"), "echo: hello").await;
    eventually("the delivery hook ran", || {
        recorder
            .names()
            .contains(&"post_gateway_delivery".to_owned())
    })
    .await;
    let names = recorder.names();
    for expected in [
        "pre_gateway_dispatch",
        "post_gateway_admission",
        "on_session_start",
        "on_turn_start",
        "on_turn_end",
    ] {
        assert!(
            names.contains(&expected.to_owned()),
            "{expected} in {names:?}"
        );
    }
    let payload = &recorder.calls.lock().unwrap()[0].1;
    assert_eq!(payload["schema_version"], "prometheus.hooks.v1");
    assert!(
        !payload.to_string().contains("hello"),
        "dispatch hooks carry no message text"
    );
}

#[tokio::test]
async fn new_replaces_the_session_and_records_why() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    h.dm("telegram", "1", "first").await;
    said(&h.adapter("telegram"), "echo: first").await;
    let key = "default:telegram:dm:1";
    let old = h.store().route(key).unwrap().unwrap().session_id;
    h.dm("telegram", "1", "/reset").await;
    said(&h.adapter("telegram"), "New session started.").await;
    let new = h.store().route(key).unwrap().unwrap().session_id;
    assert_ne!(old, new);
    let state = h.store().routed_session(new).unwrap().unwrap();
    assert_eq!(state.reset_reason, Some(ResetReason::Reset));
    assert_eq!(h.store().session(new).unwrap().parent_session_id, Some(old));
    assert!(
        h.store().session(old).unwrap().ended_at.is_some(),
        "the replaced session ends"
    );
}

#[tokio::test]
async fn concurrent_new_commands_create_one_session() {
    let h = Harness::start(Setup::new(&open("telegram"))).await;
    h.dm("telegram", "1", "first").await;
    said(&h.adapter("telegram"), "echo: first").await;
    for _ in 0..3 {
        h.send("telegram", Inbound::text(ChatType::Dm, "1", "1", "/new"))
            .await;
    }
    eventually("three replies", || {
        h.adapter("telegram")
            .texts()
            .iter()
            .filter(|t| *t == "New session started.")
            .count()
            == 3
    })
    .await;
    assert_eq!(h.sessions().len(), 2, "one session from the burst of /new");
}

#[tokio::test]
async fn reset_ends_the_replaced_session_for_its_own_profile_hooks() {
    let hooks = Arc::new(Hooks::default());
    let mine = Recorder::new(json!({}));
    hooks
        .register("default", "on_session_end", mine.clone())
        .unwrap();
    let work = Recorder::new(json!({}));
    hooks
        .register("work", "on_session_end", work.clone())
        .unwrap();
    let h = Harness::start(Setup {
        hooks,
        profiles: vec![("default", open("telegram")), ("work", open("telegram"))],
        ..Setup::new("")
    })
    .await;
    h.dm("telegram", "1", "first").await;
    said(&h.adapter("telegram"), "echo: first").await;
    assert!(mine.names().is_empty(), "no session ended yet");
    let key = "default:telegram:dm:1";
    let old = h.store().route(key).unwrap().unwrap().session_id;
    h.dm("telegram", "1", "/reset").await;
    said(&h.adapter("telegram"), "New session started.").await;
    let new = h.store().route(key).unwrap().unwrap().session_id;
    let calls = mine.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "{calls:?}");
    let (name, payload) = &calls[0];
    assert_eq!(name, "on_session_end");
    assert_eq!(payload["session_id"], old.to_string());
    assert_eq!(payload["replaced_by"], new.to_string());
    assert_eq!(payload["reset_reason"], "reset");
    assert_eq!(payload["schema_version"], "prometheus.hooks.v1");
    assert!(work.names().is_empty(), "another profile's hook never runs");
}
