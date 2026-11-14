//! `prometheus kanban` and its workers against the mock provider (spec:
//! kanban: Board storage location; Board slugs and selection; CLI
//! surface; Board export and import; Attachments; Worker spawn by run
//! ticket; Kanban toolset and grants; Event log and run history;
//! Per-task cost budget; Eval and harness-evolution tasks).

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{mock_home, ok, run, run_with_stdin, stderr, stdout};
use serde_json::Value;

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
fn worker_home(rules: &str) -> tempfile::TempDir {
    let home = mock_home();
    std::fs::write(home.path().join("profiles/default/mock.toml"), rules).unwrap();
    common::append_config(home.path(), "\n[providers.mock]\nscript = \"mock.toml\"\n");
    home
}

/// Ticks the dispatcher until `id` leaves `running` (the worker ran and
/// was reaped), or panics after a minute.
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

/// The first run's session id, once the worker recorded it (it records
/// after its last turn, which may end after the task moved).
fn run_session(home: &Path, id: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(session) = task(home, id)["runs"][0]["session_id"].as_str() {
            return session.to_owned();
        }
        assert!(
            Instant::now() < deadline,
            "no session recorded: {}",
            task(home, id)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Scenario: Default board exists on first use.
#[test]
fn the_default_board_exists_on_first_use() {
    let home = common::temp_home();
    let out = ok(&run(home.path(), &["kanban", "list"]));
    assert_eq!(out, "board 'default' is empty\n");
    assert!(home.path().join("kanban/boards/default/board.db").is_file());
}

/// Scenario: Stale selection is reported.
#[test]
fn a_stale_selection_exits_66() {
    let home = common::temp_home();
    ok(&run(
        home.path(),
        &["config", "set", "kanban.current_board", "atm10"],
    ));
    let out = run(home.path(), &["kanban", "list"]);
    assert_eq!(out.status.code(), Some(66));
    assert!(
        stderr(&out).contains("board 'atm10' not found (set kanban.current_board or pass --board)")
    );
    ok(&run(home.path(), &["kanban", "boards", "create", "atm10"]));
    assert_eq!(
        ok(&run(home.path(), &["kanban", "list"])),
        "board 'atm10' is empty\n"
    );
}

/// Scenario: Bulk archive with one failure.
#[test]
fn a_bulk_archive_reports_each_id_and_exits_1() {
    let home = common::temp_home();
    let a = create(home.path(), &["a"]);
    let b = create(home.path(), &["b"]);
    let out = run(home.path(), &["kanban", "archive", &a, &b, "t_zz"]);
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(text.contains(&format!("ok {a} archived")), "{text}");
    assert!(text.contains(&format!("ok {b} archived")), "{text}");
    assert!(text.contains("failed t_zz: not found"), "{text}");
}

/// Scenario: Round trip.
#[test]
fn an_exported_board_imports_as_a_copy() {
    let home = common::temp_home();
    let parent = create(home.path(), &["parent"]);
    let child = create(home.path(), &["child", "--parent", &parent]);
    let file = home.path().join("note.txt");
    std::fs::write(&file, "attached words").unwrap();
    ok(&run(
        home.path(),
        &["kanban", "attach", &child, file.to_str().unwrap()],
    ));
    let archive = home.path().join("board.json");
    ok(&run(
        home.path(),
        &[
            "kanban",
            "boards",
            "export",
            "default",
            archive.to_str().unwrap(),
        ],
    ));
    let refused = run(
        home.path(),
        &["kanban", "boards", "import", archive.to_str().unwrap()],
    );
    assert_eq!(refused.status.code(), Some(65), "{}", stderr(&refused));
    let imported = json(
        home.path(),
        &[
            "kanban",
            "boards",
            "import",
            archive.to_str().unwrap(),
            "--rename",
            "copy",
        ],
    );
    // The copy lives in the same home: its colliding ids were remapped.
    let new_id = |old: &str| imported["remapped"][old].as_str().unwrap().to_owned();
    let (parent, child) = (new_id(&parent), new_id(&child));
    let listed = json(home.path(), &["kanban", "--board", "copy", "list"]);
    let titles: Vec<&str> = listed["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles.len(), 2);
    assert!(titles.contains(&"parent") && titles.contains(&"child"));
    let copied = json(home.path(), &["kanban", "--board", "copy", "show", &child]);
    assert_eq!(copied["parents"][0], parent.as_str());
    let attachment = &copied["attachments"][0];
    assert_eq!(attachment["filename"], "note.txt");
    let rows = json(
        home.path(),
        &["kanban", "--board", "copy", "attachments", &child],
    );
    let path = rows["attachments"][0]["path"].as_str().unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), "attached words");
}

/// Scenario: Oversized upload refused (the CLI surface).
#[test]
fn an_oversized_attachment_is_refused_and_nothing_stored() {
    let home = common::temp_home();
    let id = create(home.path(), &["big"]);
    let file = home.path().join("big.bin");
    std::fs::File::create(&file)
        .unwrap()
        .set_len(26 * 1024 * 1024)
        .unwrap();
    let out = run(
        home.path(),
        &["kanban", "attach", &id, file.to_str().unwrap()],
    );
    assert_eq!(out.status.code(), Some(65));
    assert!(
        stderr(&out).contains("attachment too large: 27262976 > 26214400"),
        "{}",
        stderr(&out)
    );
    assert_eq!(
        json(home.path(), &["kanban", "attachments", &id])["attachments"],
        Value::Array(vec![])
    );
    let blobs = home.path().join("kanban/blobs");
    assert_eq!(common::files_under(&blobs).len(), 0);
}

/// Scenario: Child of an open parent waits; Relative dir workspace
/// rejected at creation.
#[test]
fn creation_from_the_cli_validates_and_gates() {
    let home = common::temp_home();
    let out = run(
        home.path(),
        &["kanban", "create", "x", "--workspace", "dir:../tenants/foo"],
    );
    assert_eq!(out.status.code(), Some(65));
    assert!(stderr(&out).contains("workspace dir must be an absolute path"));
    assert_eq!(
        json(home.path(), &["kanban", "list"])["tasks"],
        Value::Array(vec![])
    );
    let parent = create(home.path(), &["parent"]);
    let child = create(home.path(), &["child", "--parent", &parent]);
    assert_eq!(status(home.path(), &child), "todo");
    let log = json(home.path(), &["kanban", "log", &child]);
    assert!(
        log["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["kind"] == "dependency_wait")
    );
}

/// Scenario: Audit a refusal.
#[test]
fn a_refused_completion_is_in_the_log() {
    let home = common::temp_home();
    let id = create(home.path(), &["report"]);
    let out = run(home.path(), &["kanban", "complete", &id, "--result", "   "]);
    assert_eq!(out.status.code(), Some(1));
    assert!(stdout(&out).contains("has no result or summary evidence"));
    let log = ok(&run(home.path(), &["kanban", "log", &id]));
    assert!(log.contains("completion_refused"), "{log}");
    assert!(log.contains("has no result or summary evidence"), "{log}");
}

const COMPLETES: &str = "[[rule]]\nuser_regex = 'Kanban task'\n[[rule.tool_calls]]\nname = \"kanban_complete\"\n\
    arguments = { result = \"report written\" }\n\n[[rule]]\ntool_result_regex = 'status'\ntext = \"finished\"\n";

/// A worker drives its task to done through the kanban tools; the run
/// records its session.
#[test]
fn a_dispatched_worker_completes_its_task() {
    let home = worker_home(COMPLETES);
    let id = create(home.path(), &["write the report", "--assignee", "default"]);
    assert_eq!(work_until_settled(home.path(), &id), "done");
    let session = run_session(home.path(), &id);
    let session = session.as_str();
    let shown = task(home.path(), &id);
    assert_eq!(shown["task"]["result"], "report written");
    assert_eq!(shown["runs"][0]["outcome"], "completed");
    let messages = common::session_messages(home.path(), session);
    assert!(
        messages
            .iter()
            .any(|m| m.to_string().contains("kanban_complete"))
    );
    let runs = ok(&run(home.path(), &["kanban", "runs", &id]));
    assert!(runs.contains(&format!("sessions show {session}")), "{runs}");
    assert!(
        !home
            .path()
            .join("kanban/boards/default/workspaces")
            .join(&id)
            .exists()
    );
}

/// Scenario: Worker cannot touch another board.
#[test]
fn a_worker_cannot_touch_another_board() {
    let rules = "[[rule]]\nuser_regex = 'Kanban task'\n[[rule.tool_calls]]\nname = \"kanban_create\"\n\
        arguments = { board = \"beta\", title = \"sneaky\" }\n\n[[rule]]\ntool_result_regex = 'capability denied'\n\
        [[rule.tool_calls]]\nname = \"kanban_block\"\narguments = { kind = \"needs_input\", reason = \"saw the denial\" }\n\n\
        [[rule]]\nrepeat = true\ntext = \"stopped\"\n";
    let home = worker_home(rules);
    ok(&run(home.path(), &["kanban", "boards", "create", "beta"]));
    let id = create(home.path(), &["stay home", "--assignee", "default"]);
    assert_eq!(work_until_settled(home.path(), &id), "blocked");
    let session = run_session(home.path(), &id);
    let messages = common::session_messages(home.path(), &session);
    assert!(
        messages.iter().any(|m| m
            .to_string()
            .contains("capability denied: kanban board 'beta'")),
        "{messages:?}"
    );
    assert_eq!(
        json(home.path(), &["kanban", "--board", "beta", "list"])["tasks"],
        Value::Array(vec![])
    );
}

/// Scenario: Replayed ticket refused.
#[test]
fn a_replayed_ticket_is_refused() {
    let home = worker_home(COMPLETES);
    let id = create(home.path(), &["once only", "--assignee", "default"]);
    let boards = pmt_kanban::Boards::open(home.path()).unwrap();
    let mut board = boards.board("default").unwrap();
    let claim = match board
        .claim(
            &pmt_kanban::Actor::user("test"),
            &id,
            "default",
            pmt_kanban::Role::Worker,
            900_000,
            pmt_kanban::now_ms(),
        )
        .unwrap()
    {
        pmt_kanban::ClaimOutcome::Claimed(claim) => claim,
        pmt_kanban::ClaimOutcome::Rejected(_) => unreachable!(),
    };
    let ticket = pmt_kanban::ticket::Ticket {
        v: 1,
        board: "default".into(),
        run: claim.run_id.clone(),
        secret: claim.secret.clone(),
    }
    .line();
    let first = run_with_stdin(
        home.path(),
        &["kanban", "work", "--run", &claim.run_id],
        &ticket,
    );
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    assert_eq!(status(home.path(), &id), "done");
    let again = run_with_stdin(
        home.path(),
        &["kanban", "work", "--run", &claim.run_id],
        &ticket,
    );
    assert_eq!(again.status.code(), Some(78));
    assert!(
        stderr(&again).contains("run ticket already used"),
        "{}",
        stderr(&again)
    );
    let missing = run_with_stdin(home.path(), &["kanban", "work", "--run", &claim.run_id], "");
    assert_eq!(missing.status.code(), Some(78));
}

/// Scenario: Launch profile secrets do not leak.
#[test]
fn a_worker_never_sees_the_dispatchers_secrets() {
    let home = common::temp_home();
    ok(&run(
        home.path(),
        &["profile", "create", "ops", "--no-alias"],
    ));
    ok(&run(
        home.path(),
        &["profile", "create", "writer", "--no-alias"],
    ));
    for (key, value) in [
        ("model.provider", "openai"),
        ("model.default", "gpt-4o-mini"),
    ] {
        ok(&run(
            home.path(),
            &["-p", "writer", "config", "set", key, value],
        ));
    }
    let id = create(home.path(), &["writer's work", "--assignee", "writer"]);
    let deadline = Instant::now() + Duration::from_mins(1);
    loop {
        let out = common::command(home.path())
            .args(["-p", "ops", "kanban", "dispatch", "--once"])
            .env("OPENAI_API_KEY", "sk-ops-launch-secret-0123456789")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        ok(&out);
        let now = status(home.path(), &id);
        if now == "blocked" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "task stayed {now}: {}",
            task(home.path(), &id)
        );
        std::thread::sleep(Duration::from_millis(300));
    }
    let shown = task(home.path(), &id);
    let failure = shown["task"]["last_failure"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        failure.to_lowercase().contains("openai") || failure.contains("OPENAI_API_KEY"),
        "{shown}"
    );
    assert_eq!(shown["runs"][0]["exit_code"], 78, "{shown}");
    let log = ok(&run(home.path(), &["kanban", "log", &id]));
    assert!(log.contains("gave_up"), "{log}");
    let worker_log = std::fs::read_to_string(home.path().join("kanban/boards/default/logs").join(
        format!("{id}-{}.log", shown["runs"][0]["id"].as_str().unwrap()),
    ))
    .unwrap_or_default();
    assert!(!worker_log.contains("sk-ops-launch-secret"), "{worker_log}");
}

/// Scenario: Budget exhausted.
#[test]
fn an_exhausted_budget_blocks_the_task() {
    // The mock provider prices nothing, so a token budget is what a
    // single answer exhausts.
    let rules = "[[rule]]\nrepeat = true\ntext = \"thinking about it at great length\"\n";
    let home = worker_home(rules);
    let id = create(
        home.path(),
        &[
            "expensive",
            "--assignee",
            "default",
            "--budget-tokens",
            "10",
        ],
    );
    assert_eq!(work_until_settled(home.path(), &id), "blocked");
    let shown = task(home.path(), &id);
    assert_eq!(shown["task"]["block_kind"], "budget", "{shown}");
    assert!(shown["spend"]["tokens"].as_u64().unwrap() >= 10, "{shown}");
    assert!(ok(&run(home.path(), &["kanban", "show", &id])).contains("spend: "));
}

/// The worker guidance and the kanban tools reach a worker session,
/// and `kanban context` prints the worker's first message.
#[test]
fn context_prints_what_a_worker_receives() {
    let home = common::temp_home();
    let parent = create(home.path(), &["gather data"]);
    ok(&run(
        home.path(),
        &["kanban", "complete", &parent, "--result", "42 rows"],
    ));
    let id = create(
        home.path(),
        &["summarise", "--parent", &parent, "--body", "One paragraph."],
    );
    ok(&run(
        home.path(),
        &["kanban", "comment", &id, "keep it short"],
    ));
    let context = ok(&run(home.path(), &["kanban", "context", &id]));
    assert!(context.contains("## summarise"), "{context}");
    assert!(context.contains("One paragraph."), "{context}");
    assert!(context.contains("42 rows"), "{context}");
    assert!(context.contains("keep it short"), "{context}");
    assert_eq!(status(home.path(), &id), "ready", "context does not claim");
}

/// Scenario: Delegated child has no board access.
#[test]
fn a_delegated_child_gets_no_kanban_tools() {
    let rules = r#"
[[rule]]
user_regex = 'Kanban task'
tools_regex = '"kanban_complete"'
[[rule.tool_calls]]
name = "delegate_task"
arguments = { tasks = [{ goal = "Summarise the README" }] }

[[rule]]
user = "Summarise the README"
tools_regex = '"kanban_'
text = "CHILD SAW KANBAN"

[[rule]]
user = "Summarise the README"
text = "child summary done"

[[rule]]
tool_result_regex = 'child summary done|CHILD SAW KANBAN'
[[rule.tool_calls]]
name = "kanban_complete"
arguments = { result = "delegated and done" }

[[rule]]
repeat = true
text = "finished"
"#;
    let home = worker_home(rules);
    common::append_config(
        home.path(),
        "\n[tools]\nenabled_toolsets = [\"file\", \"delegation\"]\n",
    );
    let id = create(home.path(), &["delegate", "--assignee", "default"]);
    assert_eq!(
        work_until_settled(home.path(), &id),
        "done",
        "{}",
        task(home.path(), &id)
    );
    let listed = json(home.path(), &["sessions", "list", "--all"]);
    let child = listed["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["source"] == "subagent")
        .expect("a delegated child ran")["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let messages = common::session_messages(home.path(), &child);
    let texts: Vec<String> = messages
        .iter()
        .filter_map(|m| m["content"].as_str().map(str::to_owned))
        .collect();
    assert!(texts.iter().any(|t| t == "child summary done"), "{texts:?}");
    assert!(
        !texts.iter().any(|t| t.contains("CHILD SAW KANBAN")),
        "{texts:?}"
    );
}

/// Scenario: Eval task on replayed provider.
#[test]
fn an_eval_task_runs_the_smoke_suite_offline_and_attaches_its_report() {
    let home = common::temp_home();
    let id = create(
        home.path(),
        &[
            "smoke eval",
            "--assignee",
            "default",
            "--kind",
            "eval",
            "--suite",
            "smoke",
            "--provider-mode",
            "replay",
        ],
    );
    let deadline = Instant::now() + Duration::from_mins(3);
    let mut now = status(home.path(), &id);
    while now != "done" && now != "blocked" {
        assert!(Instant::now() < deadline, "eval stayed {now}");
        ok(&run(home.path(), &["kanban", "dispatch", "--once"]));
        std::thread::sleep(Duration::from_millis(500));
        now = status(home.path(), &id);
    }
    let shown = task(home.path(), &id);
    assert_eq!(now, "done", "{shown}");
    assert!(
        shown["task"]["result"]
            .as_str()
            .unwrap()
            .starts_with("smoke: "),
        "{shown}"
    );
    assert_eq!(shown["attachments"][0]["filename"], "eval-report.txt");
    // No model call was made by the worker itself: no session, no spend.
    assert_eq!(shown["spend"]["tokens"], 0);
    assert!(shown["runs"][0]["session_id"].is_null(), "{shown}");
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

/// A worktree workspace is a git worktree of the profile's
/// `terminal.cwd` on `kanban/<task>`, preserved after completion.
#[test]
fn a_worktree_task_works_on_its_own_branch_and_keeps_it() {
    let home = worker_home(COMPLETES);
    let repo = common::user_home(home.path()).join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.name", "t"]);
    git(&repo, &["config", "user.email", "t@example.invalid"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    ok(&run(
        home.path(),
        &["config", "set", "terminal.cwd", repo.to_str().unwrap()],
    ));
    let id = create(
        home.path(),
        &[
            "on a branch",
            "--assignee",
            "default",
            "--workspace",
            "worktree",
        ],
    );
    assert_eq!(
        work_until_settled(home.path(), &id),
        "done",
        "{}",
        task(home.path(), &id)
    );
    let tree = repo.join(".worktrees").join(&id);
    assert!(
        tree.join("README.md").is_file(),
        "the worktree is preserved"
    );
    let branches = git(&repo, &["branch", "--list", &format!("kanban/{id}")]);
    assert!(branches.contains(&format!("kanban/{id}")), "{branches}");
}
