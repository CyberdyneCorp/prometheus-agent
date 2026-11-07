//! The memory revision journal, the session summary and semantic recall
//! through the binary (spec: memory, Memory revision journal; Reviewable
//! memory changes per session; Semantic recall).

mod common;

use std::path::Path;

use common::{mock_home, ok, run, stderr};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const SCRIPT: &str = r#"
[[rule]]
step = 0
[[rule.tool_calls]]
name = "memory"
arguments = { target = "user", operations = [{ action = "add", content = "Lives in Lisbon." }, { action = "add", content = "Works in UTC." }] }

[[rule]]
step = 1
[[rule.tool_calls]]
name = "memory"
arguments = { target = "user", operations = [{ action = "patch", old_text = "Lisbon", content = "Lives in Porto." }] }

[[rule]]
step = 2
text = "noted"
"#;

fn memory_json(home: &Path, args: &[&str]) -> Value {
    let mut full = vec!["--json", "memory"];
    full.extend_from_slice(args);
    serde_json::from_str(&ok(&run(home, &full))).unwrap()
}

#[test]
fn an_agent_mistake_is_summarized_at_session_end_and_reverted_by_the_user() {
    // Scenarios: Session end shows a summary; Revert an agent mistake.
    let home = mock_home();
    let script = home.path().join("mock.toml");
    std::fs::write(&script, SCRIPT).unwrap();
    let out = run(
        home.path(),
        &[
            "chat",
            "--mock-script",
            script.to_str().unwrap(),
            "-z",
            "remember where I live",
        ],
    );
    let err = stderr(&out);
    assert!(out.status.success(), "{err}");
    assert!(
        err.contains("Memory: +2 ~1 -0 (review with /memory review)"),
        "{err}"
    );

    let log = memory_json(home.path(), &["log", "--origin", "agent"]);
    let mistake = log.as_array().unwrap().last().unwrap().clone();
    assert_eq!(mistake["operation"], "patch");
    assert_eq!(mistake["after"], "Lives in Porto.");
    let rev = mistake["rev"].as_i64().unwrap();
    let session = mistake["session_id"].as_str().unwrap().to_owned();
    let diff = memory_json(home.path(), &["diff", "--session", &session]);
    assert!(
        diff["diff"].as_str().unwrap().contains("+Lives in Porto."),
        "{diff}"
    );

    let reverted = memory_json(home.path(), &["revert", &rev.to_string()]);
    assert_eq!(reverted["rev"], rev + 1);

    let entries = memory_json(home.path(), &["list"]);
    let contents: Vec<&str> = entries
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["content"].as_str().unwrap())
        .collect();
    assert!(
        contents.contains(&"Lives in Lisbon.") && !contents.contains(&"Lives in Porto."),
        "{contents:?}"
    );
    let newest = memory_json(home.path(), &["log", "--since", &rev.to_string()]);
    assert_eq!(newest[0]["rev"], rev + 1);
    assert_eq!(newest[0]["origin"], "user");
}

/// Embeds by concept (children / databases), so a paraphrase sharing no
/// word with an entry still lands next to it.
struct Concepts;

impl Respond for Concepts {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        let data: Vec<Value> = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(index, text)| {
                let t = text.as_str().unwrap().to_lowercase();
                let kid = ["kid", "daughter"].iter().any(|w| t.contains(w));
                let db = ["postgres", "database"].iter().any(|w| t.contains(w));
                json!({"index": index, "embedding": [u8::from(kid), u8::from(db), 0.1]})
            })
            .collect();
        ResponseTemplate::new(200).set_body_json(json!({"data": data}))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn memory_search_ranks_a_paraphrase_with_the_configured_embedding_model() {
    // Scenario: Paraphrase is recalled with embeddings (ranking path).
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(Concepts)
        .mount(&server)
        .await;
    let home = mock_home();
    common::append_config(
        home.path(),
        &format!(
            "\n[providers.emb]\nbase_url = \"{}/v1\"\nwire_format = \"chat_completions\"\nauth = \"none\"\n\n\
             [memory.recall]\nembedding_model = \"emb/e1\"\n",
            server.uri()
        ),
    );
    let h = home.path().to_owned();
    let (ana, explained) = tokio::task::spawn_blocking(move || {
        let add = |content: &str| {
            memory_json(
                &h,
                &["add", "--target", "user", "--tier", "archival", content],
            )["id"]
                .as_str()
                .unwrap()
                .to_owned()
        };
        let ana = add("The user's daughter is named Ana.");
        add("Project Atlas uses Postgres 16.");
        let explained = memory_json(&h, &["search", "what's my kid called?", "--explain"]);
        (ana, explained)
    })
    .await
    .unwrap();
    let hits = explained.as_array().unwrap();
    assert_eq!(hits.len(), 1, "{explained}");
    assert_eq!(hits[0]["id"], ana.as_str());
    assert_eq!(hits[0]["bm25"], 0.0, "no query word appears in the entry");
    assert!(hits[0]["cosine"].as_f64().unwrap() > 0.99, "{explained}");
    assert!(!server.received_requests().await.unwrap().is_empty());
}
