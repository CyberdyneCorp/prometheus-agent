//! Browse, search, inspect, snapshots and publishing.

use pmt_skillhub::install::{self, Options};
use pmt_skillhub::publish::{self, Target};
use pmt_skillhub::{Identifier, inspect, search, snapshot, taps};
use pmt_skills::lint::LintContext;
use serde_json::json;

use crate::support::{Env, skill_md, snapshot as files_of};

#[test]
fn inspect_installs_nothing() {
    let env = Env::new();
    env.fake.repo(
        "openai/skills",
        &[
            (
                "skills/k8s/SKILL.md",
                skill_md("k8s", "Use kubectl.").as_bytes(),
            ),
            ("skills/k8s/references/cheats.md", b"kubectl get pods"),
        ],
    );
    let before = files_of(&env.skills());
    let shown =
        inspect::inspect(&env.hub, &Identifier::parse("openai/skills/k8s").unwrap()).unwrap();
    assert_eq!(shown.name, "k8s");
    assert_eq!(shown.trust, "trusted");
    assert_eq!(shown.files, ["SKILL.md", "references/cheats.md"]);
    assert!(shown.skill_md.contains("Use kubectl."));
    assert_eq!(shown.scan.verdict.as_str(), "safe");
    assert!(shown.upstream.repo_url.unwrap().contains("openai/skills"));
    assert_eq!(
        files_of(&env.skills()),
        before,
        "inspect wrote under skills/"
    );
    assert!(!env.profile().join("cache/skills-hub/quarantine").exists());
}

#[test]
fn browse_lists_the_official_catalog_first_without_network() {
    let env = Env::new();
    // The central index is unreachable (404): official still lists.
    let listed = search::browse(&env.hub, Some("official"));
    let ids: Vec<&str> = listed.iter().map(|l| l.identifier.as_str()).collect();
    assert_eq!(
        ids,
        ["official/mlops/axolotl", "official/research/arxiv-tools"]
    );
    assert!(
        env.fake.requests().is_empty(),
        "browsing official made requests"
    );
}

#[test]
fn search_uses_the_index_then_live_sources() {
    let env = Env::new();
    env.fake.json(
        "/index.json",
        &json!({"skills": [
            {"identifier": "anthropics/skills/pdf", "name": "pdf", "description": "PDF forms."}
        ]}),
    );
    let found = search::search(&env.hub, "pdf", None);
    assert_eq!(found[0].identifier, "anthropics/skills/pdf");
    // No index match: skills.sh answers live.
    env.fake.json(
        "/skills-sh/api/search?q=terraform&limit=10",
        &json!({"skills": [
            {"id": "hashicorp/agent-skills/terraform", "name": "terraform", "installs": 42}
        ]}),
    );
    let found = search::search(&env.hub, "terraform", None);
    assert_eq!(
        found[0].identifier,
        "skills-sh/hashicorp/agent-skills/terraform"
    );
    assert_eq!(found[0].installs, Some(42));
    // The index is cached: a second search does not refetch it.
    let fetches = |env: &Env| {
        env.fake
            .requests()
            .iter()
            .filter(|(r, _)| r == "GET /index.json")
            .count()
    };
    let before = fetches(&env);
    assert_eq!(search::search(&env.hub, "pdf", None).len(), 1);
    assert_eq!(fetches(&env), before);
}

#[test]
fn snapshot_round_trip() {
    let mut env = Env::new();
    taps::add(env.profile(), "my-org/agent-skills", None).unwrap();
    env.reload_taps();
    env.fake.repo(
        "my-org/agent-skills",
        &[
            ("skills/a/SKILL.md", skill_md("a", "1").as_bytes()),
            ("skills/b/SKILL.md", skill_md("b", "1").as_bytes()),
        ],
    );
    for id in [
        "my-org/agent-skills/a",
        "my-org/agent-skills/b",
        "official/mlops/axolotl",
    ] {
        install::install(
            &env.hub,
            &Identifier::parse(id).unwrap(),
            &Options::default(),
        )
        .unwrap();
    }
    let text = serde_json::to_string(&snapshot::export(&env.hub).unwrap()).unwrap();
    let parsed = snapshot::parse(&text).unwrap();
    assert_eq!(parsed.skills.len(), 3);
    assert_eq!(parsed.taps.len(), 1);

    let mut fresh = Env::new();
    fresh.hub.endpoints = env.fake.endpoints();
    let rows = snapshot::import(&mut fresh.hub, &parsed, false);
    assert!(rows.iter().all(|r| r.action != "failed"), "{rows:?}");
    let names: Vec<String> = pmt_skillhub::maintain::entries(&fresh.hub)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, ["a", "axolotl", "b"]);
    assert_eq!(
        taps::read(fresh.profile()).unwrap()[0].repo,
        "my-org/agent-skills"
    );
}

fn lint_ctx(dir: &str) -> LintContext<'_> {
    static RESOLVES: fn(&str) -> bool = |_| true;
    LintContext {
        dir_name: dir,
        description_chars: 60,
        resolves: &RESOLVES,
    }
}

#[test]
fn publish_opens_a_pull_request_and_never_pushes_to_the_default_branch() {
    let mut env = Env::new();
    env.hub.github_token = Some("t0ken".to_owned());
    let dir = env.profile().join("work/deploy-runbook");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        skill_md("deploy-runbook", "Deploy safely."),
    )
    .unwrap();
    let pair = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let key = env.profile().join("sign.key");
    std::fs::write(&key, pair.sk.to_box(None).unwrap().to_string()).unwrap();
    let target = Target::GitHub {
        repo: "team/skills".to_owned(),
    };
    let published = publish::publish(
        &env.hub,
        &dir,
        &target,
        Some(&key),
        &lint_ctx("deploy-runbook"),
    )
    .unwrap();
    assert_eq!(published.url, "https://github.com/team/skills/pull/1");
    assert!(published.signed && dir.join("SKILL.sig").exists());
    let requests: Vec<String> = env.fake.requests().into_iter().map(|(r, _)| r).collect();
    assert!(
        requests
            .iter()
            .any(|r| r == "POST /github/repos/team/skills/pulls"),
        "{requests:?}"
    );
    assert!(
        requests
            .iter()
            .all(|r| !r.contains("heads/main") || r.starts_with("GET")),
        "{requests:?}"
    );
    assert!(requests.iter().any(|r| r.starts_with(
        "PUT /github/repos/publisher/skills/contents/skills/deploy-runbook/SKILL.md"
    )));
}

#[test]
fn publish_refuses_lint_findings_and_dangerous_skills() {
    let mut env = Env::new();
    env.hub.github_token = Some("t0ken".to_owned());
    let dir = env.profile().join("work/flashy");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: flashy\ndescription: A powerful tool.\n---\nbody\n",
    )
    .unwrap();
    let target = Target::GitHub {
        repo: "team/skills".to_owned(),
    };
    let err = publish::publish(&env.hub, &dir, &target, None, &lint_ctx("flashy")).unwrap_err();
    assert!(err.message().contains("banned_word"), "{}", err.message());
    std::fs::write(dir.join("SKILL.md"), skill_md("flashy", "rm -rf / ")).unwrap();
    let err = publish::publish(&env.hub, &dir, &target, None, &lint_ctx("flashy")).unwrap_err();
    assert!(err.message().contains("dangerous"), "{}", err.message());
    assert!(env.fake.requests().is_empty());
}
