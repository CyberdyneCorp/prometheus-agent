//! The revision journal, per-session review and semantic recall (spec:
//! memory, Memory revision journal; Reviewable memory changes per session;
//! Semantic recall).

use std::sync::Arc;

use pmt_agent::tools::MemoryPort;
use pmt_agent::turn::model::BoxFuture;
use pmt_config::schema::MemoryReviewMode;
use pmt_core::error::Error;
use pmt_memory::journal;
use pmt_memory::rank::Embedder;
use pmt_store::{MemoryOrigin, Operation, RevisionFilter};
use serde_json::json;

use crate::support::{Profile, add, call};

async fn patch(memory: &pmt_memory::MemorySession, id: &str, content: &str) {
    let (ok, out) = call(
        memory,
        json!({"target": "user", "operations": [{"action": "patch", "id": id, "content": content}]}),
    )
    .await;
    assert!(ok, "{out}");
}

#[tokio::test]
async fn reverting_an_agent_patch_restores_the_entry_as_a_new_user_revision() {
    // Scenario: Revert an agent mistake.
    let profile = Profile::new("");
    let session = profile.session();
    let memory = profile.memory(session);
    let id = add(&memory, "user", "Lives in Lisbon.").await;
    patch(&memory, &id, "Lives in Porto.").await;
    let revisions = profile
        .store
        .memory_revisions(&RevisionFilter::default())
        .unwrap();
    let mistake = revisions.last().unwrap();
    assert_eq!(mistake.operation, Operation::Patch);
    assert_eq!(mistake.origin, MemoryOrigin::Agent);
    assert_eq!(mistake.session_id, Some(session.to_string()));

    let new = journal::revert(&profile.store, mistake.rev).unwrap();

    assert_eq!(new, mistake.rev + 1);
    let entry = profile.store.memory_entry(&id).unwrap().unwrap();
    assert_eq!(entry.content, "Lives in Lisbon.");
    let revert = profile.store.memory_revision(new).unwrap().unwrap();
    assert_eq!(revert.origin, MemoryOrigin::User);
    assert_eq!(revert.after.unwrap().content, "Lives in Lisbon.");
}

#[tokio::test]
async fn reverting_an_add_removes_the_entry_and_the_diff_shows_each_change() {
    let profile = Profile::new("");
    let memory = profile.memory(profile.session());
    let id = add(&memory, "user", "Uses vim.").await;
    let rev = profile
        .store
        .memory_revisions(&RevisionFilter::default())
        .unwrap()[0]
        .rev;
    let diff = journal::diff(
        &profile
            .store
            .memory_revisions(&RevisionFilter::default())
            .unwrap(),
    );
    assert!(
        diff.contains("+++ b/") && diff.contains("+Uses vim."),
        "{diff}"
    );
    journal::revert(&profile.store, rev).unwrap();
    assert!(profile.store.memory_entry(&id).unwrap().is_none());
    assert!(journal::revert(&profile.store, 9_999).is_err());
}

#[tokio::test]
async fn a_session_that_changed_memory_gets_a_one_line_summary() {
    // Scenario: Session end shows a summary.
    let profile = Profile::new("");
    let session = profile.session();
    let memory = profile.memory(session);
    let first = add(&memory, "user", "Prefers terse replies.").await;
    add(&memory, "user", "Works in UTC.").await;
    patch(&memory, &first, "Prefers very terse replies.").await;
    let line = journal::summary(
        &profile.store,
        MemoryReviewMode::Summary,
        &session.to_string(),
    )
    .unwrap();
    assert_eq!(
        line.as_deref(),
        Some("Memory: +2 ~1 -0 (review with /memory review)")
    );
    assert_eq!(
        journal::summary(&profile.store, MemoryReviewMode::Off, &session.to_string()).unwrap(),
        None
    );
    let other = profile.session();
    assert_eq!(
        journal::summary(
            &profile.store,
            MemoryReviewMode::Summary,
            &other.to_string()
        )
        .unwrap(),
        None
    );
}

#[tokio::test]
async fn memory_review_lists_diffs_and_keeps_or_reverts_each_change() {
    let profile = Profile::new("");
    let session = profile.session();
    let memory = profile.memory(session);
    let id = add(&memory, "user", "Lives in Lisbon.").await;
    patch(&memory, &id, "Lives in Porto.").await;
    let config = &profile.scope.config().memory;
    let sid = session.to_string();
    let run = |args: &str| pmt_memory::slash::run(&profile.store, config, Some(&sid), args);

    let listing = run("review").unwrap().text;
    assert!(
        listing.contains("-Lives in Lisbon.") && listing.contains("+Lives in Porto."),
        "{listing}"
    );
    assert!(listing.contains("/memory review revert"), "{listing}");
    let changes = journal::session_changes(&profile.store, &sid).unwrap();
    let (added, patched) = (changes[0].rev, changes[1].rev);

    run(&format!("review keep {added}")).unwrap();
    run(&format!("review revert {patched}")).unwrap();

    assert_eq!(
        profile.store.memory_entry(&id).unwrap().unwrap().content,
        "Lives in Lisbon."
    );
    assert!(run("review").unwrap().text.contains("No memory changes"));
    assert_eq!(
        journal::summary(&profile.store, MemoryReviewMode::Summary, &sid).unwrap(),
        None,
        "reviewed changes are not summarized again on resume"
    );
}

#[tokio::test]
async fn require_mode_stages_agent_changes_instead_of_journaling_them() {
    let profile = Profile::new("[memory]\nreview_mode = \"require\"\n");
    let memory = profile.memory(profile.session());
    let (ok, out) = call(
        &memory,
        json!({"target": "user", "operations": [{"action": "add", "content": "Likes tea."}]}),
    )
    .await;
    assert!(ok && out["proposal_staged"] == true, "{out}");
    assert!(profile.store.memory_entries(None).unwrap().is_empty());
    assert!(
        profile
            .store
            .memory_revisions(&RevisionFilter::default())
            .unwrap()
            .is_empty()
    );
}

/// Embeds by concept: words about children share one axis, so a
/// paraphrase with no word in common is still close.
struct Concepts;

impl Embedder for Concepts {
    fn model(&self) -> String {
        "test/concepts".into()
    }

    fn remote(&self) -> bool {
        true
    }

    fn embed(&self, inputs: Vec<String>) -> BoxFuture<'_, Result<Vec<Vec<f32>>, Error>> {
        Box::pin(async move {
            Ok(inputs
                .iter()
                .map(|text| {
                    let t = text.to_lowercase();
                    let kid = ["kid", "daughter", "son", "child"]
                        .iter()
                        .any(|w| t.contains(w));
                    let db = ["postgres", "database"].iter().any(|w| t.contains(w));
                    vec![f32::from(u8::from(kid)), f32::from(u8::from(db)), 0.1]
                })
                .collect())
        })
    }
}

async fn archival(memory: &pmt_memory::MemorySession, content: &str) -> String {
    let (ok, out) = call(
        memory,
        json!({"target": "user", "operations": [{"action": "add", "tier": "archival", "content": content}]}),
    )
    .await;
    assert!(ok, "{out}");
    out["results"][0]["id"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn a_paraphrase_is_recalled_with_embeddings() {
    // Scenario: Paraphrase is recalled with embeddings.
    let profile = Profile::new("[memory.recall]\nembedding_model = \"test/concepts\"\n");
    let session = profile.session();
    let writer = profile.memory(session);
    let ana = archival(&writer, "The user's daughter is named Ana.").await;
    archival(&writer, "Project Atlas uses Postgres 16.").await;

    let lexical = profile.memory(session);
    assert!(
        lexical.begin_turn("what's my kid called?").await.is_none(),
        "no shared word: lexical recall finds nothing"
    );

    let mut setup = profile.setup(session);
    setup.embedder = Some(Arc::new(Concepts));
    let semantic = pmt_memory::MemorySession::new(setup);
    let recalled = semantic.begin_turn("what's my kid called?").await.unwrap();

    assert_eq!(recalled.entry_ids, vec![ana]);
    assert!(recalled.text.contains("The user's daughter is named Ana."));
}

#[tokio::test]
async fn lexical_recall_ranks_by_bm25_and_hybrid_drops_low_scores() {
    let profile = Profile::new("");
    let session = profile.session();
    let memory = profile.memory(session);
    let atlas = archival(&memory, "Project Atlas uses Postgres 16.").await;
    archival(&memory, "Lunch is at noon.").await;
    let config = profile.scope.config().memory.clone();
    let redactor = pmt_core::secret::Redactor::default();
    let ranked = pmt_memory::rank::rank(
        &profile.store,
        &config,
        None,
        &redactor,
        "which DB does Atlas use?",
    )
    .await
    .unwrap();
    assert_eq!(ranked.len(), 1);
    assert_eq!(ranked[0].id, atlas);
    assert!((ranked[0].bm25 - 1.0).abs() < 1e-9);

    let hybrid = pmt_memory::rank::rank(
        &profile.store,
        &config,
        Some(&Concepts),
        &redactor,
        "my database",
    )
    .await
    .unwrap();
    assert_eq!(hybrid.len(), 1, "lunch scores below min_score: {hybrid:?}");
    assert_eq!(hybrid[0].id, atlas);
    let stored = profile.store.memory_embeddings("test/concepts").unwrap();
    assert_eq!(
        stored.len(),
        2,
        "every eligible entry was embedded and stored"
    );
}
