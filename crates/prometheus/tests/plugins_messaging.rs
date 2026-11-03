//! Plugin live messages, message injection and platform actions through
//! the daemon's gateway (spec: plugin-system, Plugin live messages and
//! message injection; Platform actions), with the `testing-hooks` plugin
//! driving them from slash commands sent on the test platform.

#![cfg(all(unix, feature = "gateway"))]

mod common;

use std::path::Path;

use self::common::gateway::{Daemon, gateway_home, send, sent, texts, wait_said};
use self::common::{ok, run};

/// Places `testing-hooks` as plugin `talk` (hook world 1.2) in `profile`
/// with `capabilities`, its `commands` setting naming `commands`.
fn talk_plugin(home: &Path, profile: &str, capabilities: &str, commands: &str) {
    let dir = home.join("profiles").join(profile).join("plugins/talk");
    pmt_plugins::testkit::install("testing-hooks", &dir);
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            "manifest_version = 1\nid = \"talk\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
             description = \"test\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
             worlds = [\"hook@1.2\"]\n\n[capabilities]\n{capabilities}\n"
        ),
    )
    .unwrap();
    let p = ["-p", profile];
    let mut adopt = p.to_vec();
    adopt.extend(["plugins", "adopt", dir.to_str().unwrap(), "--grant", "all"]);
    ok(&run(home, &adopt));
    let mut enable = p.to_vec();
    enable.extend(["plugins", "enable", "talk", "--grant", "all"]);
    ok(&run(home, &enable));
    let setting = format!("\"{commands}\"");
    let mut set = p.to_vec();
    set.extend(["config", "set", "plugins.entries.talk.commands", &setting]);
    ok(&run(home, &set));
}

fn session_of(home: &Path, profile: &str) -> String {
    let listed = ok(&run(
        home,
        &["-p", profile, "--json", "sessions", "list", "--all"],
    ));
    let listed: serde_json::Value = serde_json::from_str(&listed).unwrap();
    listed["sessions"][0]["id"].as_str().unwrap().to_owned()
}

#[test]
fn a_progress_message_is_edited_in_place() {
    // Spec scenario "Progress message edits in place".
    let home = gateway_home("");
    let home = home.path();
    talk_plugin(home, "default", "commands = [\"post\"]", "post");
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "hello");
    wait_said(home, "default", "fast done");
    send(home, "default", "u1", "/post Migrating");
    wait_said(home, "default", "edited fallback=false");
    let posted: Vec<_> = sent(home, "default")
        .into_iter()
        .filter(|v| {
            v["text"]
                .as_str()
                .is_some_and(|t| t.starts_with("Migrating"))
        })
        .collect();
    let kinds: Vec<&str> = posted.iter().map(|v| v["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["send", "edit", "edit", "edit"], "{posted:?}");
    assert_eq!(posted[3]["text"], "Migrating 3");
    daemon.stop(home);
}

#[test]
fn injection_reaches_the_plugins_own_profile_only() {
    // Spec scenario "Injection into another profile refused".
    let home = gateway_home("");
    let home = home.path();
    ok(&run(home, &["profile", "create", "b"]));
    ok(&run(home, &["config", "set", "gateway.multiplex", "true"]));
    let other = home.join("profiles/b/config.toml");
    let mut config = std::fs::read_to_string(&other).unwrap_or_default();
    config.push_str(
        "\n[model]\nprovider = \"mock\"\ndefault = \"mock\"\n\n[gateway.platforms.test]\nenabled = true\nallow_all = true\n",
    );
    std::fs::write(&other, config).unwrap();
    talk_plugin(
        home,
        "default",
        "commands = [\"inject\"]\ninject_messages = true",
        "inject",
    );
    let daemon = Daemon::start(home);
    send(home, "b", "u2", "hello b");
    wait_said(home, "b", "mock: hello b");
    send(home, "default", "u1", "hello");
    wait_said(home, "default", "fast done");
    let foreign = session_of(home, "b");
    let before_b = texts(home, "b").len();
    send(home, "default", "u1", &format!("/inject {foreign}"));
    wait_said(home, "default", "injected=false");
    // Its own conversation takes it as new input: a turn answers it.
    let own = session_of(home, "default");
    let answered = texts(home, "default")
        .iter()
        .filter(|t| *t == "fast done")
        .count();
    send(home, "default", "u1", &format!("/inject {own}"));
    wait_said(home, "default", "injected=true");
    common::gateway::wait(
        "the injected turn",
        std::time::Duration::from_mins(1),
        || {
            texts(home, "default")
                .iter()
                .filter(|t| *t == "fast done")
                .count()
                > answered
        },
    );
    assert_eq!(
        texts(home, "b").len(),
        before_b,
        "nothing reached profile b"
    );
    daemon.stop(home);
}

#[test]
fn a_granted_platform_action_reaches_the_adapter_and_reports_what_it_lacks() {
    let home = gateway_home("");
    let home = home.path();
    talk_plugin(
        home,
        "default",
        "commands = [\"retitle\", \"react\"]\nplatform_actions = [\"set_thread_title\", \"add_reaction\"]",
        "retitle,react",
    );
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "hello");
    wait_said(home, "default", "fast done");
    send(home, "default", "u1", "/react test u1 m1");
    wait_said(
        home,
        "default",
        "ok=false action=add_reaction error=unsupported_platform_action",
    );
    send(home, "default", "u1", "/retitle nowhere c1 t1");
    wait_said(
        home,
        "default",
        "ok=false action=set_thread_title error=unknown_platform",
    );
    daemon.stop(home);
}
