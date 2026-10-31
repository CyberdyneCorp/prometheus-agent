//! The agent-level tools as the turn loop runs them (spec: agent-turn-loop,
//! todo tool contract, clarify tool contract; session-storage, Session
//! search tool).

use std::sync::Arc;

use pmt_agent::tools::clarify::{Answer, AnswerStatus, ClarifyAnswer};
use pmt_agent::tools::session_search::{Summarizer, SummaryFuture};
use pmt_agent::tools::{AgentTools, Clarifier, SessionSearch};
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::id::SessionId;
use pmt_store::{NewMessage, NewSession, Role, Source, Store};
use pmt_tools::{Status, ToolResult};
use serde_json::{Value, json};

struct TestHome {
    _root: tempfile::TempDir,
    home: Home,
}

impl TestHome {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = Home::open(&root.path().join("home")).unwrap();
        Self { _root: root, home }
    }

    fn store(&self, profile: &str) -> Arc<Store> {
        let dir = self
            .home
            .profile(&ProfileName::parse(profile).unwrap())
            .unwrap();
        let scope = ProfileScope::open(dir, &Overrides::parse::<&str>(&[]).unwrap(), Launch::Other)
            .unwrap();
        Arc::new(Store::open(&scope).unwrap())
    }
}

fn session(store: &Store) -> SessionId {
    store
        .create_session(&NewSession::new(Source::Cli))
        .unwrap()
        .id
}

fn say(store: &Store, session: SessionId, role: Role, text: &str) -> String {
    let message = NewMessage::new(role, text);
    let id = message.id.to_string();
    store.append_message(session, &message).unwrap();
    id
}

fn tools(store: &Arc<Store>, session: SessionId, clarifier: Clarifier) -> AgentTools {
    let search = SessionSearch::new(Arc::clone(store), session);
    AgentTools::new(Arc::clone(store), session, clarifier, search).unwrap()
}

fn body(result: &ToolResult) -> Value {
    serde_json::from_str(&result.text_content())
        .unwrap_or_else(|_| panic!("not JSON: {}", result.text_content()))
}

async fn call(tools: &AgentTools, name: &str, arguments: Value) -> ToolResult {
    tools
        .call(name, arguments)
        .await
        .expect("an agent-level tool")
}

#[tokio::test]
async fn todo_merge_updates_status_only() {
    let home = TestHome::new();
    let store = home.store("default");
    let s = session(&store);
    let agent = tools(&store, s, Clarifier::unattended());
    let first = body(
        &call(
            &agent,
            "todo",
            json!({"todos": [{"id": "a", "content": "write tests", "status": "pending"}]}),
        )
        .await,
    );
    let revision = first["revision"].as_u64().unwrap();
    let merged = body(
        &call(
            &agent,
            "todo",
            json!({"merge": true, "todos": [{"id": "a", "status": "completed"}]}),
        )
        .await,
    );
    assert_eq!(
        merged["todos"],
        json!([{"id": "a", "content": "write tests", "status": "completed"}])
    );
    assert_eq!(merged["revision"].as_u64().unwrap(), revision + 1);
}

#[tokio::test]
async fn todo_lists_replaces_appends_and_persists_with_the_session() {
    let home = TestHome::new();
    let store = home.store("default");
    let s = session(&store);
    let agent = tools(&store, s, Clarifier::unattended());
    let read = body(&call(&agent, "todo", json!({})).await);
    assert_eq!(read, json!({"revision": 0, "todos": []}));
    call(
        &agent,
        "todo",
        json!({"todos": [
            {"id": "p", "content": "ship", "status": "completed"},
            {"id": "c", "content": "write docs", "status": "in_progress", "parent": "p"},
            {"id": "d", "content": "old idea", "status": "cancelled"}
        ]}),
    )
    .await;
    let appended = body(
        &call(
            &agent,
            "todo",
            json!({"merge": true, "todos": [{"id": "e", "content": "release"}]}),
        )
        .await,
    );
    assert_eq!(
        appended["todos"][3],
        json!({"id": "e", "content": "release", "status": "pending"})
    );
    // Reading never bumps the revision.
    let read = body(&call(&agent, "todo", json!({})).await);
    assert_eq!(read["revision"], 2);

    // A new process of the session sees the same list.
    let again = tools(&store, s, Clarifier::unattended());
    assert_eq!(body(&call(&again, "todo", json!({})).await), read);
    // After compaction only open items and parents of open subtasks show.
    let text = again.todo_reinjection().unwrap();
    assert!(
        text.contains("write docs") && text.contains("release"),
        "{text}"
    );
    assert!(text.contains("ship"), "parent of an open subtask: {text}");
    assert!(!text.contains("old idea"), "{text}");
}

#[tokio::test]
async fn todo_caps_are_enforced_and_leave_the_list_unchanged() {
    let home = TestHome::new();
    let store = home.store("default");
    let agent = tools(&store, session(&store), Clarifier::unattended());
    let many: Vec<Value> = (0..257)
        .map(|n| json!({"id": n.to_string(), "content": "x"}))
        .collect();
    let refused = call(&agent, "todo", json!({"todos": many})).await;
    assert_eq!(refused.status, Status::Error);
    assert_eq!(refused.text_content(), "todos supports at most 256 items.");
    let long = call(
        &agent,
        "todo",
        json!({"todos": [{"id": "a", "content": "y".repeat(4_001)}]}),
    )
    .await;
    assert_eq!(long.status, Status::Error);
    assert_eq!(body(&call(&agent, "todo", json!({})).await)["revision"], 0);
}

fn questions(n: usize) -> Value {
    let list: Vec<Value> = (0..n)
        .map(|i| json!({"question": format!("Question {i}?")}))
        .collect();
    json!({"questions": list})
}

#[tokio::test]
async fn clarify_refuses_six_questions_and_asks_nothing() {
    let home = TestHome::new();
    let store = home.store("default");
    let (clarifier, mut surface) = Clarifier::attached(60);
    let agent = tools(&store, session(&store), clarifier);
    let result = call(&agent, "clarify", questions(6)).await;
    assert_eq!(result.status, Status::Error);
    assert_eq!(result.text_content(), "questions supports at most 5 items.");
    assert!(surface.try_next().is_none());
}

#[tokio::test(start_paused = true)]
async fn clarify_times_out_after_the_configured_wait() {
    let home = TestHome::new();
    let store = home.store("default");
    let (clarifier, mut surface) = Clarifier::attached(60);
    let agent = tools(&store, session(&store), clarifier);
    let asking = call(&agent, "clarify", questions(1));
    let watching = async {
        let prompt = surface.next().await.unwrap();
        // Keep the prompt open (unanswered) until the asker gives up.
        tokio::time::sleep(std::time::Duration::from_mins(2)).await;
        drop(prompt);
    };
    let (result, ()) = tokio::join!(asking, watching);
    assert!(
        result
            .text_content()
            .contains("[user did not respond within 1m]"),
        "{}",
        result.text_content()
    );
}

#[tokio::test]
async fn clarify_sends_one_request_with_every_question_and_returns_the_answers() {
    let home = TestHome::new();
    let store = home.store("default");
    let (clarifier, mut surface) = Clarifier::attached(60);
    let agent = tools(&store, session(&store), clarifier);
    let answering = async {
        let prompt = surface.next().await.unwrap();
        let request = serde_json::to_value(prompt.request()).unwrap();
        assert_eq!(request["questions"].as_array().unwrap().len(), 2);
        assert_eq!(request["allow_other"], true);
        prompt.respond(ClarifyAnswer {
            answers: vec![Answer {
                status: AnswerStatus::Answered,
                choices: vec!["blue".into()],
                text: None,
            }],
        });
    };
    let args = json!({"questions": [
        {"question": "Colour?", "choices": ["blue", "red"]},
        {"question": "Size?"}
    ]});
    let (result, ()) = tokio::join!(call(&agent, "clarify", args), answering);
    let reply = body(&result);
    assert_eq!(reply["answers"][0]["status"], "answered");
    assert_eq!(reply["answers"][0]["choices"], json!(["blue"]));
    assert_eq!(reply["answers"][1]["status"], "skipped");
}

#[tokio::test]
async fn clarify_in_an_unattended_session_tells_the_model_to_proceed() {
    let home = TestHome::new();
    let store = home.store("default");
    let agent = tools(&store, session(&store), Clarifier::unattended());
    let result = call(&agent, "clarify", questions(1)).await;
    assert_eq!(result.status, Status::Error);
    assert!(
        result.text_content().contains("best judgment"),
        "{}",
        result.text_content()
    );
}

#[tokio::test]
async fn session_search_returns_the_window_around_a_hit() {
    let home = TestHome::new();
    let store = home.store("default");
    let old = session(&store);
    let ids: Vec<String> = (0..9)
        .map(|n| {
            let text = if n == 4 {
                "the deploy key rotation".to_owned()
            } else {
                format!("message {n}")
            };
            say(
                &store,
                old,
                if n % 2 == 0 {
                    Role::User
                } else {
                    Role::Assistant
                },
                &text,
            )
        })
        .collect();
    let agent = tools(&store, session(&store), Clarifier::unattended());
    let found = body(&call(&agent, "session_search", json!({"query": "rotation"})).await);
    let hit = &found["sessions"][0]["hits"][0];
    assert_eq!(hit["message_id"], ids[4]);
    let window = body(
        &call(
            &agent,
            "session_search",
            json!({"session_id": old.to_string(), "around_message_id": ids[4], "window": 2}),
        )
        .await,
    );
    let shown: Vec<&str> = window["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["message_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        shown,
        ids[2..=6].iter().map(String::as_str).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn session_search_never_sees_another_profile() {
    let home = TestHome::new();
    let personal = home.store("default");
    let elsewhere = session(&personal);
    say(
        &personal,
        elsewhere,
        Role::User,
        "my dentist appointment is tuesday",
    );
    let work = home.store("work");
    let agent = tools(&work, session(&work), Clarifier::unattended());
    let found = body(&call(&agent, "session_search", json!({"query": "dentist"})).await);
    assert_eq!(found["sessions"], json!([]));
    // The other profile's session is unknown here, also in window mode.
    let window = call(
        &agent,
        "session_search",
        json!({"session_id": elsewhere.to_string(), "around_message_id": "msg_01ARZ3NDEKTSV4RRFFQ69G5FAV"}),
    )
    .await;
    assert_eq!(window.status, Status::Error);
}

#[tokio::test]
async fn session_search_leaves_out_the_calling_sessions_context_and_stays_under_16_kib() {
    let home = TestHome::new();
    let store = home.store("default");
    let me = session(&store);
    say(&store, me, Role::User, "zebra crossing in my current chat");
    for _ in 0..12 {
        let other = session(&store);
        say(
            &store,
            other,
            Role::User,
            &format!("zebra {}", "stripes ".repeat(150)),
        );
    }
    let agent = tools(&store, me, Clarifier::unattended());
    let result = call(
        &agent,
        "session_search",
        json!({"query": "zebra", "limit": 50, "detail": "full"}),
    )
    .await;
    assert!(result.text_content().len() <= 16 * 1024);
    let found = body(&result);
    let sessions = found["sessions"].as_array().unwrap();
    assert!(!sessions.is_empty());
    assert!(sessions.iter().all(|s| s["session_id"] != me.to_string()));
}

struct Fixed;

impl Summarizer for Fixed {
    fn summarize<'a>(
        &'a self,
        _: &'a str,
        _: SessionId,
        snippets: &'a [String],
    ) -> SummaryFuture<'a> {
        Box::pin(async move { Some(format!("{} hit(s)", snippets.len())) })
    }
}

#[tokio::test]
async fn adaptive_detail_summarizes_each_session_beyond_three() {
    let home = TestHome::new();
    let store = home.store("default");
    for _ in 0..4 {
        let other = session(&store);
        say(&store, other, Role::User, "a note about kites");
    }
    let me = session(&store);
    let search = SessionSearch::new(Arc::clone(&store), me).with_summarizer(Arc::new(Fixed));
    let agent = AgentTools::new(Arc::clone(&store), me, Clarifier::unattended(), search).unwrap();
    let adaptive = body(&call(&agent, "session_search", json!({"query": "kites"})).await);
    assert!(
        adaptive["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["summary"] == "1 hit(s)")
    );
    let full = body(
        &call(
            &agent,
            "session_search",
            json!({"query": "kites", "detail": "full"}),
        )
        .await,
    );
    assert!(full["sessions"][0].get("summary").is_none());
}

#[tokio::test]
async fn other_tools_are_not_agent_level() {
    let home = TestHome::new();
    let store = home.store("default");
    let agent = tools(&store, session(&store), Clarifier::unattended());
    assert!(agent.call("read_file", json!({})).await.is_none());
}
