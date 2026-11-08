//! Per-turn memory notifications (spec: memory, Per-turn memory
//! notifications): after the agent or a background review commits a
//! memory change, or the agent changes a skill, the surface shows a line
//! per `memory.notifications` (`on`, `verbose`, `off`; a messaging
//! platform may override it). Staged proposals are never announced.

mod common;

use common::{mock_home, run, stderr};

/// The agent adds one fact, then answers.
const ADDS: &str = r#"
[[rule]]
user = "remember"
times = 1
[[rule.tool_calls]]
name = "memory"
arguments = { target = "user", operations = [{ action = "add", content = "Lives in Porto." }] }

[[rule]]
user = "remember"
times = 1
[[rule.tool_calls]]
name = "skill_manage"
arguments = { action = "create", name = "porto-trips", content = "---\nname: porto-trips\ndescription: Plan a trip to Porto.\n---\n\nBook the train first.\n" }

[[rule]]
repeat = true
text = "noted"
"#;

fn one_shot(mode: &str) -> String {
    let home = mock_home();
    let script = home.path().join("script.toml");
    std::fs::write(&script, ADDS).unwrap();
    let set = format!("memory.notifications={mode}");
    let out = run(
        home.path(),
        &[
            "--set",
            &set,
            "chat",
            "--mock-script",
            script.to_str().unwrap(),
            "-z",
            "remember where I live",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    stderr(&out)
}

/// `on` says `Memory updated` and names the changed skill, `verbose`
/// previews the entry, `off` says nothing.
#[test]
fn each_mode_announces_what_it_promises() {
    let on = one_shot("on");
    assert!(on.contains("Memory updated"), "{on}");
    assert!(on.contains("Skill 'porto-trips' created"), "{on}");
    assert!(!on.contains("Lives in Porto."), "{on}");
    let verbose = one_shot("verbose");
    assert!(verbose.contains("Memory + Lives in Porto."), "{verbose}");
    let off = one_shot("off");
    assert!(
        !off.contains("Memory updated") && !off.contains("Skill 'porto-trips'"),
        "{off}"
    );
}

/// A background review that saves something: it runs after every turn
/// (`memory.nudge_interval = 1`).
#[cfg(any(feature = "gateway", unix))]
const REVIEW: &str = r#"
[[rule]]
user = "Review the conversation below"
times = 1
[[rule.tool_calls]]
name = "memory"
arguments = { target = "user", operations = [{ action = "add", content = "User prefers terse replies" }] }

[[rule]]
user = "Review the conversation below"
text = "reviewed"

[[rule]]
repeat = true
text = "sure"
"#;

/// The line interface shows a background review's notice between turns.
#[cfg(unix)]
#[test]
fn the_line_interface_shows_a_background_reviews_notice() {
    use common::pty::{exit, prompt, spawn};
    use expectrl::Expect as _;
    let home = mock_home();
    common::append_config(home.path(), "\n[memory]\nnudge_interval = 1\n");
    let script = home.path().join("script.toml");
    std::fs::write(&script, REVIEW).unwrap();
    let mut p = spawn(
        home.path(),
        &["--no-color", "--mock-script", script.to_str().unwrap()],
    );
    prompt(&mut p);
    p.send_line("keep it short please").unwrap();
    p.expect("sure").unwrap();
    p.expect("Memory updated").unwrap();
    prompt(&mut p);
    exit(p);
}

/// Scenario: Verbose preview (the platform's own setting wins over
/// `memory.notifications`).
#[cfg(feature = "gateway")]
#[test]
fn a_chat_receives_the_preview_of_a_background_reviews_change() {
    use common::gateway::{Daemon, gateway_home_with, send, wait_said};
    let home = gateway_home_with(
        REVIEW,
        "memory_notifications = \"verbose\"\n\n[memory]\nnudge_interval = 1\nnotifications = \"off\"\n",
    );
    let home = home.path();
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "keep it short please");
    wait_said(home, "default", "sure");
    wait_said(home, "default", "Memory + User prefers terse replies");
    daemon.stop(home);
}

/// Scenario: Staged change not announced.
#[cfg(feature = "gateway")]
#[test]
fn a_staged_review_proposal_is_not_announced() {
    use common::gateway::{Daemon, gateway_home_with, send, texts, wait, wait_said};
    let script = r#"
[[rule]]
user = "Review the conversation below"
times = 1
[[rule.tool_calls]]
name = "memory"
arguments = { target = "user", operations = [{ action = "patch", old_text = "Lisbon", content = "Lives in Porto." }] }

[[rule]]
user = "Review the conversation below"
text = "reviewed"

[[rule]]
repeat = true
text = "sure"
"#;
    let home = gateway_home_with(
        script,
        "\n[memory]\nnudge_interval = 1\nwrite_approval = true\n",
    );
    let home = home.path();
    common::ok(&run(
        home,
        &["memory", "add", "--target", "user", "Lives in Lisbon."],
    ));
    let daemon = Daemon::start(home);
    send(home, "default", "u1", "I moved");
    wait_said(home, "default", "sure");
    wait(
        "the review's proposal",
        std::time::Duration::from_mins(1),
        || common::ok(&run(home, &["memory", "pending"])).contains("Porto"),
    );
    // The proposal is staged; let anything the review would say arrive.
    send(home, "default", "u1", "thanks");
    wait(
        "the second answer",
        std::time::Duration::from_mins(1),
        || {
            texts(home, "default")
                .iter()
                .filter(|t| *t == "sure")
                .count()
                == 2
        },
    );
    let said = texts(home, "default");
    assert!(
        !said
            .iter()
            .any(|t| t.contains("Memory updated") || t.contains("Memory ~")),
        "{said:?}"
    );
    daemon.stop(home);
}
