//! The line interface driven through a pseudo-terminal with expectrl
//! (spec: cli, Default subcommand; Single typed slash-command registry;
//! Command resolution; Built-in slash command set; Busy policy
//! enforcement; Deferred effects with `--now`; Destructive command
//! confirmation; Skill invocation as user message; Quick commands;
//! Interactive input handling; Themes are data; Usage display;
//! Conversation recap on resume; Seed prompt and prompt file).

#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use common::{ok, run, temp_home};
use expectrl::session::OsSession;
use expectrl::{Eof, Expect};

/// A home whose default profile talks to the mock provider.
fn mock_home() -> tempfile::TempDir {
    let home = temp_home();
    ok(&run(
        home.path(),
        &["config", "set", "model.provider", "mock"],
    ));
    ok(&run(
        home.path(),
        &["config", "set", "model.default", "mock"],
    ));
    home
}

fn profile(home: &Path) -> PathBuf {
    home.join("profiles/default")
}

/// Writes a mock script and returns its path.
fn script(home: &Path, text: &str) -> String {
    let path = home.join("script.toml");
    std::fs::write(&path, text).unwrap();
    path.to_string_lossy().into_owned()
}

/// The binary in a pseudo-terminal, run from the home directory.
fn spawn(home: &Path, args: &[&str]) -> OsSession {
    let mut cmd: Command = common::command(home);
    cmd.args(args)
        .current_dir(home)
        .env("TERM", "xterm")
        .env_remove("NO_COLOR");
    let mut session = OsSession::spawn(cmd).unwrap();
    session.set_expect_timeout(Some(Duration::from_secs(30)));
    session
}

/// Waits for the idle prompt.
fn prompt(p: &mut OsSession) {
    p.expect("› ").unwrap();
}

/// Sends `/exit` and waits for the process to end with status 0.
fn exit(mut p: OsSession) {
    p.send_line("/exit").unwrap();
    p.expect(Eof).unwrap();
    let status = p.get_process_mut().wait().unwrap();
    assert!(format!("{status:?}").ends_with(", 0)"), "{status:?}");
}

/// The active messages of the newest session (`sessions export`).
fn messages(home: &Path) -> Vec<serde_json::Value> {
    let listed = ok(&run(home, &["--json", "sessions", "list", "--all"]));
    let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
    let id = listed["sessions"][0]["id"].as_str().unwrap().to_owned();
    ok(&run(home, &["sessions", "export", &id]))
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter_map(|line| line.get("message").cloned())
        .filter(|m| m["active"] == true)
        .collect()
}

/// Scenario: Default subcommand (`prometheus -p work` in a terminal).
#[test]
fn no_subcommand_on_a_terminal_starts_an_interactive_chat() {
    let home = mock_home();
    ok(&run(home.path(), &["profile", "create", "work"]));
    ok(&run(
        home.path(),
        &["-p", "work", "config", "set", "model.provider", "mock"],
    ));
    let mut p = spawn(home.path(), &["-p", "work"]);
    p.expect("profile work").unwrap();
    prompt(&mut p);
    p.send_line("hello there").unwrap();
    p.expect("mock: hello there").unwrap();
    prompt(&mut p);
    exit(p);
}

/// `/help` is generated from the registry; a unique prefix runs its
/// command; arguments keep their case.
#[test]
fn help_prefix_and_argument_case() {
    let home = mock_home();
    let mut p = spawn(home.path(), &[]);
    prompt(&mut p);
    p.send_line("/help").unwrap();
    p.expect("Session:").unwrap();
    p.expect("/compress").unwrap();
    p.expect("Exit:").unwrap();
    prompt(&mut p);
    p.send_line("/comp").unwrap();
    p.expect("compress: ").unwrap();
    prompt(&mut p);
    p.send_line("/TITLE My Project").unwrap();
    p.expect("title set to My Project").unwrap();
    prompt(&mut p);
    p.send_line("/zzzzzz").unwrap();
    p.expect("Unknown command: /zzzzzz").unwrap();
    prompt(&mut p);
    exit(p);
    let listed = ok(&run(home.path(), &["sessions", "list"]));
    assert!(listed.contains("My Project"), "{listed}");
}

/// Scenario: Rejected while busy; plain text typed meanwhile is queued
/// (`busy.input_mode = "queue"`), never dropped.
#[test]
fn a_command_typed_while_busy_follows_its_busy_policy() {
    let home = mock_home();
    let path = script(
        home.path(),
        "[[rule]]\nuser = \"slow\"\nrepeat = true\nlatency_ms = 1500\ntext = \"done slowly\"\n\n[[rule]]\nrepeat = true\ntext = \"quick\"\n",
    );
    let mut p = spawn(home.path(), &["--mock-script", &path]);
    prompt(&mut p);
    p.send_line("slow").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    p.send_line("/compress").unwrap();
    p.expect("/compress is unavailable while the agent is working; use /stop first")
        .unwrap();
    p.send_line("/usage").unwrap();
    p.expect("lineage").unwrap();
    p.send_line("next one").unwrap();
    p.expect("queued").unwrap();
    p.expect("done slowly").unwrap();
    p.expect("quick").unwrap();
    prompt(&mut p);
    exit(p);
}

/// Scenario: Ctrl+C interrupts the turn, not the process; then a second
/// Ctrl+C at the idle prompt exits.
#[test]
fn ctrl_c_stops_the_turn_and_twice_when_idle_exits() {
    let home = mock_home();
    let path = script(
        home.path(),
        "[[rule]]\nrepeat = true\nlatency_ms = 20000\ntext = \"never seen\"\n",
    );
    let mut p = spawn(home.path(), &["--mock-script", &path]);
    prompt(&mut p);
    p.send_line("take your time").unwrap();
    std::thread::sleep(Duration::from_millis(500));
    p.send("\x03").unwrap();
    p.expect("interrupted").unwrap();
    prompt(&mut p);
    p.send("\x03").unwrap();
    p.expect("press Ctrl+C again").unwrap();
    p.send("\x03").unwrap();
    p.expect(Eof).unwrap();
    let saved = messages(home.path());
    assert!(
        saved
            .iter()
            .any(|m| m["content"].as_str() == Some("take your time")),
        "the partial transcript is persisted: {saved:?}"
    );
}

/// A trailing `\` continues the line; history is saved owner-only with
/// secrets redacted; Ctrl+D exits.
#[test]
fn multiline_input_history_and_ctrl_d() {
    use std::os::unix::fs::PermissionsExt as _;
    let home = mock_home();
    let secret = "sk-test-0123456789abcdefghij";
    common::run_with_stdin(home.path(), &["secrets", "set", "OPENAI_API_KEY"], secret);
    let mut p = spawn(home.path(), &[]);
    prompt(&mut p);
    p.send_line("one \\").unwrap();
    p.send_line("two").unwrap();
    p.expect("mock: one ").unwrap();
    p.expect("two").unwrap();
    prompt(&mut p);
    p.send_line(format!("remember {secret}")).unwrap();
    p.expect("mock: remember").unwrap();
    prompt(&mut p);
    p.send("\x04").unwrap();
    p.expect(Eof).unwrap();
    let history = profile(home.path()).join("cli_history");
    let text = std::fs::read_to_string(&history).unwrap();
    assert!(text.contains("remember"), "{text}");
    assert!(!text.contains(secret), "{text}");
    let mode = std::fs::metadata(&history).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

/// Scenario: Cancel keeps history; `--yes` skips the question.
#[test]
fn destructive_commands_ask_first() {
    let home = mock_home();
    let mut p = spawn(home.path(), &[]);
    prompt(&mut p);
    p.send_line("keep me").unwrap();
    p.expect("mock: keep me").unwrap();
    prompt(&mut p);
    p.send_line("/clear").unwrap();
    p.expect("once / always / cancel").unwrap();
    p.send_line("cancel").unwrap();
    p.expect("cancelled").unwrap();
    prompt(&mut p);
    exit(p);
    let saved = messages(home.path());
    assert_eq!(saved.len(), 2, "transcript unchanged: {saved:?}");
    let mut p = spawn(home.path(), &["-c"]);
    prompt(&mut p);
    p.send_line("/undo --yes").unwrap();
    p.expect("undid the last exchange (2 messages)").unwrap();
    prompt(&mut p);
    exit(p);
}

/// Scenarios: Toolset change deferred / Toolset change now.
#[test]
fn a_toolset_change_waits_for_the_next_session_unless_now() {
    let home = mock_home();
    let mut p = spawn(home.path(), &[]);
    prompt(&mut p);
    p.send_line("/tools disable file").unwrap();
    p.expect("in your next session. Use /new to start one now, or add --now")
        .unwrap();
    prompt(&mut p);
    p.send_line("first").unwrap();
    p.expect("mock: first").unwrap();
    prompt(&mut p);
    p.send_line("/tools disable file --now").unwrap();
    p.expect("disabled now").unwrap();
    prompt(&mut p);
    p.send_line("second").unwrap();
    p.expect("mock: second").unwrap();
    prompt(&mut p);
    exit(p);
    let listed = ok(&run(home.path(), &["--json", "sessions", "list"]));
    let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
    let id = listed["sessions"][0]["id"].as_str().unwrap().to_owned();
    let trajectory = ok(&run(
        home.path(),
        &["trajectory", "show", &id, "--format", "jsonl"],
    ));
    let rebuilt: Vec<&str> = trajectory
        .lines()
        .filter(|l| l.contains("\"prompt.rebuilt\""))
        .collect();
    assert_eq!(rebuilt.len(), 2, "{trajectory}");
    assert!(rebuilt[1].contains("tools --now"), "{}", rebuilt[1]);
    let tools = ok(&run(home.path(), &["tools", "list", "--session", &id]));
    let read_file = tools
        .lines()
        .find(|l| l.starts_with("read_file"))
        .unwrap_or_default();
    assert!(!read_file.contains(" exposed"), "{tools}");
    let config = std::fs::read_to_string(profile(home.path()).join("config.toml")).unwrap();
    assert!(
        config.contains("disabled_toolsets = [\"file\"]"),
        "{config}"
    );
}

/// Scenario: Stacked skills: one user message loads both skills, the
/// system prompt is not rebuilt.
#[test]
fn stacked_skills_become_one_user_message() {
    let home = mock_home();
    for (name, body) in [("pdf", "PDF-STEPS"), ("summarise", "SUMMARY-STEPS")] {
        let dir = profile(home.path()).join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: Does {name}.\n---\n\n{body}\n"),
        )
        .unwrap();
    }
    let mut p = spawn(home.path(), &[]);
    prompt(&mut p);
    p.send_line("/pdf /summarise report.pdf").unwrap();
    p.expect("report.pdf").unwrap();
    prompt(&mut p);
    exit(p);
    let saved = messages(home.path());
    let user = saved[0]["content"].as_str().unwrap();
    assert!(
        user.contains("PDF-STEPS") && user.contains("SUMMARY-STEPS"),
        "{user}"
    );
    assert!(user.contains("report.pdf"), "{user}");
}

/// Scenarios: Exec quick command timeout; Skill shadowed by quick
/// command; aliases re-dispatch with the user's arguments.
#[test]
fn quick_commands_alias_exec_and_shadow_skills() {
    let home = mock_home();
    let config = profile(home.path()).join("config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str(
        "\n[quick_commands.name]\ntype = \"alias\"\ntarget = \"/title\"\n\n\
         [quick_commands.slow]\ntype = \"exec\"\ncommand = \"sleep 60\"\ntimeout_secs = 1\n\n\
         [quick_commands.deploy]\ntype = \"exec\"\ncommand = \"echo deployed-by-quick\"\n",
    );
    std::fs::write(&config, text).unwrap();
    let dir = profile(home.path()).join("skills/deploy");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: deploy\ndescription: Deploys.\n---\n\nSKILL\n",
    )
    .unwrap();
    let mut p = spawn(home.path(), &[]);
    prompt(&mut p);
    p.send_line("/name Quick Title").unwrap();
    p.expect("title set to Quick Title").unwrap();
    prompt(&mut p);
    p.send_line("/slow").unwrap();
    p.expect("quick command 'slow' timed out after 1s").unwrap();
    prompt(&mut p);
    p.send_line("/deploy").unwrap();
    p.expect("deployed-by-quick").unwrap();
    prompt(&mut p);
    exit(p);
    let logs = common::files_under(home.path())
        .into_iter()
        .filter(|f| f.to_string_lossy().contains("logs"))
        .filter_map(|f| std::fs::read_to_string(f).ok())
        .collect::<String>();
    assert_eq!(
        logs.matches("'/deploy' is defined by a quick command and a skill")
            .count(),
        1,
        "{logs}"
    );
}

/// Scenario: Unknown theme; `/theme <name>` switches and saves.
#[test]
fn themes_list_switch_and_refuse_unknown_names() {
    let home = mock_home();
    let mut p = spawn(home.path(), &["--no-color"]);
    prompt(&mut p);
    p.send_line("/theme neon").unwrap();
    p.expect("no theme 'neon'").unwrap();
    p.expect("default (current)").unwrap();
    prompt(&mut p);
    p.send_line("/theme slate").unwrap();
    p.expect("theme slate").unwrap();
    prompt(&mut p);
    exit(p);
    let config = std::fs::read_to_string(profile(home.path()).join("config.toml")).unwrap();
    assert!(config.contains("theme = \"slate\""), "{config}");
}

/// Scenario: Seed text starting with a slash; `-q` drops the banner.
#[test]
fn a_seed_query_is_sent_verbatim_and_quiet_drops_the_banner() {
    let home = mock_home();
    let mut p = spawn(
        home.path(),
        &["-q", "--query", "/model is what I want to discuss"],
    );
    p.expect("mock: /model is what I want to discuss").unwrap();
    prompt(&mut p);
    exit(p);
    let saved = messages(home.path());
    assert_eq!(
        saved[0]["content"].as_str(),
        Some("/model is what I want to discuss")
    );
}

/// Scenario: Tool-only messages skipped (recap on resume).
#[test]
fn the_recap_on_resume_skips_tool_only_messages() {
    let home = mock_home();
    let path = script(
        home.path(),
        r#"
[[rule]]
tool_result = "PLAN-RESULT"
repeat = true
text = "the final plan"

[[rule]]
user = "plan"
repeat = true
[[rule.tool_calls]]
name = "todo"
arguments = { todos = [{ id = "1", content = "PLAN-RESULT", status = "pending" }] }

[[rule]]
repeat = true
text = "ok"
"#,
    );
    let out = common::run(
        home.path(),
        &["--json", "-z", "plan it", "--mock-script", &path],
    );
    let body: serde_json::Value = serde_json::from_str(&ok(&out)).unwrap();
    let id = body["session_id"].as_str().unwrap().to_owned();
    let mut p = spawn(home.path(), &["-r", &id, "--no-color"]);
    p.expect("> plan it").unwrap();
    p.expect("the final plan").unwrap();
    prompt(&mut p);
    exit(p);
}

/// A shell in its own pseudo-terminal, with a clean environment.
fn shell(home: &Path) -> OsSession {
    let user_home = common::user_home(home);
    std::fs::create_dir_all(&user_home).unwrap();
    let mut cmd = Command::new("/usr/bin/env");
    cmd.args(["-i", "TERM=xterm", "PS1=$ "])
        .arg(format!(
            "PATH={}",
            std::env::var("PATH").unwrap_or_default()
        ))
        .arg(format!("HOME={}", user_home.display()))
        .args(["/bin/sh"])
        .current_dir(home);
    let mut session = OsSession::spawn(cmd).unwrap();
    session.set_expect_timeout(Some(Duration::from_secs(30)));
    session
}

/// The newest session id.
fn newest(home: &Path) -> String {
    let listed = ok(&run(home, &["--json", "sessions", "list"]));
    let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
    listed["sessions"][0]["id"].as_str().unwrap().to_owned()
}

/// Scenario: Per-terminal continue: session A last used from terminal 1,
/// a newer session B from terminal 2; `-c` in terminal 1 resumes A.
#[test]
fn continue_prefers_the_session_last_used_from_this_terminal() {
    let home = mock_home();
    let chat = format!("{} --home {}", common::BIN, home.path().display());
    let mut sessions = Vec::new();
    let mut terminals = Vec::new();
    for word in ["alpha", "beta"] {
        let mut t = shell(home.path());
        t.send_line(&chat).unwrap();
        prompt(&mut t);
        t.send_line(word).unwrap();
        t.expect(format!("mock: {word}").as_str()).unwrap();
        prompt(&mut t);
        t.send_line("/exit").unwrap();
        t.expect("$ ").unwrap();
        sessions.push(newest(home.path()));
        terminals.push(t);
    }
    let mut first = terminals.remove(0);
    first
        .send_line(format!("{chat} --json -c -z again"))
        .unwrap();
    let found = first.expect(expectrl::Regex("ses_[0-9A-Z]{26}")).unwrap();
    let id = String::from_utf8_lossy(found.get(0).unwrap()).into_owned();
    assert_eq!(id, sessions[0], "terminal 1 resumes its own session");
    assert_ne!(sessions[0], sessions[1]);
}

/// Scenario: Lineage cost shown: `/usage` sums the session and its whole
/// lineage (here a fork of the first session).
#[test]
fn usage_shows_the_session_and_its_lineage() {
    let home = mock_home();
    let mut p = spawn(home.path(), &["--no-color"]);
    prompt(&mut p);
    p.send_line("hi").unwrap();
    p.expect("mock: hi").unwrap();
    prompt(&mut p);
    p.send_line("/fork").unwrap();
    p.expect("forked").unwrap();
    prompt(&mut p);
    p.send_line("again").unwrap();
    p.expect("mock: again").unwrap();
    prompt(&mut p);
    p.send_line("/usage").unwrap();
    p.expect("session  calls 1 ").unwrap();
    p.expect("lineage  calls 2 ").unwrap();
    prompt(&mut p);
    exit(p);
}

/// `@` completes files under the session's working directory.
#[test]
fn at_references_complete_files_of_the_working_directory() {
    let home = mock_home();
    std::fs::write(home.path().join("notes-unique.md"), "x").unwrap();
    let mut p = spawn(home.path(), &["--no-color"]);
    prompt(&mut p);
    p.send("@file:notes-u\t").unwrap();
    p.expect("notes-unique.md").unwrap();
    p.send("\x15").unwrap();
    p.send_line("").unwrap();
    prompt(&mut p);
    exit(p);
}

const REFINE_SCRIPT: &str = r#"
[[rule]]
user_regex = "(?s)reusable know-how.*focus on: save the deploy workflow as a skill"
times = 1
latency_ms = 1500
[[rule.tool_calls]]
name = "skill_manage"
arguments = { action = "create", name = "deploy", category = "ops", content = "---\nname: deploy\ndescription: Deploy the service.\n---\n\n1. Build.\n2. Ship.\n" }

[[rule]]
user = "reusable know-how"
tool_result_regex = "Created skill"
text = "saved the deploy workflow"

[[rule]]
user = "durable knowledge"
times = 1
[[rule.tool_calls]]
name = "memory"
arguments = { target = "memory", operations = [{ action = "add", content = "Deploys run build then ship." }] }

[[rule]]
user = "durable knowledge"
text = "noted"

[[rule]]
repeat = true
text = "ok"
"#;

/// Scenario: Focused refine saves a workflow (spec: skills, /refine runs
/// self-improvement review on demand): the review runs in the background,
/// a second `/refine` meanwhile is refused, the summary reaches the chat
/// and the session's system prompt is never rebuilt.
#[test]
fn refine_reviews_in_the_background_and_posts_its_summary() {
    let home = mock_home();
    let path = script(home.path(), REFINE_SCRIPT);
    let mut p = spawn(home.path(), &["--no-color", "--mock-script", &path]);
    prompt(&mut p);
    p.send_line("deploy: build then ship").unwrap();
    p.expect("ok").unwrap();
    prompt(&mut p);
    p.send_line("/refine save the deploy workflow as a skill")
        .unwrap();
    p.expect("Self-improvement review started").unwrap();
    prompt(&mut p);
    p.send_line("/refine").unwrap();
    p.expect("a review is already running").unwrap();
    p.expect("Created skills: deploy").unwrap();
    p.expect("saved the deploy workflow").unwrap();
    p.expect("Memory: 1 saved.").unwrap();
    p.send_line("thanks").unwrap();
    p.expect("ok").unwrap();
    prompt(&mut p);
    exit(p);

    assert!(
        profile(home.path())
            .join("skills/ops/deploy/SKILL.md")
            .is_file()
    );
    let listed = ok(&run(home.path(), &["--json", "sessions", "list"]));
    let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
    let sessions = listed["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 1, "review children are hidden: {listed}");
    let id = sessions[0]["id"].as_str().unwrap().to_owned();
    let trajectory = ok(&run(
        home.path(),
        &["trajectory", "show", &id, "--format", "jsonl"],
    ));
    let rebuilt: Vec<&str> = trajectory
        .lines()
        .filter(|l| l.contains("\"prompt.rebuilt\""))
        .collect();
    assert_eq!(rebuilt.len(), 1, "built once at start: {trajectory}");
    let exported = ok(&run(home.path(), &["sessions", "export", &id]));
    let users: Vec<&str> = exported
        .lines()
        .filter(|l| l.contains("\"role\":\"user\""))
        .collect();
    assert_eq!(
        users.len(),
        2,
        "history holds only the two exchanges: {exported}"
    );
}

const NUDGE_SCRIPT: &str = r#"
[[rule]]
user = "reusable know-how"
times = 1
[[rule.tool_calls]]
name = "skill_manage"
arguments = { action = "create", name = "listing", content = "---\nname: listing\ndescription: List the skills.\n---\n\nCall skills_list.\n" }

[[rule]]
user = "reusable know-how"
text = "saved"

[[rule]]
user = "list my skills"
times = 1
[[rule.tool_calls]]
name = "skills_list"
arguments = {}

[[rule]]
user = "list my skills"
text = "listed"

[[rule]]
repeat = true
text = "ok"
"#;

/// Spec: skills, Autonomous skill creation and improvement: a completed
/// turn that reaches `skills.creation_nudge_interval` tool-calling
/// iterations runs a background review after the reply.
#[test]
fn the_creation_nudge_runs_a_background_skill_review() {
    let home = mock_home();
    ok(&run(
        home.path(),
        &["config", "set", "skills.creation_nudge_interval", "1"],
    ));
    let path = script(home.path(), NUDGE_SCRIPT);
    let mut p = spawn(home.path(), &["--no-color", "--mock-script", &path]);
    prompt(&mut p);
    p.send_line("list my skills").unwrap();
    p.expect("listed").unwrap();
    prompt(&mut p);
    let skill = profile(home.path()).join("skills/listing/SKILL.md");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !skill.is_file() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(skill.is_file(), "the review created the skill");
    exit(p);
}
