//! Decay, expiry, dormancy and consolidation on a fake clock: every check
//! passes its own `now` (spec: memory, Decay, expiry and dormancy;
//! Consolidation proposals).

use jiff::{SignedDuration, Timestamp};
use pmt_core::secret::Redactor;
use pmt_memory::consolidate::{self, Kind, Proposal};
use pmt_memory::rank::{self, Options};
use pmt_memory::{decay, snapshot};
use pmt_store::{
    MemoryEntry, MemoryOrigin, MemoryStatus, MemoryTarget, MemoryTier, RevisionFilter,
};
use serde_json::json;

use crate::support::{Profile, add, call};

const DAY: i64 = 86_400;

fn t0() -> Timestamp {
    Timestamp::from_second(1_780_000_000).unwrap()
}

fn days(n: i64) -> Timestamp {
    t0().checked_add(SignedDuration::from_secs(n * DAY))
        .unwrap()
}

/// Rewrites entry `id` with `edit` (bookkeeping, not journaled).
fn set(profile: &Profile, id: &str, edit: impl Fn(&mut MemoryEntry)) {
    profile
        .store
        .memory_batch(None, |b| {
            edit(b.entries.iter_mut().find(|e| e.id == id).unwrap());
            Ok(())
        })
        .unwrap();
}

fn entry(profile: &Profile, id: &str) -> MemoryEntry {
    profile.store.memory_entry(id).unwrap().unwrap()
}

async fn archival(profile: &Profile, content: &str) -> String {
    let memory = profile.memory(profile.session());
    let (ok, out) = call(
        &memory,
        json!({"target": "memory", "operations": [{"action": "add", "content": content, "tier": "archival"}]}),
    )
    .await;
    assert!(ok, "{out}");
    let id = out["results"][0]["id"].as_str().unwrap().to_owned();
    set(profile, &id, |e| {
        e.created_at = t0();
        e.updated_at = t0();
    });
    id
}

async fn ranked(profile: &Profile, query: &str, now: Timestamp, all: bool) -> Vec<String> {
    let options = Options {
        now,
        include_dormant: all,
    };
    let config = &profile.scope.config().memory;
    rank::rank_with(
        &profile.store,
        config,
        None,
        &Redactor::default(),
        query,
        options,
    )
    .await
    .unwrap()
    .into_iter()
    .map(|s| s.id)
    .collect()
}

#[tokio::test]
async fn an_expired_event_leaves_the_prompt_and_stays_listed_as_dormant() {
    // Scenario: Expired event leaves the prompt.
    let profile = Profile::new("");
    let memory = profile.memory(profile.session());
    let id = add(&memory, "memory", "Conference in Berlin this week.").await;
    add(&memory, "memory", "Repo uses just.").await;
    set(&profile, &id, |e| e.expires_at = Some(days(-1)));
    let config = &profile.scope.config().memory;

    // A new session's snapshot (the real clock is past the expiry).
    let prompt = profile.memory(profile.session()).snapshot().unwrap();
    assert!(!prompt.contains("Berlin"), "{prompt}");
    assert!(prompt.contains("Repo uses just."), "{prompt}");
    let kept = entry(&profile, &id);
    assert_eq!(kept.status, MemoryStatus::Dormant);
    assert_eq!(kept.content, "Conference in Berlin this week.");

    // Dormancy never deletes, and waking makes it active again.
    let woken = decay::wake(&profile.store, &id, days(0)).unwrap();
    assert_eq!(woken.status, MemoryStatus::Active);
    assert_eq!(woken.expires_at, None);
    let again = snapshot::render_at(&profile.store, config, days(0))
        .unwrap()
        .unwrap();
    assert!(again.contains("Berlin"), "{again}");
}

#[tokio::test]
async fn an_unused_archival_entry_decays_into_dormancy_and_waking_brings_it_back() {
    let profile = Profile::new("");
    let id = archival(&profile, "The staging cluster runs on Hetzner.").await;
    let config = &profile.scope.config().memory;
    // 90 days: salience 0.5, still recalled.
    assert_eq!(
        ranked(&profile, "where is the staging cluster", days(90), false).await,
        std::slice::from_ref(&id)
    );
    // 300 days: 0.5^(300/90) is about 0.1; at 400 days it is 0.046.
    assert!(
        ranked(&profile, "staging cluster", days(400), false)
            .await
            .is_empty()
    );
    assert_eq!(
        decay::sweep(&profile.store, &config.decay, days(400)).unwrap(),
        std::slice::from_ref(&id)
    );
    assert_eq!(entry(&profile, &id).status, MemoryStatus::Dormant);
    // Kept and searchable with --all.
    assert_eq!(
        ranked(&profile, "staging cluster", days(400), true).await,
        std::slice::from_ref(&id)
    );
    decay::wake(&profile.store, &id, days(400)).unwrap();
    assert_eq!(
        ranked(&profile, "staging cluster", days(401), false).await,
        [id]
    );
}

#[tokio::test]
async fn salience_ranks_a_stale_entry_below_a_fresh_one_and_pins_never_decay() {
    let profile = Profile::new("");
    let stale = archival(&profile, "Deploys go through the deploy pipeline.").await;
    let fresh = archival(&profile, "Deploys go through the deploy pipeline!").await;
    set(&profile, &fresh, |e| e.updated_at = days(100));
    assert_eq!(
        ranked(&profile, "deploy pipeline", days(120), false).await,
        [fresh.clone(), stale.clone()]
    );
    let config = &profile.scope.config().memory;
    set(&profile, &stale, |e| e.pinned = true);
    assert_eq!(
        ranked(&profile, "deploy pipeline", days(120), false).await,
        [stale.clone(), fresh]
    );
    let far = days(5000);
    assert!((decay::salience(&entry(&profile, &stale), far, &config.decay) - 1.0).abs() < 1e-9);
    decay::sweep(&profile.store, &config.decay, far).unwrap();
    assert_eq!(entry(&profile, &stale).status, MemoryStatus::Active);
}

#[tokio::test]
async fn editing_a_dormant_entry_makes_it_active() {
    let profile = Profile::new("");
    let id = archival(&profile, "Old build notes.").await;
    let config = &profile.scope.config().memory;
    decay::sweep(&profile.store, &config.decay, days(1000)).unwrap();
    assert_eq!(entry(&profile, &id).status, MemoryStatus::Dormant);
    let memory = profile.memory(profile.session());
    let (ok, out) = call(
        &memory,
        json!({"target": "memory", "operations": [{"action": "patch", "id": id, "content": "New build notes."}]}),
    )
    .await;
    assert!(ok, "{out}");
    assert_eq!(entry(&profile, &id).status, MemoryStatus::Active);
}

fn resolve(ids: &[&str]) -> Proposal {
    Proposal {
        action: Kind::Resolve,
        ids: ids.iter().map(|s| (*s).to_owned()).collect(),
        id: None,
        content: None,
        reason: Some("contradiction".into()),
    }
}

#[tokio::test]
async fn a_contradiction_is_previewed_then_committed_as_consolidation_and_pins_survive() {
    // Scenario: Contradiction is proposed, not silently applied.
    let profile = Profile::new("");
    let memory = profile.memory(profile.session());
    let vim = add(&memory, "user", "Uses vim.").await;
    let helix = add(&memory, "user", "Switched to Helix.").await;
    set(&profile, &vim, |e| {
        e.updated_at = Timestamp::from_second(1_740_000_000).unwrap();
    });
    set(&profile, &helix, |e| {
        e.updated_at = Timestamp::from_second(1_770_000_000).unwrap();
    });
    let config = &profile.scope.config().memory;
    let entries = profile
        .store
        .memory_entries(Some(MemoryTarget::User))
        .unwrap();
    let plan = consolidate::plan(&entries, MemoryTarget::User, &[resolve(&[&helix, &vim])]);
    let diff = consolidate::preview(&profile.store, config, &plan, days(0)).unwrap();
    assert!(diff.contains("-Uses vim."), "{diff}");
    assert!(!diff.contains("-Switched to Helix."), "{diff}");
    // The store is unchanged by a preview.
    assert_eq!(
        profile
            .store
            .memory_entries(Some(MemoryTarget::User))
            .unwrap(),
        entries
    );

    consolidate::commit(&profile.store, config, &plan, days(0)).unwrap();
    assert!(profile.store.memory_entry(&vim).unwrap().is_none());
    let revisions = profile
        .store
        .memory_revisions(&RevisionFilter {
            origin: Some(MemoryOrigin::Consolidation),
            ..RevisionFilter::default()
        })
        .unwrap();
    assert_eq!(revisions.len(), 1);
    assert_eq!(revisions[0].entry_id, vim);

    // A pinned older entry wins, and a pinned entry is never removed.
    let tabs = add(&memory, "user", "Indents with tabs.").await;
    let spaces = add(&memory, "user", "Indents with spaces.").await;
    set(&profile, &tabs, |e| e.pinned = true);
    let entries = profile
        .store
        .memory_entries(Some(MemoryTarget::User))
        .unwrap();
    let plan = consolidate::plan(&entries, MemoryTarget::User, &[resolve(&[&tabs, &spaces])]);
    consolidate::commit(&profile.store, config, &plan, days(0)).unwrap();
    assert!(profile.store.memory_entry(&tabs).unwrap().is_some());
    assert!(profile.store.memory_entry(&spaces).unwrap().is_none());
    let demote = Proposal {
        action: Kind::Demote,
        ids: Vec::new(),
        id: Some(tabs.clone()),
        content: None,
        reason: None,
    };
    let plan = consolidate::plan(
        &profile
            .store
            .memory_entries(Some(MemoryTarget::User))
            .unwrap(),
        MemoryTarget::User,
        &[demote],
    );
    consolidate::commit(&profile.store, config, &plan, days(0)).unwrap();
    assert_eq!(entry(&profile, &tabs).tier, MemoryTier::Archival);
}

#[test]
fn the_idle_pass_seeds_then_waits_for_its_interval_and_for_quiet() {
    let profile = Profile::new("[memory.consolidation]\nenabled = true\ninterval_secs = 3600\n");
    let config = &profile.scope.config().memory;
    let store = &profile.store;
    assert!(
        !consolidate::due(store, config, days(0), None).unwrap(),
        "seeded"
    );
    let hour = |h: i64| days(0).checked_add(SignedDuration::from_hours(h)).unwrap();
    assert!(!consolidate::due(store, config, hour(0), None).unwrap());
    assert!(consolidate::due(store, config, hour(2), None).unwrap());
    // A session active a minute ago defers it.
    let busy = hour(2).checked_sub(SignedDuration::from_mins(1)).unwrap();
    assert!(!consolidate::due(store, config, hour(2), Some(busy)).unwrap());
    consolidate::mark_run(store, hour(2)).unwrap();
    assert!(!consolidate::due(store, config, hour(2), None).unwrap());
    let off = Profile::new("");
    assert!(!consolidate::due(&off.store, &off.scope.config().memory, hour(9), None).unwrap());
}
