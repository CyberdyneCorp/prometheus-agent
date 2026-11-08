//! `external_process` providers (spec: model-providers, Plugin OAuth and
//! external-process providers) against the fake ACP agent CLI
//! `pmt-acp-fixture`.

use pmt_providers::StreamEvent;
use pmt_providers::resolve::{Target, Trace};

use crate::support::{Env, caller, request};

fn fixture() -> String {
    env!("CARGO_BIN_EXE_pmt-acp-fixture").replace('\\', "/")
}

fn config(command: &str) -> String {
    format!("[providers.agent-cli]\nauth = \"external_process\"\ncommand = \"{command}\"\n")
}

#[tokio::test]
async fn an_agent_cli_answers_over_acp_with_its_own_sign_in() {
    let env = Env::new(&config(&fixture()), &[]);
    let route = env.route("agent-cli", "default").await;
    assert!(route.base_url.starts_with("acp:///"), "{}", route.base_url);
    assert_eq!(route.billing_source(), "subscription:agent-cli/external");
    let mut stream = caller()
        .send(&route, &request("what is two plus two"), None)
        .await
        .unwrap();
    let (mut text, mut thought) = (String::new(), String::new());
    for event in stream.collect().await {
        match event {
            StreamEvent::TextDelta { text: t } => text.push_str(&t),
            StreamEvent::ReasoningDelta { text: t } => thought.push_str(&t),
            _ => {}
        }
    }
    assert_eq!(text, "agent saw: User: what is two plus two");
    // The CLI asked for a permission Prometheus cannot grant on its behalf.
    assert_eq!(thought, "permission cancelled");
    assert!(stream.failure().is_none());
}

/// Scenario: Missing external CLI.
#[tokio::test]
async fn a_missing_agent_cli_fails_the_route_naming_it() {
    let env = Env::new(&config("pmt-no-such-agent-cli"), &[]);
    let target = Target {
        provider: "agent-cli".to_owned(),
        ..Target::default()
    };
    let err = env
        .resolver()
        .resolve_target(&target, &mut Trace::default())
        .await
        .unwrap_err();
    assert_eq!(err.code().code, "provider.external_cli_missing");
    assert!(
        err.message().contains("'pmt-no-such-agent-cli'"),
        "{}",
        err.message()
    );
}

/// An executable file `dir/name` (a copy of the ACP fixture).
fn install(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let file = dir.join(if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    });
    std::fs::copy(fixture(), &file).unwrap();
    file
}

/// Scenario: Service launch with bare PATH (External agent CLI executable
/// resolution).
#[test]
fn a_bare_service_path_still_finds_the_user_local_and_vendor_installs() {
    use pmt_providers::external::locate;
    let home = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let bare = Some(std::ffi::OsString::from("/usr/bin:/bin"));
    assert_eq!(locate("claude", bare.clone(), Some(home.path())), None);
    let local = install(&home.path().join(".local/bin"), "claude");
    assert_eq!(
        locate("claude", bare.clone(), Some(home.path())),
        Some(local)
    );
    // Codex's own install prefix, searched after PATH and the user bins.
    let vendor = install(&home.path().join(".codex/bin"), "codex");
    assert_eq!(
        locate("codex", bare.clone(), Some(home.path())),
        Some(vendor.clone())
    );
    assert_eq!(
        pmt_providers::external::codex_executable(None, bare, Some(home.path())).unwrap(),
        vendor
    );
}

#[tokio::test]
async fn command_path_is_one_absolute_executable_never_a_shell_command() {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let cli = install(dir.path(), "agent cli");
    let config = format!(
        "[providers.agent-cli]\nauth = \"external_process\"\ncommand = \"not-installed\"\ncommand_path = \"{}\"\n",
        cli.display().to_string().replace('\\', "/")
    );
    let env = Env::new(&config, &[]);
    let route = env.route("agent-cli", "default").await;
    let mut stream = caller().send(&route, &request("hi"), None).await.unwrap();
    stream.collect().await;
    assert!(stream.failure().is_none(), "{:?}", stream.failure());

    let relative = Env::new(
        "[providers.agent-cli]\nauth = \"external_process\"\ncommand_path = \"bin/agent --flag\"\n",
        &[],
    );
    let target = Target {
        provider: "agent-cli".to_owned(),
        ..Target::default()
    };
    let err = relative
        .resolver()
        .resolve_target(&target, &mut Trace::default())
        .await
        .unwrap_err();
    assert_eq!(err.code().code, "config.invalid");
}
