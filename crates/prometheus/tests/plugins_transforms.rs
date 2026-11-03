//! Plugin hooks that rewrite or watch a turn (spec: plugin-system,
//! Transform hooks rewrite results and replies; Streaming and lifecycle
//! observer hooks; `pre_verify` continuation hook; Approval and
//! human-input observer hooks), with the `testing-hooks` sample plugin
//! in real sessions on the mock provider.

mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;

use self::common::{command, mock_home, newest_session, ok, run, session_messages, stderr, stdout};

/// Places `testing-hooks` as plugin `id` (hook world 1.2) asking for
/// `capabilities`, enables it with everything granted and stores its
/// `settings` (`key`, JSON value).
fn hooks_plugin(home: &Path, id: &str, capabilities: &str, settings: &[(&str, &str)]) {
    let dir = home.join("profiles/default/plugins").join(id);
    pmt_plugins::testkit::install("testing-hooks", &dir);
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            "manifest_version = 1\nid = \"{id}\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
             description = \"test\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
             worlds = [\"hook@1.2\"]\n\n[capabilities]\n{capabilities}\n"
        ),
    )
    .unwrap();
    ok(&run(
        home,
        &["plugins", "adopt", dir.to_str().unwrap(), "--grant", "all"],
    ));
    ok(&run(home, &["plugins", "enable", id, "--grant", "all"]));
    for (key, value) in settings {
        ok(&run(
            home,
            &[
                "config",
                "set",
                &format!("plugins.entries.{id}.{key}"),
                value,
            ],
        ));
    }
}

/// What the plugin stored under `key` (its `state` interface).
fn plugin_state(home: &Path, id: &str, key: &str) -> String {
    let name = key.bytes().fold(String::new(), |mut name, b| {
        use std::fmt::Write as _;
        let _ = write!(name, "{b:02x}");
        name
    });
    std::fs::read_to_string(
        home.join("profiles/default/plugin-data")
            .join(id)
            .join("kv")
            .join(name),
    )
    .unwrap_or_default()
}

fn script(home: &Path, rules: &str) -> PathBuf {
    let path = home.join("script.toml");
    std::fs::write(&path, rules).unwrap();
    path
}

/// The working directory sessions run in (`<home>/work`).
fn work(home: &Path) -> PathBuf {
    let dir = home.join("work");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn one_shot(home: &Path, args: &[&str]) -> std::process::Output {
    command(home)
        .args(args)
        .current_dir(work(home))
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn texts_of(home: &Path, role: &str) -> Vec<String> {
    session_messages(home, &newest_session(home))
        .into_iter()
        .filter(|m| m["role"] == role)
        .map(|m| m["content"].as_str().unwrap_or_default().to_owned())
        .collect()
}

#[test]
fn a_tool_result_is_redacted_before_the_model_sees_it_and_stored_as_seen() {
    // Spec scenario "Tool result redaction".
    let home = mock_home();
    hooks_plugin(
        home.path(),
        "redact",
        "hooks = [\"transform_tool_result\"]",
        &[
            ("hooks", "\"transform_tool_result\""),
            ("mode", "\"redact:result\""),
        ],
    );
    std::fs::write(
        work(home.path()).join("page.txt"),
        "the key is sk-abc123xyz, keep it\n",
    )
    .unwrap();
    let rules = r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "read_file"
arguments = { path = "page.txt" }

[[rule]]
tool_result = "sk-abc123"
text = "LEAKED"

[[rule]]
tool_result = "[redacted]"
text = "saw it redacted"
"#;
    let script = script(home.path(), rules);
    let out = one_shot(
        home.path(),
        &["-z", "go", "--mock-script", script.to_str().unwrap()],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "saw it redacted\n");
    let tools = texts_of(home.path(), "tool");
    assert_eq!(tools.len(), 1);
    assert!(tools[0].contains("[redacted]"), "{tools:?}");
    assert!(!tools[0].contains("sk-abc123"), "{tools:?}");
}

#[test]
fn a_rewritten_reply_is_what_is_delivered_and_stored() {
    let home = mock_home();
    hooks_plugin(
        home.path(),
        "shout",
        "hooks = [\"transform_llm_output\"]",
        &[
            ("hooks", "\"transform_llm_output\""),
            ("mode", "\"upper:response\""),
        ],
    );
    let out = one_shot(home.path(), &["-z", "hello there"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "MOCK: HELLO THERE\n");
    let replies = texts_of(home.path(), "assistant");
    assert_eq!(replies, ["MOCK: HELLO THERE"]);
}

#[test]
fn a_held_reply_whose_hook_times_out_is_withheld() {
    let home = mock_home();
    hooks_plugin(
        home.path(),
        "redactor",
        "hooks = [\"transform_llm_output\"]",
        &[
            ("hooks", "\"transform_llm_output\""),
            ("hold", "\"transform_llm_output\""),
            ("mode", "\"spin\""),
        ],
    );
    ok(&run(
        home.path(),
        &["config", "set", "plugins.hook_timeout_ms", "300"],
    ));
    let out = one_shot(home.path(), &["-z", "secret plans"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "reply withheld by a plugin\n");
    assert!(!stdout(&out).contains("secret plans"));
    let replies = texts_of(home.path(), "assistant");
    assert_eq!(replies, ["reply withheld by a plugin"]);
}

#[test]
fn an_endless_pre_verify_continue_is_bounded() {
    // Spec scenario "Endless continue is bounded".
    let home = mock_home();
    hooks_plugin(
        home.path(),
        "verifier",
        "hooks = [\"pre_verify\"]",
        &[
            ("hooks", "\"pre_verify\""),
            ("mode", "\"continue:run the tests first\""),
        ],
    );
    let rules = r#"
[[rule]]
user = "edit"
[[rule.tool_calls]]
name = "write_file"
arguments = { path = "a.txt", content = "x" }

[[rule]]
repeat = true
text = "final answer"
"#;
    let script = script(home.path(), rules);
    let out = one_shot(
        home.path(),
        &[
            "-z",
            "edit",
            "--mock-script",
            script.to_str().unwrap(),
            "--yolo",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "final answer\n");
    let nudges = texts_of(home.path(), "user")
        .into_iter()
        .filter(|t| t.contains("run the tests first"))
        .count();
    assert_eq!(nudges, 3);
    let finals = texts_of(home.path(), "assistant")
        .into_iter()
        .filter(|t| t == "final answer")
        .count();
    assert_eq!(finals, 4);
    let last = plugin_state(home.path(), "verifier", "last_pre_verify");
    assert!(last.contains("\"attempt\":2"), "{last}");
    assert!(last.contains("a.txt"), "{last}");
}

#[test]
fn a_stream_observer_cannot_change_the_reply() {
    // Spec scenario "Stream observer cannot change output".
    let home = mock_home();
    hooks_plugin(
        home.path(),
        "watcher",
        "hooks = [\"on_stream_start\", \"on_stream_delta\", \"on_stream_end\"]",
        &[
            ("hooks", "\"on_stream_start,on_stream_delta,on_stream_end\""),
            ("mode", "\"upper:delta\""),
        ],
    );
    let out = one_shot(home.path(), &["-z", "quiet words"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(stdout(&out), "mock: quiet words\n");
    assert_eq!(texts_of(home.path(), "assistant"), ["mock: quiet words"]);
}

#[test]
fn disabling_a_skill_tells_on_skill_lifecycle() {
    let home = mock_home();
    hooks_plugin(
        home.path(),
        "skills-log",
        "hooks = [\"on_skill_lifecycle\"]",
        &[("hooks", "\"on_skill_lifecycle\"")],
    );
    ok(&run(home.path(), &["skills", "disable", "nothing-here"]));
    let last = plugin_state(home.path(), "skills-log", "last_on_skill_lifecycle");
    assert!(last.contains("\"skill\":\"nothing-here\""), "{last}");
    assert!(last.contains("\"action\":\"disable\""), "{last}");
}

#[cfg(all(unix, feature = "gateway"))]
mod gateway {
    use super::common::gateway::{Daemon, gateway_home_with, send, sent, wait_said};
    use super::common::{ok, run};
    use super::{hooks_plugin, plugin_state};

    #[test]
    fn a_held_reply_never_leaks_on_a_messaging_platform() {
        // Spec scenario "Held reply never leaks": the hook times out, the
        // chat gets the withheld notice and no draft or edit carried the
        // original text.
        let rules = "[[rule]]\nrepeat = true\ntext = \"the original secret reply\"\n";
        let home = gateway_home_with(rules, "");
        let home = home.path();
        hooks_plugin(
            home,
            "redactor",
            "hooks = [\"transform_llm_output\"]",
            &[
                ("hooks", "\"transform_llm_output\""),
                ("hold", "\"transform_llm_output\""),
                ("mode", "\"spin\""),
            ],
        );
        ok(&run(
            home,
            &["config", "set", "plugins.hook_timeout_ms", "300"],
        ));
        let daemon = Daemon::start(home);
        send(home, "default", "u1", "hello");
        wait_said(home, "default", "reply withheld by a plugin");
        let all = serde_json::to_string(&sent(home, "default")).unwrap();
        assert!(!all.contains("the original secret reply"), "{all}");
        daemon.stop(home);
    }

    #[test]
    fn a_notifier_observes_approvals_in_a_messaging_session() {
        // Spec scenario "Notifier observes approvals".
        let rules = r#"
[[rule]]
user = "clean"
[[rule.tool_calls]]
name = "terminal"
arguments = { command = "git reset --hard" }

[[rule]]
repeat = true
text = "done"
"#;
        let home = gateway_home_with(rules, "");
        let home = home.path();
        hooks_plugin(
            home,
            "notifier",
            "hooks = [\"pre_approval_request\", \"post_approval_response\"]",
            &[("hooks", "\"pre_approval_request,post_approval_response\"")],
        );
        let daemon = Daemon::start(home);
        send(home, "default", "u1", "clean");
        super::common::gateway::wait(
            "the approval observer",
            std::time::Duration::from_mins(1),
            || !plugin_state(home, "notifier", "last_pre_approval_request").is_empty(),
        );
        let seen = plugin_state(home, "notifier", "last_pre_approval_request");
        assert!(seen.contains("\"surface\":\"gateway\""), "{seen}");
        assert!(seen.contains("git reset --hard"), "{seen}");
        // The approval prompt is still waiting for the person.
        daemon.kill();
    }
}
