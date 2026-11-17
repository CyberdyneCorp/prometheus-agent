//! Library maintenance contracts (spec: experience-library, Retention
//! cap; Rollback demotes entries; Lessons offered to later sessions;
//! Restore on resume and repair; Export and import between profiles;
//! Library events).

use std::path::Path;

use pmt_config::harness::{HarnessDir, Source, Tree};
use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::secret::Redactor;
use pmt_evolve::experience::archive::{self, ImportRules};
use pmt_evolve::experience::entry::{EntryMeta, Status};
use pmt_evolve::experience::lesson::{Confidence, Distilled, Edit, Lesson};
use pmt_evolve::experience::library::NewEntry;
use pmt_evolve::experience::{self, Library, Outcome, Settings, ops};
use pmt_evolve::harness::Rules;
use pmt_evolve::proposal::{NewProposal, Proposals};

struct Profile {
    _root: tempfile::TempDir,
    scope: ProfileScope,
}

impl Profile {
    fn new(name: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = Home::open(&root.path().join("home")).unwrap();
        let profile = home.profile(&ProfileName::parse(name).unwrap()).unwrap();
        let overrides = Overrides::parse(&["evolve.enabled=true"]).unwrap();
        let scope = ProfileScope::open(profile, &overrides, Launch::Other).unwrap();
        Self { _root: root, scope }
    }

    fn dir(&self) -> &Path {
        self.scope.dir()
    }

    fn settings(&self) -> Settings {
        let config = self.scope.config();
        Settings::of(&config.evolve, &config.experience, Redactor::default())
    }
}

fn rules() -> Rules {
    Rules::with_toolsets(["file".to_owned()])
}

fn meta(id: &str, domain: &str, created_at: &str, confidence: Confidence) -> EntryMeta {
    EntryMeta {
        entry_id: id.to_owned(),
        domain: domain.to_owned(),
        job_id: "evo_1".into(),
        harness_version: "h1-x".into(),
        mode: "parallel".into(),
        k: 1,
        confidence,
        created_at: created_at.to_owned(),
        pinned: false,
        status: Status::Active,
        times_shown: 0,
        title: format!("task {id}"),
        cue: format!("cue {id}"),
        lesson: format!("lesson {id}"),
        inventory: "always-on edits only".into(),
        proposal_id: String::new(),
        imported_from: None,
    }
}

fn write(library: &Library, meta: EntryMeta, cap: usize) -> Vec<String> {
    library
        .write(
            &NewEntry {
                meta,
                document: "# task\n".into(),
                tree: Tree::builtin(),
            },
            cap,
        )
        .unwrap()
        .evicted
}

/// Scenario: Eviction.
#[test]
fn the_oldest_unpinned_entry_is_evicted_and_a_pinned_one_kept() {
    let p = Profile::new("work");
    let library = Library::of(p.dir());
    let stamp = |i: u32| format!("2026-10-01T00:{:02}:{:02}Z", i / 60, i % 60);
    for i in 0..50 {
        let mut m = meta(
            &format!("20261001-t-{i:06x}"),
            "accounting",
            &stamp(i),
            Confidence::High,
        );
        m.pinned = i == 0;
        write(&library, m, 50);
    }
    assert_eq!(library.entries("accounting").len(), 50);
    let evicted = write(
        &library,
        meta(
            "20261002-t-ffffff",
            "accounting",
            &stamp(100),
            Confidence::High,
        ),
        50,
    );
    assert_eq!(evicted, ["20261001-t-000001"]);
    let ids: Vec<String> = library
        .entries("accounting")
        .into_iter()
        .map(|e| e.entry_id)
        .collect();
    assert_eq!(ids.len(), 50);
    assert!(
        ids.contains(&"20261001-t-000000".to_owned()),
        "the pinned one stays"
    );
    let events = library.log().read();
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "experience.entry.evicted"
                && e["payload"]["entry_id"] == "20261001-t-000001")
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e["type"] == "experience.entry.added")
            .count(),
        51
    );
}

/// Scenario: Rolling back an adopted harness.
#[test]
fn a_rollback_demotes_entries_of_versions_the_harness_left() {
    let p = Profile::new("work");
    let harness = HarnessDir::of(p.dir());
    let h1 = harness.active_id().unwrap();
    let mut tree = harness.tree(&h1).unwrap();
    tree.files
        .insert("GUARDRAILS.md".into(), "Sum refunds.".into());
    let h2 = harness.write(&tree, Source::Evolve, Some("evo_1")).unwrap();
    harness.activate(&h2.id).unwrap();
    let library = Library::of(p.dir());
    let mut adopted = meta(
        "20261029-a-aaaaaa",
        "accounting",
        "2026-10-29T00:00:00Z",
        Confidence::High,
    );
    adopted.harness_version.clone_from(&h2.id);
    let mut pinned = meta(
        "20261029-b-bbbbbb",
        "accounting",
        "2026-10-29T00:00:01Z",
        Confidence::High,
    );
    pinned.harness_version.clone_from(&h2.id);
    pinned.pinned = true;
    let mut older = meta(
        "20261028-c-cccccc",
        "accounting",
        "2026-10-28T00:00:00Z",
        Confidence::High,
    );
    older.harness_version.clone_from(&h1);
    for m in [adopted, pinned, older] {
        write(&library, m, 50);
    }
    assert!(
        ops::demote_after_rollback(p.dir()).unwrap().is_empty(),
        "h2 is active"
    );

    // `harness rollback h1`: a new version holding h1's tree.
    let back = harness
        .write(&harness.tree(&h1).unwrap(), Source::Rollback, None)
        .unwrap();
    harness.activate(&back.id).unwrap();
    assert_eq!(
        ops::demote_after_rollback(p.dir()).unwrap(),
        ["20261029-a-aaaaaa"]
    );
    let status = |id: &str| library.find(id).unwrap().status;
    assert_eq!(status("20261029-a-aaaaaa"), Status::RolledBack);
    assert_eq!(status("20261029-b-bbbbbb"), Status::Active, "pinned");
    assert_eq!(
        status("20261028-c-cccccc"),
        Status::Active,
        "h1 is the lineage"
    );
    let snapshot = p.dir().join("snap");
    assert!(library.snapshot("accounting", &snapshot, 20).unwrap());
    assert!(!snapshot.join("entries/20261029-a-aaaaaa").exists());
    assert!(
        library
            .log()
            .read()
            .iter()
            .any(|e| e["type"] == "experience.entry.status_changed"
                && e["payload"]["status"] == "rolled-back")
    );
    ops::restore(p.dir(), "20261029-a-aaaaaa").unwrap();
    assert_eq!(status("20261029-a-aaaaaa"), Status::Active);
}

/// Scenario: Low-confidence lesson excluded.
#[test]
fn only_high_and_medium_lessons_reach_sessions_and_each_showing_counts() {
    let p = Profile::new("work");
    let library = Library::of(p.dir());
    write(
        &library,
        meta(
            "20261029-low-111111",
            "devops",
            "2026-10-29T00:00:00Z",
            Confidence::Low,
        ),
        50,
    );
    write(
        &library,
        meta(
            "20261029-high-222222",
            "devops",
            "2026-10-29T00:00:01Z",
            Confidence::High,
        ),
        50,
    );
    let (text, ids) = ops::session_lessons(p.dir(), "devops", 5).unwrap();
    assert_eq!(ids, ["20261029-high-222222"]);
    assert_eq!(
        text,
        "## Lessons from earlier work in devops\n- cue 20261029-high-222222 → lesson 20261029-high-222222"
    );
    assert!(ops::session_lessons(p.dir(), "accounting", 5).is_none());
    ops::shown(p.dir(), &ids);
    assert_eq!(library.find("20261029-high-222222").unwrap().times_shown, 1);
}

fn lesson() -> Distilled {
    Distilled {
        edits: vec![Edit {
            component: "GUARDRAILS.md".into(),
            verdict: "helped".into(),
            ..Edit::default()
        }],
        lesson: Some(Lesson {
            cue: "ledgers with refunds".into(),
            lesson: "sum signed amounts".into(),
            evidence: "base1 step 4".into(),
            confidence: Confidence::Medium,
            confidence_reason: String::new(),
        }),
    }
}

/// Adopts a proposal with a lesson in `domain`; the entry id.
fn adopt_with_lesson(p: &Profile, domain: &str, title: &str, line: &str) -> String {
    let harness = HarnessDir::of(p.dir());
    let base_version = harness.active_id().unwrap();
    let base = harness.tree(&base_version).unwrap();
    let mut tree = base.clone();
    tree.files.insert("GUARDRAILS.md".into(), line.into());
    let distilled = lesson();
    let proposal = Proposals::of(p.dir())
        .create(&NewProposal {
            job_id: "evo_1",
            mode: "parallel",
            label: "agg1",
            base_version: &base_version,
            base: &base,
            tree: &tree,
            reason: "it states the total",
            selected_by: "model",
            domain,
            title,
            k: 1,
            distilled: &distilled,
        })
        .unwrap();
    match experience::adopt(p.dir(), &proposal.id, false, &rules(), &p.settings())
        .unwrap()
        .1
    {
        Outcome::Written(w) => w.meta.entry_id,
        other => panic!("{other:?}"),
    }
}

/// Scenario: Deleted library directory.
#[test]
fn a_deleted_domain_is_rebuilt_from_adopted_proposals() {
    let p = Profile::new("work");
    let first = adopt_with_lesson(&p, "accounting", "Reconcile Q3 ledger", "Sum refunds.");
    let second = adopt_with_lesson(&p, "accounting", "Close the month", "Check totals.");
    std::fs::remove_dir_all(p.dir().join("experience/accounting")).unwrap();
    let mut restored = ops::rebuild(p.dir(), &p.settings(), Some("accounting")).unwrap();
    restored.sort();
    let mut expected = vec![first, second];
    expected.sort();
    assert_eq!(
        restored, expected,
        "the same entries, by their recorded ids"
    );
    let doc = Library::of(p.dir()).document(&Library::of(p.dir()).find(&expected[0]).unwrap());
    assert!(doc.contains("## Transferable lesson"), "{doc}");
    // A forgotten entry is not brought back.
    ops::forget(p.dir(), &expected[0]).unwrap();
    assert!(
        ops::rebuild(p.dir(), &p.settings(), Some("accounting"))
            .unwrap()
            .is_empty()
    );
}

/// Scenario: Archive with a secret.
#[test]
fn an_archive_with_a_secret_is_refused_whole() {
    let source = Profile::new("source");
    let clean = adopt_with_lesson(&source, "accounting", "Reconcile Q3 ledger", "Sum refunds.");
    let file = source.dir().join("accounting.tar.zst");
    assert_eq!(
        archive::export(source.dir(), "accounting", &file).unwrap(),
        1
    );

    let target = Profile::new("target");
    let rules = rules();
    let settings = target.settings();
    let checks = ImportRules {
        rules: &rules,
        settings: &settings,
    };
    let imported = archive::import(target.dir(), &file, Some("Finance Ops"), checks).unwrap();
    assert_eq!(imported.len(), 1);
    let entry = &imported[0];
    assert_eq!(entry.domain, "finance-ops");
    assert_ne!(entry.entry_id, clean, "a new id");
    assert_eq!(entry.status, Status::Active);
    assert_eq!(
        entry.imported_from.as_deref(),
        Some(file.display().to_string().as_str())
    );

    // A second archive: the clean entry plus one whose skill holds a key.
    let tampered = source.dir().join("tampered.tar.zst");
    let mut builder = tar::Builder::new(Vec::new());
    let unpacked = zstd::decode_all(std::fs::read(&file).unwrap().as_slice()).unwrap();
    let mut original = tar::Archive::new(unpacked.as_slice());
    let mut add = |path: &str, bytes: &[u8]| {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o600);
        header.set_cksum();
        builder.append_data(&mut header, path, bytes).unwrap();
    };
    let mut files = Vec::new();
    for item in original.entries().unwrap() {
        let mut item = item.unwrap();
        let path = item.path().unwrap().to_string_lossy().into_owned();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut item, &mut bytes).unwrap();
        files.push((path, bytes));
    }
    for (path, bytes) in &files {
        add(path, bytes);
        if let Some(rest) = path.strip_prefix(&format!("entries/{clean}/")) {
            add(&format!("entries/20261030-leaky-abcdef/{rest}"), bytes);
        }
    }
    add(
        "entries/20261030-leaky-abcdef/harness/skills/recon/SKILL.md",
        b"---\nname: recon\ndescription: reconciling ledgers\n---\nUse key sk-live-abcdef1234567890 for the API.\n",
    );
    let packed = zstd::encode_all(builder.into_inner().unwrap().as_slice(), 3).unwrap();
    std::fs::write(&tampered, packed).unwrap();
    let before = Library::of(target.dir()).all();
    let err = archive::import(target.dir(), &tampered, None, checks).unwrap_err();
    assert_eq!(err.exit_code(), 65);
    assert!(
        err.to_string().contains("entry 20261030-leaky-abcdef"),
        "{err}"
    );
    assert_eq!(Library::of(target.dir()).all(), before, "nothing written");
}
