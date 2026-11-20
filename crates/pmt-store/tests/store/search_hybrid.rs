//! Hybrid search ranking fixtures (spec: session-storage, Hybrid semantic
//! and full-text search): stored vectors stand in for an embedding model.

use pmt_core::id::{MessageId, SessionId};
use pmt_store::search::hybrid::{QueryVector, SEMANTIC_UNAVAILABLE};
use pmt_store::{NewMessage, Role, SearchOptions, Store};

use crate::support::{TestHome, session};

const MODEL: &str = "emb/e1";

fn say(store: &Store, session: SessionId, text: &str, vector: &[f32]) -> MessageId {
    let message = NewMessage::new(Role::User, text);
    store.append_message(session, &message).unwrap();
    store
        .put_message_embeddings(MODEL, &[(message.id, text.to_owned(), vector.to_vec())])
        .unwrap();
    message.id
}

fn ids(results: &pmt_store::SearchResults) -> Vec<MessageId> {
    results.hits.iter().map(|h| h.message_id).collect()
}

#[test]
fn fusion_ranks_agreement_first_and_finds_paraphrases() {
    let home = TestHome::new();
    let store = home.store("default");
    let s = session(&store).id;
    // Both rankings: "database" in text and the database direction.
    let both = say(&store, s, "the database migration finished", &[1.0, 0.0]);
    // Text only.
    let text = say(&store, s, "a database of recipes for dinner", &[0.0, 1.0]);
    // Meaning only: no query word.
    let meaning = say(&store, s, "we moved to Postgres 16", &[0.9, 0.1]);
    let query = QueryVector {
        model: MODEL,
        vector: &[1.0, 0.0],
    };
    let found = store
        .hybrid_search("database", &SearchOptions::default(), Some(query))
        .unwrap();
    assert_eq!(found.degraded, None);
    let order = ids(&found);
    assert_eq!(order[0], both, "agreement wins");
    assert!(
        order.contains(&text) && order.contains(&meaning),
        "{order:?}"
    );
    let full_text = store.search("database", &SearchOptions::default()).unwrap();
    assert!(!ids(&full_text).contains(&meaning));
}

#[test]
fn without_vectors_a_hybrid_search_is_full_text_marked_degraded() {
    let home = TestHome::new();
    let store = home.store("default");
    let s = session(&store).id;
    let message = NewMessage::new(Role::User, "kubernetes upgrade notes");
    store.append_message(s, &message).unwrap();
    // No query vector (the model is unreachable).
    let found = store
        .hybrid_search("kubernetes", &SearchOptions::default(), None)
        .unwrap();
    assert_eq!(found.degraded, Some(SEMANTIC_UNAVAILABLE));
    assert_eq!(ids(&found), [message.id]);
    // A query vector of a model nothing was embedded with.
    let query = QueryVector {
        model: "other/model",
        vector: &[1.0],
    };
    let found = store
        .hybrid_search("kubernetes", &SearchOptions::default(), Some(query))
        .unwrap();
    assert_eq!(found.degraded, Some(SEMANTIC_UNAVAILABLE));
    // Unembedded messages are offered to the background pass once.
    let pending = store.unembedded_messages(MODEL, 10).unwrap();
    assert_eq!(pending.len(), 1);
    store
        .put_message_embeddings(MODEL, &[(message.id, pending[0].text.clone(), vec![1.0])])
        .unwrap();
    assert!(store.unembedded_messages(MODEL, 10).unwrap().is_empty());
}
