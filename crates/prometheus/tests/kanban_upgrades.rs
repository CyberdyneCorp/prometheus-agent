//! Kanban upgrades through the CLI and real workers on the mock provider
//! (spec: kanban, URL attachments and attachment listing tools; PR
//! completion contracts; Goal-mode cards; Per-task model override and
//! pinned skills; Respawn guard and dispatch dry run; projects, Kanban
//! board binding).

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{mock_home, ok, run, run_with_stdin, stderr, stdout};
use serde_json::Value;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn json(home: &Path, args: &[&str]) -> Value {
    let mut all = vec!["--json"];
    all.extend(args);
    serde_json::from_str(&ok(&run(home, &all))).unwrap()
}

fn create(home: &Path, args: &[&str]) -> String {
    let mut all = vec!["kanban", "create"];
    all.extend(args);
    json(home, &all)["id"].as_str().unwrap().to_owned()
}

fn task(home: &Path, id: &str) -> Value {
    json(home, &["kanban", "show", id])
}

fn status(home: &Path, id: &str) -> String {
    task(home, id)["task"]["status"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// A mock home whose default profile plays `rules`.
fn worker_home(rules: &str) -> common::TestHome {
    let home = mock_home();
    std::fs::write(home.path().join("profiles/default/mock.toml"), rules).unwrap();
    common::append_config(home.path(), "\n[providers.mock]\nscript = \"mock.toml\"\n");
    home
}

/// Ticks until `id` leaves `running`/`ready`, or panics after a minute.
fn work_until_settled(home: &Path, id: &str) -> String {
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        ok(&run(home, &["kanban", "dispatch", "--once"]));
        let now = status(home, id);
        if now != "running" && now != "ready" {
            return now;
        }
        assert!(
            Instant::now() < deadline,
            "task {id} stayed {now}: {}",
            task(home, id)
        );
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// The first run's session, once recorded.
fn run_session(home: &Path, id: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(session) = task(home, id)["runs"][0]["session_id"].as_str() {
            return session.to_owned();
        }
        assert!(Instant::now() < deadline, "no session: {}", task(home, id));
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn tool_results(home: &Path, session: &str) -> Vec<String> {
    common::session_messages(home, session)
        .into_iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m.to_string())
        .collect()
}

struct Server {
    runtime: tokio::runtime::Runtime,
    server: MockServer,
}

impl Server {
    fn start() -> Self {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let server = runtime.block_on(MockServer::start());
        Self { runtime, server }
    }

    fn json(&self, at: &str, body: &Value) {
        self.runtime.block_on(
            Mock::given(method("GET"))
                .and(path(at))
                .and(header("authorization", "Bearer ghp_test_token"))
                .respond_with(ResponseTemplate::new(200).set_body_json(body))
                .mount(&self.server),
        );
    }

    fn uri(&self) -> String {
        self.server.uri()
    }
}

/// Scenario: Worker attaches a report by link.
#[test]
fn a_worker_attaches_a_report_by_link() {
    let server = Server::start();
    server.runtime.block_on(
        Mock::given(method("GET"))
            .and(path("/r/report.pdf"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/pdf")
                    .set_body_bytes(b"%PDF-1.7 quarterly numbers".to_vec()),
            )
            .mount(&server.server),
    );
    let url = format!("{}/r/report.pdf", server.uri());
    let rules = format!(
        "[[rule]]\nuser_regex = 'Kanban task'\n[[rule.tool_calls]]\nname = \"kanban_attach_url\"\n\
         arguments = {{ url = \"{url}\" }}\n\n[[rule]]\ntool_result_regex = 'report.pdf'\n\
         [[rule.tool_calls]]\nname = \"kanban_attachments\"\narguments = {{}}\n\n[[rule]]\n\
         tool_result_regex = 'uploader'\n[[rule.tool_calls]]\nname = \"kanban_complete\"\n\
         arguments = {{ result = \"report attached\" }}\n\n[[rule]]\nrepeat = true\ntext = \"done\"\n"
    );
    let home = worker_home(&rules);
    // The stand-in serves on loopback, which the egress policy refuses
    // unless the profile allows private destinations.
    ok(&run(
        home.path(),
        &["config", "set", "security.egress.allow_private", "true"],
    ));
    let id = create(home.path(), &["fetch the report", "--assignee", "default"]);
    assert_eq!(
        work_until_settled(home.path(), &id),
        "done",
        "{}",
        task(home.path(), &id)
    );
    let listed = json(home.path(), &["kanban", "attachments", &id]);
    let row = &listed["attachments"][0];
    assert_eq!(row["filename"], "report.pdf");
    assert_eq!(row["size"], 26);
    assert_eq!(row["content_type"], "application/pdf");
    assert_eq!(row["uploader"], "default");
    let path = row["path"].as_str().unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"%PDF-1.7 quarterly numbers");
    let session = run_session(home.path(), &id);
    assert!(
        tool_results(home.path(), &session)
            .iter()
            .any(|r| r.contains("uploader") && r.contains("report.pdf")),
        "kanban_attachments listed it to the worker"
    );
}

/// Scenario: Internal address refused.
#[test]
fn an_internal_address_is_refused_and_nothing_is_stored() {
    let rules = "[[rule]]\nuser_regex = 'Kanban task'\n[[rule.tool_calls]]\nname = \"kanban_attach_url\"\n\
         arguments = { url = \"http://169.254.169.254/latest/meta-data\" }\n\n[[rule]]\n\
         tool_result_regex = '169.254.169.254'\n[[rule.tool_calls]]\nname = \"kanban_block\"\n\
         arguments = { kind = \"capability\", reason = \"the link was refused\" }\n\n[[rule]]\n\
         repeat = true\ntext = \"stopped\"\n";
    let home = worker_home(rules);
    let id = create(home.path(), &["fetch metadata", "--assignee", "default"]);
    assert_eq!(work_until_settled(home.path(), &id), "blocked");
    let session = run_session(home.path(), &id);
    let refusal = tool_results(home.path(), &session)
        .into_iter()
        .find(|r| r.contains("169.254.169.254"))
        .expect("the tool answered");
    // The egress policy refuses the metadata endpoint before any byte is
    // sent.
    assert!(
        refusal.contains("\"is_error\":true") && refusal.contains("metadata endpoint"),
        "{refusal}"
    );
    assert_eq!(
        json(home.path(), &["kanban", "attachments", &id])["attachments"],
        Value::Array(vec![])
    );
    let blobs = home.path().join("kanban/blobs");
    assert!(common::files_under(&blobs).is_empty(), "nothing stored");
    assert_denial_audited(home.path(), "169.254.169.254:80");
}

/// `host_port`'s refusal is in the assignee's audit chain under
/// `kanban_attach_url`, like every egress denial, and the chain verifies.
fn assert_denial_audited(home: &Path, host_port: &str) {
    let audit =
        std::fs::read_to_string(home.join("profiles/default/logs/security-audit.jsonl")).unwrap();
    assert!(
        audit
            .lines()
            .any(|line| line.contains("\"kind\":\"egress_denied\"")
                && line.contains(host_port)
                && line.contains("kanban_attach_url")),
        "{audit}"
    );
    let verified = ok(&run(home, &["security", "audit", "verify"]));
    assert!(verified.contains("intact"), "{verified}");
}

/// A loopback link is refused like any internal address, and audited.
#[test]
fn a_loopback_link_is_refused_and_audited() {
    let rules = "[[rule]]\nuser_regex = 'Kanban task'\n[[rule.tool_calls]]\nname = \"kanban_attach_url\"\n\
         arguments = { url = \"http://127.0.0.1:9/secret\" }\n\n[[rule]]\n\
         tool_result_regex = '127.0.0.1'\n[[rule.tool_calls]]\nname = \"kanban_block\"\n\
         arguments = { kind = \"capability\", reason = \"the link was refused\" }\n\n[[rule]]\n\
         repeat = true\ntext = \"stopped\"\n";
    let home = worker_home(rules);
    let id = create(home.path(), &["fetch loopback", "--assignee", "default"]);
    assert_eq!(work_until_settled(home.path(), &id), "blocked");
    assert_eq!(
        json(home.path(), &["kanban", "attachments", &id])["attachments"],
        Value::Array(vec![])
    );
    assert_denial_audited(home.path(), "127.0.0.1:9");
}

/// A home whose GitHub stand-in has PR 41 with a red required `ci/test`
/// and PR 42 green, for profile `default`'s token.
fn github_home() -> (common::TestHome, Server) {
    let server = Server::start();
    let home = mock_home();
    ok(&run(
        home.path(),
        &["config", "set", "kanban.github_api_url", &server.uri()],
    ));
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "GITHUB_TOKEN"],
        "ghp_test_token\n",
    ));
    for (n, conclusion) in [(41, "failure"), (42, "success")] {
        server.json(
            &format!("/repos/acme/api/pulls/{n}"),
            &serde_json::json!({"head": {"sha": format!("sha{n}")}, "base": {"ref": "main"}}),
        );
        server.json(
            &format!("/repos/acme/api/commits/sha{n}/check-runs"),
            &serde_json::json!({"check_runs": [{"id": 900 + n, "name": "ci/test", "status": "completed", "conclusion": conclusion}]}),
        );
        server.json(
            &format!("/repos/acme/api/commits/sha{n}/status"),
            &serde_json::json!({"statuses": []}),
        );
    }
    server.json(
        "/repos/acme/api/branches/main/protection/required_status_checks",
        &serde_json::json!({"contexts": ["ci/test"]}),
    );
    (home, server)
}

/// Claims `id` by hand; returns the ticket line.
fn claim(home: &Path, id: &str) -> String {
    json(home, &["kanban", "claim", id])["ticket"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn complete_as_worker(home: &Path, ticket: &str, id: &str, pr: &str) -> std::process::Output {
    let metadata = format!("{{\"published_pr\": \"{pr}\"}}");
    run_with_stdin(
        home,
        &[
            "kanban",
            "complete",
            id,
            "--ticket",
            "-",
            "--result",
            "PR opened",
            "--metadata",
            &metadata,
        ],
        &format!("{ticket}\n"),
    )
}

/// Scenario: Red required check blocks completion.
#[test]
fn a_red_required_check_blocks_the_workers_completion() {
    let (home, _server) = github_home();
    let id = create(
        home.path(),
        &["ship the api change", "--completion-contract", "acme/api"],
    );
    assert_eq!(
        task(home.path(), &id)["task"]["completion_contract"],
        "acme/api"
    );
    let ticket = claim(home.path(), &id);
    let out = complete_as_worker(
        home.path(),
        &ticket,
        &id,
        "https://github.com/acme/api/pull/41",
    );
    assert_ne!(out.status.code(), Some(0));
    assert!(
        stderr(&out).contains("ci/test (failure)"),
        "{}",
        stderr(&out)
    );
    assert_eq!(status(home.path(), &id), "running");
    let log = json(home.path(), &["kanban", "log", &id]);
    let event = log["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "pr_acceptance")
        .expect("a pr_acceptance event");
    assert_eq!(event["payload"]["classification"], "checks");
    assert_eq!(event["payload"]["required"][0], "ci/test");
    assert_eq!(event["payload"]["head_sha"], "sha41");
    assert_eq!(event["payload"]["check_ids"][0], 941);
}

/// Scenario: Retry cannot substitute a sibling PR.
#[test]
fn a_retry_cannot_substitute_a_sibling_pr() {
    let (home, _server) = github_home();
    let id = create(
        home.path(),
        &["ship it", "--completion-contract", "acme/api"],
    );
    let ticket = claim(home.path(), &id);
    complete_as_worker(
        home.path(),
        &ticket,
        &id,
        "https://github.com/acme/api/pull/41",
    );
    let out = complete_as_worker(
        home.path(),
        &ticket,
        &id,
        "https://github.com/acme/api/pull/42",
    );
    assert!(
        stderr(&out)
            .contains("completion contract already bound to https://github.com/acme/api/pull/41"),
        "{}",
        stderr(&out)
    );
    assert_eq!(status(home.path(), &id), "running");
    // A fresh task bound to the green PR completes.
    let green = create(
        home.path(),
        &["ship it", "--completion-contract", "acme/api"],
    );
    let ticket = claim(home.path(), &green);
    let out = complete_as_worker(
        home.path(),
        &ticket,
        &green,
        "https://github.com/acme/api/pull/42",
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(status(home.path(), &green), "done");
}

/// The assignee's own credential is required: without one the
/// completion is refused as `auth`.
#[test]
fn a_contract_without_the_assignees_credential_is_refused() {
    let (home, _server) = github_home();
    ok(&run(home.path(), &["secrets", "unset", "GITHUB_TOKEN"]));
    let id = create(
        home.path(),
        &["ship it", "--completion-contract", "acme/api"],
    );
    let ticket = claim(home.path(), &id);
    let out = complete_as_worker(
        home.path(),
        &ticket,
        &id,
        "https://github.com/acme/api/pull/42",
    );
    assert!(
        stderr(&out).contains("not logged in for profile default"),
        "{}",
        stderr(&out)
    );
}

/// Scenario: Judge keeps the worker going.
#[test]
fn the_goal_judge_keeps_the_worker_going_in_the_same_session() {
    let rules = "[[rule]]\nuser_regex = 'Kanban task'\ntext = \"I translated half of the pages.\"\n\n\
        [[rule]]\nuser_regex = 'goal judge'\ntext = '{\"verdict\": \"continue\", \"reason\": \"half the pages remain\"}'\n\n\
        [[rule]]\nuser_regex = 'goal continuation'\ntext = \"I translated more pages.\"\n\n\
        [[rule]]\nuser_regex = 'goal judge'\ntext = '{\"verdict\": \"continue\", \"reason\": \"a few pages remain\"}'\n\n\
        [[rule]]\nuser_regex = 'goal continuation'\n[[rule.tool_calls]]\nname = \"kanban_complete\"\n\
        arguments = { result = \"every page translated\" }\n\n\
        [[rule]]\nuser_regex = 'goal judge'\ntext = '{\"verdict\": \"done\", \"reason\": \"all pages are translated\"}'\n\n\
        [[rule]]\nrepeat = true\ntext = \"finished\"\n";
    let home = worker_home(rules);
    let id = create(
        home.path(),
        &["translate every page", "--assignee", "default", "--goal"],
    );
    assert_eq!(
        work_until_settled(home.path(), &id),
        "done",
        "{}",
        task(home.path(), &id)
    );
    let shown = task(home.path(), &id);
    assert_eq!(
        shown["runs"].as_array().unwrap().len(),
        1,
        "one session, one run"
    );
    let run_id = shown["runs"][0]["id"].as_str().unwrap();
    let log = std::fs::read_to_string(
        home.path()
            .join("kanban/boards/default/logs")
            .join(format!("{id}-{run_id}.log")),
    )
    .unwrap();
    assert!(
        log.contains("kanban goal loop: turn 1/20 verdict=continue"),
        "{log}"
    );
    assert!(
        log.contains("kanban goal loop: turn 2/20 verdict=continue"),
        "{log}"
    );
    let session = run_session(home.path(), &id);
    let users = common::session_messages(home.path(), &session)
        .into_iter()
        .filter(|m| m["role"] == "user" && m.to_string().contains("goal continuation"))
        .count();
    assert_eq!(users, 2, "two continuations in the same session");
}

/// Scenario: Impossible card is never done.
#[test]
fn an_impossible_card_is_blocked_and_never_done() {
    let rules = "[[rule]]\nuser_regex = 'Kanban task'\ntext = \"There is no repository to read.\"\n\n\
        [[rule]]\nuser_regex = 'goal judge'\ntext = '{\"verdict\": \"unachievable\", \"reason\": \"source repo does not exist\"}'\n\n\
        [[rule]]\nrepeat = true\ntext = \"ok\"\n";
    let home = worker_home(rules);
    let id = create(
        home.path(),
        &["port the source repo", "--assignee", "default", "--goal"],
    );
    assert_eq!(work_until_settled(home.path(), &id), "blocked");
    assert_eq!(
        task(home.path(), &id)["task"]["block_reason"],
        "source repo does not exist"
    );
    let out = run(
        home.path(),
        &["kanban", "complete", &id, "--result", "done anyway"],
    );
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stdout(&out).contains("goal judged unachievable"),
        "{}",
        stdout(&out)
    );
    assert_eq!(status(home.path(), &id), "blocked");
}

/// Scenario: Hard card on a stronger model.
#[test]
fn a_hard_card_runs_on_its_own_model() {
    let rules = "[[rule]]\nuser_regex = 'Kanban task'\n[[rule.tool_calls]]\nname = \"kanban_complete\"\n\
        arguments = { result = \"done on the big model\" }\n\n[[rule]]\nrepeat = true\ntext = \"finished\"\n";
    let home = worker_home(rules);
    let id = create(
        home.path(),
        &[
            "hard card",
            "--assignee",
            "default",
            "--model",
            "big-model",
            "--provider",
            "mock",
        ],
    );
    assert_eq!(work_until_settled(home.path(), &id), "done");
    let session = run_session(home.path(), &id);
    let shown = json(home.path(), &["sessions", "show", &session]);
    assert!(shown.to_string().contains("big-model"), "{shown}");
    // set-model clears it for the next dispatch; --provider needs a model.
    ok(&run(home.path(), &["kanban", "set-model", &id, "none"]));
    assert!(task(home.path(), &id)["task"]["model"].is_null());
    let out = run(
        home.path(),
        &["kanban", "create", "x", "--provider", "mock"],
    );
    assert_eq!(out.status.code(), Some(2), "clap refuses --provider alone");
}

/// Pinned skills are checked at creation and again at spawn.
#[test]
fn pinned_skills_are_checked_at_create_and_at_spawn() {
    let home = worker_home("[[rule]]\nrepeat = true\ntext = \"hi\"\n");
    let out = run(
        home.path(),
        &[
            "kanban",
            "create",
            "x",
            "--assignee",
            "default",
            "--skill",
            "nope",
        ],
    );
    assert_eq!(out.status.code(), Some(65));
    assert!(
        stderr(&out).contains("unknown skill 'nope' for profile 'default'"),
        "{}",
        stderr(&out)
    );
    let skill = home
        .path()
        .join("profiles/default/skills/release-notes/SKILL.md");
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(
        &skill,
        "---\nname: release-notes\ndescription: Write release notes.\n---\nGroup changes by area.\n",
    )
    .unwrap();
    let id = create(
        home.path(),
        &["notes", "--assignee", "default", "--skill", "release-notes"],
    );
    std::fs::remove_dir_all(skill.parent().unwrap()).unwrap();
    assert_eq!(work_until_settled(home.path(), &id), "blocked");
    let shown = task(home.path(), &id);
    assert_eq!(shown["task"]["block_kind"], "capability");
    assert!(
        shown["task"]["block_reason"]
            .as_str()
            .unwrap()
            .contains("release-notes")
    );
}

/// Scenario: Fresh PR is not re-worked.
#[test]
fn a_fresh_pr_comment_is_guarded_in_the_dry_run() {
    let home = worker_home("[[rule]]\nrepeat = true\ntext = \"hi\"\n");
    let id = create(home.path(), &["follow up", "--assignee", "default"]);
    ok(&run(
        home.path(),
        &[
            "kanban",
            "comment",
            &id,
            "opened https://github.com/acme/api/pull/41",
        ],
    ));
    let dry = ok(&run(home.path(), &["kanban", "dispatch", "--dry-run"]));
    assert!(dry.contains(&format!("Guarded (active_pr): {id}")), "{dry}");
    ok(&run(home.path(), &["kanban", "dispatch", "--once"]));
    let shown = task(home.path(), &id);
    assert_eq!(shown["task"]["status"], "ready");
    assert_eq!(shown["runs"], Value::Array(vec![]), "no worker spawned");
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Scenario: Branch convention (and the project's board is selected by
/// default).
#[test]
fn a_bound_projects_board_is_selected_and_names_the_branch() {
    let rules = "[[rule]]\nuser_regex = 'Kanban task'\n[[rule.tool_calls]]\nname = \"kanban_complete\"\n\
        arguments = { result = \"fixed\" }\n\n[[rule]]\nrepeat = true\ntext = \"finished\"\n";
    let home = worker_home(rules);
    let repo = common::user_home(home.path()).join("api");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "user.email", "t@example.invalid"]);
    std::fs::write(repo.join("README.md"), "api\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let out = run(
        home.path(),
        &[
            "projects",
            "create",
            "api",
            repo.to_str().unwrap(),
            "--board",
            "backend",
        ],
    );
    assert_eq!(out.status.code(), Some(66), "binding a missing board fails");
    assert!(
        stderr(&out).contains("board 'backend' not found"),
        "{}",
        stderr(&out)
    );
    ok(&run(
        home.path(),
        &["kanban", "boards", "create", "backend"],
    ));
    ok(&run(
        home.path(),
        &[
            "projects",
            "create",
            "api",
            repo.to_str().unwrap(),
            "--board",
            "backend",
            "--use",
        ],
    ));
    let created = json(
        home.path(),
        &[
            "kanban",
            "create",
            "Fix Login Bug",
            "--workspace",
            "worktree",
            "--assignee",
            "default",
        ],
    );
    assert_eq!(
        created["board"], "backend",
        "the project's board is the default"
    );
    let id = created["id"].as_str().unwrap().to_owned();
    let shown = json(home.path(), &["kanban", "--board", "backend", "show", &id]);
    let branch = format!("api/{id}-fix-login-bug");
    let text = ok(&run(
        home.path(),
        &["kanban", "--board", "backend", "show", &id],
    ));
    assert!(text.contains(&format!("branch: {branch}")), "{text}");
    assert_eq!(shown["task"]["project"], "api");
    // The worker's worktree is of the project's primary folder, on that
    // branch.
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        ok(&run(home.path(), &["kanban", "dispatch", "--once"]));
        let now =
            json(home.path(), &["kanban", "--board", "backend", "show", &id])["task"]["status"]
                .as_str()
                .unwrap()
                .to_owned();
        if now == "done" {
            break;
        }
        assert!(Instant::now() < deadline, "stayed {now}");
        std::thread::sleep(Duration::from_millis(300));
    }
    let branches = git(&repo, &["branch", "--list", &branch]);
    assert!(branches.contains(&branch), "{branches}");
}
