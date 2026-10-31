//! Write approval, pending changes and the background review (spec:
//! memory, Write approval and pending changes; Background review cannot
//! delete unattended; Memory nudge interval).

use std::sync::Arc;

use pmt_agent::turn::model::BoxFuture;
use pmt_memory::{Approver, MemorySession, slash, write};
use pmt_store::{MemoryOrigin, MemoryStatus};
use serde_json::json;

use crate::support::{Profile, add, call, mock_model};

struct Answer(Option<bool>);

impl Approver for Answer {
    fn ask<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Option<bool>> {
        Box::pin(async move { self.0 })
    }
}

#[tokio::test]
async fn approval_stages_without_a_person_and_a_stale_patch_is_refused() {
    let profile = Profile::new("[memory]\nwrite_approval = true\n");
    let session = profile.session();
    // The user's own seed entry (approval applies to agent writes only).
    let seeded = pmt_memory::manage::import(
        &profile.store,
        &profile.scope.config().memory,
        &[pmt_memory::manage::Item {
            id: None,
            target: pmt_store::MemoryTarget::Memory,
            kind: pmt_store::MemoryKind::Fact,
            tier: pmt_store::MemoryTier::Core,
            pinned: false,
            content: "A".into(),
        }],
        &pmt_core::secret::Redactor::default(),
    )
    .unwrap();
    assert_eq!(seeded.added, 1);
    let id = profile.store.memory_entries(None).unwrap()[0].id.clone();

    let memory = profile.memory(session);
    let (ok, out) = call(
        &memory,
        json!({"target": "memory", "operations": [{"action": "patch", "id": id, "content": "A2"}]}),
    )
    .await;
    assert!(ok, "{out}");
    assert_eq!(out["proposal_staged"], true);
    let pending_id = out["pending_id"].as_str().unwrap().to_owned();
    assert_eq!(
        profile.store.memory_entry(&id).unwrap().unwrap().content,
        "A"
    );

    // The user edits the entry to B after the proposal was staged.
    profile
        .store
        .memory_batch(None, |b| {
            b.entries[0].content = "B".into();
            Ok(())
        })
        .unwrap();
    let config = &profile.scope.config().memory;
    let err = write::approve(&profile.store, config, &pending_id).unwrap_err();
    assert_eq!(err.code().code, "memory.stale");
    assert_eq!(
        profile.store.memory_entry(&id).unwrap().unwrap().content,
        "B"
    );
    let reply = slash::run(&profile.store, config, "pending").unwrap();
    assert!(reply.text.contains(&pending_id), "{}", reply.text);
    slash::run(&profile.store, config, &format!("reject {pending_id}")).unwrap();
    assert!(profile.store.memory_pending().unwrap().is_empty());
}

#[tokio::test]
async fn approve_applies_a_staged_add_and_inline_answers_decide() {
    let profile = Profile::new("[memory]\nwrite_approval = true\n");
    let session = profile.session();
    let memory = profile.memory(session);
    let (_, out) = call(
        &memory,
        json!({"target": "user", "operations": [{"action": "add", "content": "Likes tea."}]}),
    )
    .await;
    let pending_id = out["pending_id"].as_str().unwrap().to_owned();
    assert!(profile.store.memory_entries(None).unwrap().is_empty());
    let reply = slash::run(
        &profile.store,
        &profile.scope.config().memory,
        &format!("approve {pending_id}"),
    )
    .unwrap();
    assert!(reply.text.contains("Approved"), "{}", reply.text);
    let entries = profile.store.memory_entries(None).unwrap();
    assert_eq!(entries[0].content, "Likes tea.");
    assert_eq!(entries[0].origin, MemoryOrigin::Agent);

    let mut yes = profile.setup(session);
    yes.approver = Some(Arc::new(Answer(Some(true))));
    let yes = MemorySession::new(yes);
    let (ok, out) = call(
        &yes,
        json!({"target": "user", "operations": [{"action": "add", "content": "Likes coffee."}]}),
    )
    .await;
    assert!(ok && out.get("pending_id").is_none(), "{out}");
    let mut no = profile.setup(session);
    no.approver = Some(Arc::new(Answer(Some(false))));
    let no = MemorySession::new(no);
    let (ok, out) = call(
        &no,
        json!({"target": "user", "operations": [{"action": "add", "content": "Likes juice."}]}),
    )
    .await;
    assert!(
        !ok && out["error"].as_str().unwrap().contains("declined"),
        "{out}"
    );
    assert_eq!(profile.store.memory_entries(None).unwrap().len(), 2);
}

#[tokio::test]
async fn a_background_remove_becomes_a_proposal_while_adds_commit() {
    let profile = Profile::new("");
    let session = profile.session();
    let id = add(&profile.memory(session), "memory", "Deploys on Fridays.").await;
    let mut setup = profile.setup(session);
    setup.review = true;
    let review = MemorySession::new(setup);
    let (ok, out) = call(
        &review,
        json!({"target": "memory", "operations": [
            {"action": "remove", "id": id},
            {"action": "add", "content": "Deploys on Thursdays."}
        ]}),
    )
    .await;
    assert!(ok, "{out}");
    assert_eq!(out["proposal_staged"], true);
    assert!(out["message"].as_str().unwrap().contains("/memory pending"));
    let entry = profile.store.memory_entry(&id).unwrap().unwrap();
    assert_eq!(entry.status, MemoryStatus::Active, "the entry stays");
    let pending = profile.store.memory_pending().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].origin, MemoryOrigin::BackgroundReview);
    let added = profile.store.memory_entries(None).unwrap();
    assert!(added.iter().any(
        |e| e.content == "Deploys on Thursdays." && e.origin == MemoryOrigin::BackgroundReview
    ));
}

const REVIEW_SCRIPT: &str = r#"
[[rule]]
times = 1
[[rule.tool_calls]]
name = "memory"
arguments = { target = "user", operations = [{ action = "remove", old_text = "metric" }, { action = "add", content = "Works in UTC." }] }

[[rule]]
text = "Saved."
"#;

#[tokio::test]
async fn the_background_review_runs_in_a_child_session_and_never_deletes() {
    let profile = Profile::new("");
    let session = profile.session();
    let parent = profile.memory(session);
    let id = add(&parent, "user", "Prefers metric units.").await;
    let model = mock_model(&profile, REVIEW_SCRIPT).await;
    let child = pmt_agent::review::child_session(&profile.store, session).unwrap();
    let result = pmt_memory::review::run(pmt_memory::review::ReviewRun {
        parent,
        session,
        child,
        model,
        focus: None,
    })
    .await
    .unwrap();
    assert_eq!(result.final_response.as_deref(), Some("Saved."));
    assert_eq!(
        profile.store.memory_entry(&id).unwrap().unwrap().status,
        MemoryStatus::Active
    );
    assert_eq!(profile.store.memory_pending().unwrap().len(), 1);
    assert!(
        profile
            .store
            .memory_entries(None)
            .unwrap()
            .iter()
            .any(|e| e.content == "Works in UTC.")
    );
}

const FOCUSED_SCRIPT: &str = r#"
[[rule]]
user = "focus on: time zones"
times = 1
[[rule.tool_calls]]
name = "memory"
arguments = { target = "user", operations = [{ action = "remove", old_text = "metric" }, { action = "add", content = "Works in UTC." }] }

[[rule]]
text = "Saved."
"#;

/// `/refine <focus>` (spec: skills, /refine runs self-improvement review
/// on demand): the focus reaches the memory review and its summary
/// counts what it saved and what awaits approval.
#[tokio::test]
async fn a_focused_review_reports_its_memory_changes() {
    let profile = Profile::new("");
    let session = profile.session();
    let parent = profile.memory(session);
    add(&parent, "user", "Prefers metric units.").await;
    let model = mock_model(&profile, FOCUSED_SCRIPT).await;
    let child = pmt_agent::review::child_session(&profile.store, session).unwrap();
    let (changes, result) = pmt_memory::review::refine(pmt_memory::review::ReviewRun {
        parent,
        session,
        child,
        model,
        focus: Some("time zones".to_owned()),
    })
    .await
    .unwrap();
    assert_eq!(result.final_response.as_deref(), Some("Saved."));
    assert_eq!(
        (changes.saved, changes.updated, changes.proposed),
        (1, 0, 1)
    );
    assert_eq!(
        changes.summary(),
        "Memory: 1 saved, 1 proposed (see /memory pending)."
    );
}

#[tokio::test]
async fn the_nudge_counts_user_turns_without_memory_calls() {
    let profile = Profile::new("[memory]\nnudge_interval = 3\n");
    let memory = profile.memory(profile.session());
    assert!(!memory.end_turn());
    assert!(!memory.end_turn());
    assert!(memory.end_turn(), "third turn is due");
    add(&memory, "user", "Likes tea.").await;
    assert!(!memory.end_turn(), "a memory call resets");
}
