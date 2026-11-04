//! Plugins in sessions (spec: plugin-system, Plugin system-prompt sections
//! are bounded and frozen; Plugin slash commands; Plugin-bundled skills;
//! Plugin LLM access gate; Hot reload respects the prompt-cache invariant;
//! Safe mode).

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;

use self::common::{command, mock_home, newest_session, ok, run, stderr, stdout};

fn plugin_dir(home: &Path, id: &str) -> PathBuf {
    home.join("profiles/default/plugins").join(id)
}

/// Places plugin `id` (crate `component`) with `worlds` and
/// `[capabilities]` text, with its install record.
fn place(home: &Path, id: &str, component: &str, worlds: &str, capabilities: &str) {
    let dir = plugin_dir(home, id);
    pmt_plugins::testkit::install(component, &dir);
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            "manifest_version = 1\nid = \"{id}\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
             description = \"test\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
             worlds = [{worlds}]\n\n[capabilities]\n{capabilities}\n"
        ),
    )
    .unwrap();
    record(&dir);
}

/// The install record `prometheus plugins adopt` writes.
fn record(dir: &Path) {
    pmt_plugins::record::InstallRecord {
        source: pmt_plugins::record::Origin::Adopted,
        location: dir.to_string_lossy().into_owned(),
        r#ref: None,
        package_digest: None,
        digest: pmt_plugins::digest::of_dir(dir).unwrap(),
        signer: None,
        tier: None,
        installed_at: "2026-11-03T00:00:00Z".to_owned(),
        flags: pmt_plugins::record::Flags::default(),
        held_update: None,
    }
    .write(dir)
    .unwrap();
}

/// The frozen system prompt text of session `id`.
fn frozen_prompt(home: &Path, id: &str) -> String {
    let db = rusqlite::Connection::open(home.join("profiles/default/state.db")).unwrap();
    let tiers: String = db
        .query_row(
            "SELECT tiers FROM session_prompts WHERE session_id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap();
    tiers
}

fn one_shot(home: &Path, args: &[&str]) -> String {
    let out = command(home)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    stdout(&out)
}

#[test]
fn sections_and_skills_join_sessions_started_while_the_plugin_is_enabled() {
    // Spec scenario "Disabling a plugin hides its skills next session",
    // with the plugin's system-prompt section alongside.
    let home = mock_home();
    place(
        home.path(),
        "kanban",
        "sample-hook",
        "\"hook@1.1\"",
        "prompt_section = 200\ncommands = [\"sample\"]",
    );
    let skill = plugin_dir(home.path(), "kanban").join("skills/board");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: board\ndescription: Works the kanban board.\n---\n\nBOARD-STEPS\n",
    )
    .unwrap();
    record(&plugin_dir(home.path(), "kanban"));
    ok(&run(
        home.path(),
        &["plugins", "enable", "kanban", "--grant", "all"],
    ));
    one_shot(home.path(), &["-z", "hi"]);
    let prompt = frozen_prompt(home.path(), &newest_session(home.path()));
    assert!(prompt.contains("Prefer short sentences."), "{prompt}");
    assert!(prompt.contains("kanban:board"), "{prompt}");
    // The skill is read in place, never copied into the profile.
    assert!(!home.path().join("profiles/default/skills/board").exists());

    ok(&run(home.path(), &["plugins", "disable", "kanban"]));
    one_shot(home.path(), &["-z", "hi again"]);
    let prompt = frozen_prompt(home.path(), &newest_session(home.path()));
    assert!(!prompt.contains("kanban:board"), "{prompt}");
    assert!(!prompt.contains("Prefer short sentences."), "{prompt}");
}

#[test]
fn a_plugin_model_call_is_charged_to_the_session_under_the_plugin() {
    // Spec: Plugin LLM access gate (every call charged to the session's
    // ledger under the plugin id).
    let home = mock_home();
    place(
        home.path(),
        "probe",
        "testing-probe",
        "\"tool@1.0\"",
        "llm = { enabled = true }",
    );
    ok(&run(
        home.path(),
        &["plugins", "enable", "probe", "--grant", "all"],
    ));
    let script = home.path().join("script.toml");
    std::fs::write(
        &script,
        r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "llm"
arguments = { v = "" }

[[rule]]
user = "hi"
text = "plugin answer"

[[rule]]
tool_result = "plugin answer"
text = "done"
"#,
    )
    .unwrap();
    let answer = one_shot(
        home.path(),
        &["-z", "go", "--mock-script", script.to_str().unwrap()],
    );
    assert_eq!(answer, "done\n");
    let id = newest_session(home.path());
    let db = rusqlite::Connection::open(home.path().join("profiles/default/state.db")).unwrap();
    let charged: i64 = db
        .query_row(
            "SELECT count(*) FROM usage_ledger WHERE session_id = ?1 AND purpose = 'plugin.probe'",
            [&id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(charged, 1);
}

#[test]
fn safe_mode_starts_with_a_broken_config_and_without_plugins() {
    // Spec scenarios "Broken config does not block safe mode" and "Broken
    // plugin bypassed".
    let home = mock_home();
    place(home.path(), "stuck", "testing-hooks", "\"hook@1.1\"", "");
    ok(&run(home.path(), &["plugins", "enable", "stuck"]));
    // A plugin that never finishes loading: without safe mode the load
    // deadline would be all that stops it.
    ok(&run(
        home.path(),
        &["config", "set", "plugins.entries.stuck.load", "\"spin\""],
    ));
    let config = home.path().join("profiles/default/config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("\nthis is [broken\n");
    std::fs::write(&config, text).unwrap();
    let out = command(home.path())
        .args(["--safe-mode", "chat", "-z", "hi"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(
        stderr(&out).contains(
            "--safe-mode reads no profile default model: launch with --model <m> --provider <p>"
        ),
        "{}",
        stderr(&out)
    );
    let started = std::time::Instant::now();
    let out = command(home.path())
        .args([
            "chat",
            "--safe-mode",
            "--model",
            "mock",
            "--provider",
            "mock",
            "-z",
            "hi",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "mock: hi\n");
    assert!(
        stderr(&out).contains("safe mode is active"),
        "{}",
        stderr(&out)
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(8));
}

#[cfg(unix)]
mod line {
    use std::path::Path;
    use std::process::Command;
    use std::time::Duration;

    use expectrl::session::OsSession;
    use expectrl::{Eof, Expect};

    use super::common::{ok, run};
    use super::{place, plugin_dir};

    fn spawn(home: &Path) -> OsSession {
        let mut cmd: Command = super::common::command(home);
        cmd.args(["--set", "ui.interface=line"])
            .current_dir(home)
            .env("TERM", "xterm")
            .env_remove("NO_COLOR");
        let mut session = OsSession::spawn(cmd).unwrap();
        session.set_expect_timeout(Some(Duration::from_secs(30)));
        session
    }

    fn prompt(p: &mut OsSession) {
        p.expect("› ").unwrap();
    }

    fn exit(mut p: OsSession) {
        p.send_line("/exit").unwrap();
        p.expect(Eof).unwrap();
    }

    /// The tools `tools list --session` reports exposed for session `id`.
    fn exposed(home: &Path, id: &str) -> Vec<String> {
        ok(&run(home, &["tools", "list", "--session", id]))
            .lines()
            .filter(|l| l.contains(" exposed"))
            .filter_map(|l| l.split_whitespace().next().map(str::to_owned))
            .collect()
    }

    fn sessions(home: &Path) -> Vec<String> {
        let listed = ok(&run(home, &["--json", "sessions", "list", "--all"]));
        let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
        listed["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn a_reloaded_plugin_tool_waits_for_the_next_session_unless_now() {
        // Spec scenario "New tool waits for the next session", `--now`, and
        // "Core command cannot be taken": the plugin's `/model` is refused
        // while its `/forecast` runs.
        let home = super::common::mock_home();
        place(home.path(), "weather", "sample-tool", "\"tool@1.0\"", "");
        place(
            home.path(),
            "radio",
            "testing-hooks",
            "\"hook@1.1\"",
            "commands = [\"forecast\", \"model\"]",
        );
        ok(&run(
            home.path(),
            &[
                "config",
                "set",
                "plugins.entries.radio.commands",
                "\"forecast,model\"",
            ],
        ));
        ok(&run(home.path(), &["plugins", "enable", "weather"]));
        ok(&run(
            home.path(),
            &["plugins", "enable", "radio", "--grant", "all"],
        ));
        let mut p = spawn(home.path());
        prompt(&mut p);
        p.send_line("first").unwrap();
        p.expect("mock: first").unwrap();
        prompt(&mut p);
        p.send_line("/forecast berlin").unwrap();
        p.expect("ran /forecast berlin in ses_").unwrap();
        // `session.info` names the surface and the session's cwd.
        p.expect(" from cli at ").unwrap();
        p.expect(home.path().file_name().unwrap().to_str().unwrap())
            .unwrap();
        prompt(&mut p);
        p.send_line("/model").unwrap();
        p.expect("mock").unwrap();
        prompt(&mut p);
        // The update: the same plugin now carries the probe's tools.
        let dir = plugin_dir(home.path(), "weather");
        pmt_plugins::testkit::install("testing-probe", &dir);
        super::record(&dir);
        ok(&run(
            home.path(),
            &["plugins", "grants", "approve", "weather"],
        ));
        p.send_line("/plugins reload weather").unwrap();
        p.expect("tools: next session").unwrap();
        prompt(&mut p);
        p.send_line("second").unwrap();
        p.expect("mock: second").unwrap();
        prompt(&mut p);
        let first = sessions(home.path())[0].clone();
        let tools = exposed(home.path(), &first);
        assert!(tools.contains(&"echo".to_owned()), "{tools:?}");
        assert!(!tools.contains(&"fetch".to_owned()), "{tools:?}");
        p.send_line("/plugins reload weather --now").unwrap();
        p.expect("tools: now (this session)").unwrap();
        prompt(&mut p);
        p.send_line("third").unwrap();
        p.expect("mock: third").unwrap();
        prompt(&mut p);
        let tools = exposed(home.path(), &first);
        assert!(tools.contains(&"fetch".to_owned()), "{tools:?}");
        p.send_line("/new").unwrap();
        p.expect("once / always / cancel").unwrap();
        p.send_line("once").unwrap();
        p.expect("new session").unwrap();
        prompt(&mut p);
        p.send_line("fourth").unwrap();
        p.expect("mock: fourth").unwrap();
        prompt(&mut p);
        exit(p);
        let newest = sessions(home.path())[0].clone();
        assert_ne!(newest, first);
        let tools = exposed(home.path(), &newest);
        assert!(tools.contains(&"fetch".to_owned()), "{tools:?}");
        let trajectory = ok(&run(
            home.path(),
            &["trajectory", "show", &first, "--format", "jsonl"],
        ));
        let rebuilt: Vec<&str> = trajectory
            .lines()
            .filter(|l| l.contains("\"prompt.rebuilt\""))
            .collect();
        assert!(
            rebuilt.iter().any(|l| l.contains("plugins --now")),
            "{trajectory}"
        );
    }

    #[test]
    fn a_plugin_enabled_after_the_session_started_works_after_plugins_reload() {
        // Spec: Hot reload respects the prompt-cache invariant (`/plugins
        // reload` replaces the instances; slash commands are active at
        // once): `plugins enable` run while a one-process session is open
        // counts at its next `/plugins reload`, and for its next session.
        let home = super::common::mock_home();
        place(
            home.path(),
            "radio",
            "testing-hooks",
            "\"hook@1.1\"",
            "commands = [\"forecast\"]",
        );
        ok(&run(
            home.path(),
            &[
                "config",
                "set",
                "plugins.entries.radio.commands",
                "\"forecast\"",
            ],
        ));
        let mut p = spawn(home.path());
        prompt(&mut p);
        p.send_line("/forecast berlin").unwrap();
        p.expect("Unknown command").unwrap();
        prompt(&mut p);
        ok(&run(
            home.path(),
            &["plugins", "enable", "radio", "--grant", "all"],
        ));
        p.send_line("/plugins reload").unwrap();
        p.expect("radio loaded").unwrap();
        p.expect("slash commands: now").unwrap();
        prompt(&mut p);
        p.send_line("/forecast berlin").unwrap();
        p.expect("ran /forecast berlin in ses_").unwrap();
        prompt(&mut p);
        p.send_line("/new").unwrap();
        p.expect("once / always / cancel").unwrap();
        p.send_line("once").unwrap();
        p.expect("new session").unwrap();
        prompt(&mut p);
        p.send_line("/forecast paris").unwrap();
        p.expect("ran /forecast paris in ses_").unwrap();
        prompt(&mut p);
        exit(p);
    }

    #[test]
    fn reload_now_reports_skills_for_the_next_session_as_the_prompt_shows() {
        // Spec: Hot reload respects the prompt-cache invariant (the command
        // prints which parts became active now and which are deferred): an
        // update adding a section and a skill, reloaded with `--now`, puts
        // the section into the current prompt, while the skill, reported
        // `next session`, joins only a new session's prompt.
        let home = super::common::mock_home();
        place(
            home.path(),
            "kanban",
            "testing-hooks",
            "\"hook@1.1\"",
            "prompt_section = 200\ncommands = [\"sample\"]",
        );
        ok(&run(
            home.path(),
            &["plugins", "enable", "kanban", "--grant", "all"],
        ));
        let mut p = spawn(home.path());
        prompt(&mut p);
        p.send_line("first").unwrap();
        p.expect("mock: first").unwrap();
        prompt(&mut p);
        // The update: the plugin now renders a section and ships a skill.
        let dir = plugin_dir(home.path(), "kanban");
        pmt_plugins::testkit::install("sample-hook", &dir);
        let skill = dir.join("skills/board");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: board\ndescription: Works the kanban board.\n---\n\nBOARD-STEPS\n",
        )
        .unwrap();
        super::record(&dir);
        ok(&run(
            home.path(),
            &["plugins", "grants", "approve", "kanban"],
        ));
        p.send_line("/plugins reload --now").unwrap();
        p.expect("kanban loaded").unwrap();
        p.expect("system prompt sections: now (this session)")
            .unwrap();
        p.expect("skills: next session").unwrap();
        prompt(&mut p);
        p.send_line("second").unwrap();
        p.expect("mock: second").unwrap();
        prompt(&mut p);
        exit(p);
        let first = sessions(home.path())[0].clone();
        let rebuilt = super::frozen_prompt(home.path(), &first);
        assert!(rebuilt.contains("Prefer short sentences."), "{rebuilt}");
        assert!(!rebuilt.contains("kanban:board"), "{rebuilt}");
        super::one_shot(home.path(), &["-z", "third"]);
        let next = super::frozen_prompt(home.path(), &super::common::newest_session(home.path()));
        assert!(next.contains("kanban:board"), "{next}");
    }
}

#[cfg(all(unix, feature = "gateway"))]
mod daemon {
    use expectrl::Expect as _;

    use super::common::{daemon, mock_home, ok, pty, run, stderr};
    use super::place;

    #[test]
    fn an_enabled_plugin_command_works_at_once_in_a_running_daemon_session() {
        // Spec: Hot reload respects the prompt-cache invariant (an enable
        // replaces the running daemon's plugin instances; slash commands
        // are active at once in sessions started before it).
        let home = mock_home();
        place(
            home.path(),
            "radio",
            "testing-hooks",
            "\"hook@1.1\"",
            "commands = [\"forecast\"]",
        );
        ok(&run(
            home.path(),
            &[
                "config",
                "set",
                "plugins.entries.radio.commands",
                "\"forecast\"",
            ],
        ));
        let _daemon = daemon::start(home.path(), &[]);
        let mut p = pty::spawn(home.path(), &[]);
        p.expect("attached to the gateway daemon").unwrap();
        pty::prompt(&mut p);
        p.send_line("first").unwrap();
        p.expect("mock: first").unwrap();
        pty::prompt(&mut p);
        p.send_line("/forecast berlin").unwrap();
        p.expect("Unknown command").unwrap();
        pty::prompt(&mut p);
        let out = run(
            home.path(),
            &["plugins", "enable", "radio", "--grant", "all"],
        );
        assert!(out.status.success(), "{}", stderr(&out));
        let said = stderr(&out);
        assert!(
            said.contains("the running daemon reloaded its plugins:\nradio loaded"),
            "{said}"
        );
        assert!(said.contains("slash commands: now"), "{said}");
        assert!(said.contains("tools: next session"), "{said}");
        p.send_line("/forecast berlin").unwrap();
        p.expect("ran /forecast berlin in ses_").unwrap();
        p.expect(" from cli at /").unwrap();
        pty::prompt(&mut p);
        pty::exit(p);
    }

    #[test]
    fn a_plugin_command_sent_on_a_messaging_platform_sees_the_gateway_surface() {
        // Spec: Host interfaces offered to plugins (`session.info` gives
        // the surface and cwd) for a command run by the daemon's gateway.
        use super::common::gateway::{Daemon, gateway_home, send, texts, wait_said};

        let home = gateway_home("");
        let home = home.path();
        place(
            home,
            "radio",
            "testing-hooks",
            "\"hook@1.1\"",
            "commands = [\"forecast\"]",
        );
        ok(&run(
            home,
            &[
                "config",
                "set",
                "plugins.entries.radio.commands",
                "\"forecast\"",
            ],
        ));
        ok(&run(
            home,
            &["plugins", "enable", "radio", "--grant", "all"],
        ));
        let daemon = Daemon::start(home);
        send(home, "default", "u1", "hello");
        wait_said(home, "default", "fast done");
        send(home, "default", "u1", "/forecast berlin");
        wait_said(home, "default", "ran /forecast berlin in ses_");
        let said = texts(home, "default");
        let answer = said.iter().find(|t| t.contains("ran /forecast")).unwrap();
        assert!(answer.contains(" from gateway at /"), "{answer}");
        daemon.stop(home);
    }

    #[test]
    fn a_plugin_tool_called_in_a_messaging_session_sees_the_gateway_surface() {
        // Spec: Host interfaces offered to plugins (`session.info` gives
        // the surface): the tool registry is shared by every session of the
        // daemon, yet a gateway session's tool call reports `gateway`.
        use super::common::gateway::{Daemon, gateway_home_with, send, wait_said};

        let rules = r#"
[[rule]]
user = "probe"
[[rule.tool_calls]]
name = "session"
arguments = {}

# A plugin's result arrives inside its untrusted-content block.
[[rule]]
tool_result_regex = '(?m)^default\|gateway\|ses_'
text = "probe saw gateway"

[[rule]]
tool_result_regex = '.'
text = "probe saw another surface"

[[rule]]
repeat = true
text = "fast done"
"#;
        let home = gateway_home_with(rules, "");
        let home = home.path();
        place(home, "probe", "testing-probe", "\"tool@1.0\"", "");
        ok(&run(
            home,
            &["plugins", "enable", "probe", "--grant", "all"],
        ));
        let daemon = Daemon::start(home);
        send(home, "default", "u1", "probe");
        wait_said(home, "default", "probe saw gateway");
        daemon.stop(home);
    }
}
