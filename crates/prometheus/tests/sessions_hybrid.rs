//! Hybrid semantic + full-text session search through the binary (spec:
//! session-storage, Hybrid semantic and full-text search), against a local
//! embeddings server whose vectors encode a few concepts.

mod common;

use std::path::Path;

use common::{append_config, mock_home, ok, run, stderr};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// One dimension per concept: databases, children, anything else.
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
                let db = ["database", "db ", "postgres"]
                    .iter()
                    .any(|w| t.contains(w));
                let kid = ["kid", "daughter"].iter().any(|w| t.contains(w));
                json!({"index": index, "embedding": [u8::from(db), u8::from(kid), 0.1]})
            })
            .collect();
        ResponseTemplate::new(200).set_body_json(json!({"data": data}))
    }
}

fn configure(home: &Path, base_url: &str) {
    append_config(
        home,
        &format!(
            "\n[providers.emb]\nbase_url = \"{base_url}/v1\"\nwire_format = \"chat_completions\"\nauth = \"none\"\n\n\
             [sessions.search]\nsemantic = true\nembedding_model = \"emb/e1\"\n"
        ),
    );
}

fn say(home: &Path, text: &str) {
    ok(&run(home, &["chat", "-z", text]));
}

fn search(home: &Path, query: &str, mode: &str) -> Value {
    let out = run(
        home,
        &["--json", "sessions", "search", query, "--mode", mode],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap()
}

fn snippets(found: &Value) -> Vec<String> {
    found["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["snippet"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_paraphrase_is_found_in_hybrid_mode() {
    // Scenario: Paraphrase found.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/embeddings"))
        .respond_with(Concepts)
        .mount(&server)
        .await;
    let home = mock_home();
    configure(home.path(), &server.uri());
    let h = home.path().to_owned();
    let (hybrid, fts) = pmt_core::spawn::blocking(move || {
        say(&h, "We moved the database to Postgres 16");
        say(&h, "My daughter starts school on Monday");
        say(&h, "Remind me to water the plants");
        let query = "which DB engine did we migrate to";
        (search(&h, query, "hybrid"), search(&h, query, "fts"))
    })
    .await
    .unwrap();
    assert!(hybrid.get("degraded").is_none(), "{hybrid}");
    let top: Vec<String> = snippets(&hybrid).into_iter().take(5).collect();
    assert!(top.iter().any(|s| s.contains("Postgres 16")), "{hybrid}");
    // Full text alone shares no word with that message.
    assert!(
        !snippets(&fts).iter().any(|s| s.contains("Postgres 16")),
        "{fts}"
    );
    assert!(!server.received_requests().await.unwrap().is_empty());
}

#[test]
fn an_offline_embedding_model_degrades_to_full_text() {
    // Scenario: Embedding model offline.
    let home = mock_home();
    // Nothing listens on the discard port.
    configure(home.path(), "http://127.0.0.1:9");
    say(home.path(), "We moved the database to Postgres 16");
    let found = search(home.path(), "Postgres", "hybrid");
    assert_eq!(found["degraded"], "semantic_unavailable", "{found}");
    assert!(
        snippets(&found).iter().any(|s| s.contains("Postgres")),
        "{found}"
    );
    // The default mode is hybrid once semantic search is on.
    let auto = search(home.path(), "Postgres", "auto");
    assert_eq!(auto["degraded"], "semantic_unavailable", "{auto}");
}
