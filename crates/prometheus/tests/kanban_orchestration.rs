//! Kanban orchestration, worker lanes and lifecycle hooks through the CLI
//! on the mock provider (spec: kanban, Swarm topology helper; Orchestrator
//! guidance, specify and decompose; Additional CLI verbs; Subscription
//! delivery modes and inheritance; Worker lanes; plugin-system, Kanban
//! lifecycle observer hooks).

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{mock_home, ok, run, run_with_stdin, stderr};
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
    json(home, &["kanban", "show", id])["task"].clone()
}

fn tasks(home: &Path) -> Vec<Value> {
    json(home, &["kanban", "list"])["tasks"]
        .as_array()
        .unwrap()
        .clone()
}

fn kinds(home: &Path, id: &str) -> Vec<String> {
    json(home, &["kanban", "log", id])["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_owned())
        .collect()
}

fn worker_home(rules: &str) -> tempfile::TempDir {
    let home = mock_home();
    std::fs::write(home.path().join("profiles/default/mock.toml"), rules).unwrap();
    common::append_config(home.path(), "\n[providers.mock]\nscript = \"mock.toml\"\n");
    home
}

fn profile(home: &Path, name: &str) {
    ok(&run(home, &["profile", "create", name, "--no-alias"]));
    for (key, value) in [("model.provider", "mock"), ("model.default", "mock")] {
        ok(&run(home, &["-p", name, "config", "set", key, value]));
    }
}

/// Scenario: Atomic swarm creation.
#[test]
fn a_swarm_is_six_cards_with_the_verifier_gated_on_the_workers() {
    let home = mock_home();
    for name in ["researcher", "architect", "sre", "reviewer", "writer"] {
        profile(home.path(), name);
    }
    let swarm = json(
        home.path(),
        &[
            "kanban",
            "swarm",
            "failover plan",
            "--workers",
            "researcher,architect,sre",
            "--verifier",
            "reviewer",
            "--synthesizer",
            "writer",
        ],
    );
    assert_eq!(tasks(home.path()).len(), 6, "{swarm}");
    let verifier = swarm["verifier"].as_str().unwrap();
    assert_eq!(task(home.path(), verifier)["status"], "todo");
    for worker in swarm["workers"].as_array().unwrap() {
        assert_eq!(
            task(home.path(), worker.as_str().unwrap())["status"],
            "ready"
        );
    }
    let root = swarm["root"].as_str().unwrap();
    assert_eq!(task(home.path(), root)["status"], "done");
    let shown = json(home.path(), &["kanban", "show", root]);
    assert!(
        shown["comments"][0]["body"]
            .as_str()
            .unwrap()
            .contains("\"entries\"")
    );
}

/// Scenario: Unknown worker profile aborts everything.
#[test]
fn an_unknown_swarm_profile_creates_nothing() {
    let home = mock_home();
    for name in ["researcher", "architect", "reviewer", "writer"] {
        profile(home.path(), name);
    }
    let out = run(
        home.path(),
        &[
            "kanban",
            "swarm",
            "failover plan",
            "--workers",
            "researcher,architect,ghost",
            "--verifier",
            "reviewer",
            "--synthesizer",
            "writer",
        ],
    );
    assert_eq!(out.status.code(), Some(66));
    assert!(
        stderr(&out).contains("unknown profile 'ghost'"),
        "{}",
        stderr(&out)
    );
    assert!(
        json(home.path(), &["kanban", "list", "--all"])["tasks"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

/// Scenario: Specify promotes a triage idea.
#[test]
fn specify_expands_a_triage_idea_into_a_todo_task() {
    let rules = "[[rule]]\nuser_regex = 'add rate limiting'\ntext = \"Limit each client to 10 requests per second.\\n\\n\
        Acceptance criteria:\\n- requests above the limit get HTTP 429\"\n\n[[rule]]\nrepeat = true\ntext = \"hi\"\n";
    let home = worker_home(rules);
    let id = create(home.path(), &["add rate limiting", "--status", "triage"]);
    let out = ok(&run(home.path(), &["kanban", "specify", &id]));
    assert!(out.contains(&format!("ok {id} todo")), "{out}");
    let shown = task(home.path(), &id);
    assert_eq!(shown["status"], "todo");
    assert!(
        shown["body"]
            .as_str()
            .unwrap()
            .contains("Acceptance criteria"),
        "{shown}"
    );
}

/// `kanban.orchestrator_profile` (the dashboard's orchestration settings):
/// a specified triage idea with no assignee goes to the orchestrator
/// profile, and one that names an assignee keeps it.
#[test]
fn a_specified_idea_without_an_assignee_goes_to_the_orchestrator_profile() {
    let rules = "[[rule]]\nrepeat = true\ntext = \"Do it.\\n\\nAcceptance criteria:\\n- done\"\n";
    let home = worker_home(rules);
    profile(home.path(), "planner");
    ok(&run(
        home.path(),
        &["config", "set", "kanban.orchestrator_profile", "planner"],
    ));
    let open = create(home.path(), &["plan the launch", "--status", "triage"]);
    let owned = create(
        home.path(),
        &[
            "fix the logo",
            "--status",
            "triage",
            "--assignee",
            "default",
        ],
    );
    ok(&run(home.path(), &["kanban", "specify", &open]));
    ok(&run(home.path(), &["kanban", "specify", &owned]));
    assert_eq!(task(home.path(), &open)["assignee"], "planner");
    assert_eq!(task(home.path(), &owned)["assignee"], "default");
}

/// Scenario: Decompose rejects unknown assignees.
#[test]
fn decompose_refuses_a_graph_naming_an_unknown_assignee() {
    let rules = "[[rule]]\nuser_regex = 'migrate the billing'\ntext = '{\"tasks\": [{\"key\": \"a\", \"title\": \"plan\", \"assignee\": \"default\"}, \
        {\"key\": \"b\", \"title\": \"build\", \"assignee\": \"nobody\", \"depends_on\": [\"a\"]}]}'\n\n\
        [[rule]]\nuser_regex = 'ship the docs'\ntext = '{\"tasks\": [{\"key\": \"a\", \"title\": \"outline\", \"assignee\": \"default\"}, \
        {\"key\": \"b\", \"title\": \"write\", \"assignee\": \"default\", \"depends_on\": [\"a\"]}]}'\n\n\
        [[rule]]\nrepeat = true\ntext = \"hi\"\n";
    let home = worker_home(rules);
    let id = create(home.path(), &["migrate the billing"]);
    let out = run(home.path(), &["kanban", "decompose", &id]);
    assert_eq!(out.status.code(), Some(65));
    assert!(stderr(&out).contains("nobody"), "{}", stderr(&out));
    assert_eq!(tasks(home.path()).len(), 1, "no child was created");
    let docs = create(home.path(), &["ship the docs"]);
    let graph = json(home.path(), &["kanban", "decompose", &docs]);
    let a = graph["children"]["a"].as_str().unwrap();
    let b = graph["children"]["b"].as_str().unwrap();
    assert_eq!(
        task(home.path(), a)["status"],
        "ready",
        "auto_promote_children"
    );
    assert_eq!(task(home.path(), b)["status"], "todo");
    assert_eq!(
        task(home.path(), &docs)["status"],
        "todo",
        "the goal waits for its children"
    );
}

/// `kanban.auto_decompose`: the dispatcher specifies triage ideas itself.
#[test]
fn auto_decompose_specifies_triage_tasks_before_dispatching() {
    let rules = "[[rule]]\nuser_regex = 'cache the index'\ntext = \"Cache it.\\n\\nAcceptance criteria:\\n- a second read hits the cache\"\n\n\
        [[rule]]\nrepeat = true\ntext = \"hi\"\n";
    let home = worker_home(rules);
    ok(&run(
        home.path(),
        &["config", "set", "kanban.auto_decompose", "true"],
    ));
    let id = create(home.path(), &["cache the index", "--status", "triage"]);
    ok(&run(home.path(), &["kanban", "dispatch", "--once"]));
    let shown = task(home.path(), &id);
    assert_ne!(shown["status"], "triage");
    assert!(
        shown["body"]
            .as_str()
            .unwrap()
            .contains("Acceptance criteria")
    );
}

/// An orchestrator profile's worker gets the orchestrator guidance in its
/// frozen system prompt.
#[test]
fn an_orchestrators_worker_is_told_to_decompose_not_implement() {
    let rules = "[[rule]]\nuser_regex = 'Kanban task'\n[[rule.tool_calls]]\nname = \"kanban_complete\"\n\
        arguments = { result = \"planned\" }\n\n[[rule]]\nrepeat = true\ntext = \"finished\"\n";
    let home = worker_home(rules);
    ok(&run(
        home.path(),
        &["config", "set", "kanban.role", "orchestrator"],
    ));
    let id = create(home.path(), &["plan the launch", "--assignee", "default"]);
    let deadline = Instant::now() + Duration::from_mins(1);
    while task(home.path(), &id)["status"] != "done" {
        assert!(Instant::now() < deadline, "{}", task(home.path(), &id));
        ok(&run(home.path(), &["kanban", "dispatch", "--once"]));
        std::thread::sleep(Duration::from_millis(300));
    }
    let session: pmt_core::id::SessionId = loop {
        // The worker records its session after its last turn.
        if let Some(s) =
            json(home.path(), &["kanban", "show", &id])["runs"][0]["session_id"].as_str()
        {
            break s.parse().unwrap();
        }
        assert!(Instant::now() < deadline, "no session recorded");
        std::thread::sleep(Duration::from_millis(100));
    };
    let layout = pmt_config::Home::open(home.path()).unwrap();
    let dir = layout
        .existing_profile(&pmt_config::ProfileName::parse("default").unwrap())
        .unwrap();
    let scope = pmt_config::ProfileScope::open(
        dir,
        &pmt_config::Overrides::default(),
        pmt_config::Launch::Other,
    )
    .unwrap();
    let prompt = pmt_store::Store::open(&scope)
        .unwrap()
        .session_prompt(session)
        .unwrap()
        .expect("a stored prompt");
    let text: String = prompt.tiers.into_iter().map(|t| t.text).collect();
    assert!(text.contains("## Kanban worker"), "{text}");
    assert!(text.contains("## Kanban orchestrator"), "{text}");
}

/// Scenario: Inspect worker context (with the other additional verbs:
/// init, assignees, claim, heartbeat, reassign, reopen-review).
#[test]
fn the_additional_verbs_go_through_the_board() {
    let home = mock_home();
    profile(home.path(), "qa");
    let init = ok(&run(home.path(), &["kanban", "init"]));
    assert!(
        init.contains("board default") && init.contains("dispatcher:"),
        "{init}"
    );
    let assignees = json(home.path(), &["kanban", "assignees"]);
    assert!(
        assignees["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "qa")
    );
    let id = create(home.path(), &["review me", "--assignee", "default"]);
    let context = ok(&run(home.path(), &["kanban", "context", &id]));
    assert!(context.contains("review me"));
    assert_eq!(
        task(home.path(), &id)["status"],
        "ready",
        "context claims nothing"
    );
    let ticket = json(home.path(), &["kanban", "claim", &id])["ticket"]
        .as_str()
        .unwrap()
        .to_owned();
    ok(&run_with_stdin(
        home.path(),
        &[
            "kanban",
            "heartbeat",
            &id,
            "--ticket",
            "-",
            "--note",
            "halfway",
        ],
        &format!("{ticket}\n"),
    ));
    ok(&run_with_stdin(
        home.path(),
        &[
            "kanban",
            "request-review",
            &id,
            "--ticket",
            "-",
            "--summary",
            "please look",
        ],
        &format!("{ticket}\n"),
    ));
    let replayed = run_with_stdin(
        home.path(),
        &["kanban", "comment", &id, "late", "--ticket", "-"],
        &format!("{ticket}\n"),
    );
    assert_eq!(
        replayed.status.code(),
        Some(78),
        "the run ended: its ticket is void"
    );
    ok(&run(
        home.path(),
        &["kanban", "reopen-review", &id, "--reason", "not yet"],
    ));
    assert_eq!(task(home.path(), &id)["status"], "ready");
    let out = ok(&run(home.path(), &["kanban", "reassign", &id, "qa"]));
    assert!(out.contains(&format!("ok {id} qa")), "{out}");
    let log = kinds(home.path(), &id);
    for kind in [
        "claimed",
        "heartbeat",
        "review_requested",
        "review_reopened",
        "assigned",
    ] {
        assert!(log.iter().any(|k| k == kind), "{kind} missing from {log:?}");
    }
}

/// `notify subscribe` records the delivery mode and chat type a wake uses.
#[test]
fn a_subscription_records_its_delivery_mode_and_chat_type() {
    let home = mock_home();
    let id = create(home.path(), &["watched"]);
    let sub = ok(&run(
        home.path(),
        &[
            "kanban",
            "notify",
            "subscribe",
            &id,
            "--adapter",
            "discord",
            "--chat",
            "c1",
            "--thread",
            "t9",
            "--delivery-mode",
            "notify+wake",
            "--chat-type",
            "thread",
        ],
    ));
    assert!(sub.contains("notify+wake"), "{sub}");
    let subs = json(home.path(), &["kanban", "notify", "list", &id]);
    assert_eq!(subs["subscriptions"][0]["chat_type"], "thread");
}

/// Writes a data-only plugin `id` declaring `lanes` (TOML tables) with
/// `files`, recorded as installed.
fn lane_plugin(home: &Path, id: &str, lanes: &str, files: &[(&str, &str)]) {
    let dir = home.join("profiles/default/plugins").join(id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            "manifest_version = 1\nid = \"{id}\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
             description = \"lanes\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\nworlds = []\n\n{lanes}"
        ),
    )
    .unwrap();
    for (name, text) in files {
        std::fs::write(dir.join(name), text).unwrap();
    }
    pmt_plugins::record::InstallRecord {
        source: pmt_plugins::record::Origin::Adopted,
        location: dir.to_string_lossy().into_owned(),
        r#ref: None,
        package_digest: None,
        digest: pmt_plugins::digest::of_dir(&dir).unwrap(),
        signer: None,
        tier: None,
        installed_at: "2026-11-03T00:00:00Z".to_owned(),
        flags: pmt_plugins::record::Flags::default(),
        held_update: None,
    }
    .write(&dir)
    .unwrap();
}

/// The lane script: reads its ticket, then completes its task through the
/// CLI with it.
const LANE: &str = "read -r TICKET\n\
    printf '%s\\n' \"$TICKET\" | prometheus kanban complete \"$PROMETHEUS_KANBAN_TASK\" --ticket - \
    --result \"lane did it in $(basename \"$PWD\")\"\n";

/// Every lane log of the default board (for a failure message).
fn lane_logs(home: &Path) -> String {
    common::files_under(&home.join("kanban/boards/default/logs"))
        .iter()
        .map(|f| std::fs::read_to_string(f).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n---\n")
}

/// Scenario: External CLI lane completes a task (and Unknown assignee is
/// surfaced once its plugin is disabled).
#[cfg(unix)]
#[test]
fn an_external_cli_lane_completes_its_task_under_the_sandbox() {
    let home = mock_home();
    lane_plugin(
        home.path(),
        "codex",
        "[[kanban_lanes]]\nname = \"codex-cli\"\ncommand = [\"sh\", \"lane.sh\"]\ndescription = \"Codex CLI\"\n",
        &[("lane.sh", LANE)],
    );
    ok(&run(
        home.path(),
        &["plugins", "enable", "codex", "--grant", "all"],
    ));
    let assignees = json(home.path(), &["kanban", "assignees"]);
    assert_eq!(assignees["lanes"][0]["name"], "codex-cli", "{assignees}");
    let id = create(home.path(), &["port the parser", "--assignee", "codex-cli"]);
    let bin = Path::new(env!("CARGO_BIN_EXE_prometheus"))
        .parent()
        .unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let deadline = Instant::now() + Duration::from_mins(1);
    while task(home.path(), &id)["status"] != "done" {
        assert!(
            Instant::now() < deadline,
            "{}\n{:?}\n{}",
            task(home.path(), &id),
            kinds(home.path(), &id),
            lane_logs(home.path())
        );
        let out = common::command(home.path())
            .args(["kanban", "dispatch", "--once"])
            .env("PATH", &path)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
        std::thread::sleep(Duration::from_millis(300));
    }
    assert_eq!(
        task(home.path(), &id)["result"],
        format!("lane did it in {id}"),
        "it ran in the task's workspace"
    );
    // Its plugin disabled, a lane task stays ready and is surfaced once.
    let next = create(home.path(), &["port the lexer", "--assignee", "codex-cli"]);
    ok(&run(home.path(), &["plugins", "disable", "codex"]));
    ok(&run(home.path(), &["kanban", "dispatch", "--once"]));
    ok(&run(home.path(), &["kanban", "dispatch", "--once"]));
    assert_eq!(task(home.path(), &next)["status"], "ready");
    let skipped = kinds(home.path(), &next)
        .iter()
        .filter(|k| *k == "skipped_nonspawnable")
        .count();
    assert_eq!(skipped, 1);
}

/// A lane that tries to open its board's database for writing, then
/// attaches a report and completes through its broker.
const PROBING_LANE: &str = "read -r TICKET\n\
    if : >> \"$PROMETHEUS_HOME/kanban/boards/default/board.db\" 2>/dev/null; then W=writable; else W=denied; fi\n\
    printf 'quarterly' > report.txt\n\
    printf '%s\\n' \"$TICKET\" | prometheus kanban attach \"$PROMETHEUS_KANBAN_TASK\" report.txt --ticket -\n\
    printf '%s\\n' \"$TICKET\" | prometheus kanban complete \"$PROMETHEUS_KANBAN_TASK\" --ticket - --result \"db $W\"\n";

/// A lane cannot write its board's database: its sandbox grants only its
/// mailbox, and its reports (an attachment, the completion) reach the
/// board through its broker.
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn a_lane_cannot_write_the_board_and_reports_through_its_broker() {
    let home = mock_home();
    lane_plugin(
        home.path(),
        "prober",
        "[[kanban_lanes]]\nname = \"probe-cli\"\ncommand = [\"sh\", \"lane.sh\"]\n",
        &[("lane.sh", PROBING_LANE)],
    );
    ok(&run(
        home.path(),
        &["plugins", "enable", "prober", "--grant", "all"],
    ));
    let id = create(home.path(), &["probe the board", "--assignee", "probe-cli"]);
    let bin = Path::new(env!("CARGO_BIN_EXE_prometheus"))
        .parent()
        .unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let deadline = Instant::now() + Duration::from_mins(1);
    while task(home.path(), &id)["status"] != "done" {
        assert!(
            Instant::now() < deadline,
            "{}\n{:?}\n{}",
            task(home.path(), &id),
            kinds(home.path(), &id),
            lane_logs(home.path())
        );
        let out = common::command(home.path())
            .args(["kanban", "dispatch", "--once"])
            .env("PATH", &path)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
        std::thread::sleep(Duration::from_millis(300));
    }
    assert_eq!(
        task(home.path(), &id)["result"],
        "db denied",
        "{}",
        lane_logs(home.path())
    );
    let listed = json(home.path(), &["kanban", "attachments", &id]);
    assert_eq!(
        listed["attachments"][0]["filename"], "report.txt",
        "{listed}"
    );
    assert_eq!(
        listed["attachments"][0]["uploader"], "probe-cli",
        "{listed}"
    );
    assert!(
        !home.path().join("kanban/lanes").exists()
            || common::files_under(&home.path().join("kanban/lanes")).is_empty(),
        "the mailbox is removed when the lane ends"
    );
}

/// A lane named like a profile fails plugin load.
#[test]
fn a_lane_named_like_a_profile_fails_plugin_load() {
    let home = mock_home();
    profile(home.path(), "qa");
    lane_plugin(
        home.path(),
        "clash",
        "[[kanban_lanes]]\nname = \"qa\"\ncommand = [\"true\"]\n",
        &[],
    );
    ok(&run(
        home.path(),
        &["plugins", "enable", "clash", "--grant", "all"],
    ));
    let list = ok(&run(home.path(), &["plugins", "list"]));
    assert!(
        list.contains("kanban lane 'qa' collides with profile 'qa'"),
        "{list}"
    );
    let out = run(
        home.path(),
        &["kanban", "create", "x", "--assignee", "nobody-lane"],
    );
    assert_eq!(out.status.code(), Some(66));
}

/// Scenario: Slow observer cannot hold the board.
#[cfg(unix)]
#[test]
fn a_slow_dispatch_tick_hook_does_not_hold_the_board() {
    let home = mock_home();
    // Shell hooks may write only their scratch directory.
    let marker = home
        .path()
        .join("profiles/default/cache/hooks/hook-started");
    let hook = home.path().join("slow.sh");
    std::fs::write(
        &hook,
        "cat > /dev/null\ntouch \"$TMPDIR/hook-started\"\nsleep 20\n",
    )
    .unwrap();
    common::append_config(
        home.path(),
        &format!(
            "\n[[hooks.on_kanban_dispatch_tick]]\ncommand = \"sh {}\"\ntimeout_secs = 25\n",
            hook.display()
        ),
    );
    ok(&run(
        home.path(),
        &[
            "plugins",
            "hooks",
            "allow",
            "on_kanban_dispatch_tick",
            &format!("sh {}", hook.display()),
        ],
    ));
    let mut dispatcher = common::command(home.path())
        .args(["kanban", "dispatch", "--once"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !marker.exists() {
        assert!(Instant::now() < deadline, "the hook never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    // The tick's hook is still sleeping; another writer proceeds at once.
    let started = Instant::now();
    let id = create(home.path(), &["written while the hook sleeps"]);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the board was held"
    );
    assert_eq!(task(home.path(), &id)["status"], "ready");
    assert!(
        dispatcher.try_wait().unwrap().is_none(),
        "the hook still runs"
    );
    let _ = dispatcher.kill();
    let _ = dispatcher.wait();
}
