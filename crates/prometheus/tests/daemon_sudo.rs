//! Sudo passwords in daemon sessions (spec: execution-environments, Sudo
//! password handling): an interactive session asks its clients with
//! `sudo.request` once and reuses the answer; a one-shot session never
//! asks. The stand-in `sudo` behaves as sudo without a terminal: `-S`
//! reads the password, `-v` only checks it, a good one is remembered.

#![cfg(all(feature = "gateway", unix))]

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::Duration;

use pmt_rpc::client::{Client, Inbox};
use pmt_rpc::control::Purpose;
use serde_json::{Value, json};

use self::common::{daemon, mock_home};

const PASSWORD: &str = "daemon-sudo-password";

fn fake_sudo(dir: &Path) -> String {
    let cache = dir.join("sudo-cache");
    format!(
        r#"#!/bin/sh
typed=""; check=0
while [ $# -gt 0 ]; do
  case "$1" in
    -S) IFS= read -r typed || true; shift ;;
    -p) shift 2 ;;
    -v) check=1; shift ;;
    -*) shift ;;
    *) break ;;
  esac
done
if [ -f "{cache}" ]; then :
elif [ "$typed" = "{PASSWORD}" ]; then touch "{cache}"
else echo "sudo: a terminal is required to read the password" >&2; exit 1
fi
[ "$check" = 1 ] && exit 0
rm -f "{cache}"
exec "$@"
"#,
        cache = cache.display()
    )
}

/// Two sudo commands, then a summary of their results.
fn script(sudo: &Path) -> String {
    let call = |n: u8| {
        format!(
            "[[rule]]\nstep = {n}\n[[rule.tool_calls]]\nname = \"terminal\"\narguments = {{ command = \"{} sh -c 'echo elevated-{n}'\" }}\n\n",
            sudo.display()
        )
    };
    format!(
        "{}{}[[rule]]\nstep = 2\ntool_result_regex = \"elevated-1\"\ntext = \"both ran\"\n\n[[rule]]\nstep = 2\ntext = \"refused\"\n",
        call(0),
        call(1)
    )
}

/// Runs the script in a new session; answers every `sudo.request` with the
/// password and returns the answer and how many were asked.
async fn run(client: &Client, inbox: &mut Inbox, work: &Path, extra: Value) -> (String, usize) {
    let sudo = work.join("bin/sudo");
    let script_path = work.join("script.toml");
    std::fs::write(&script_path, script(&sudo)).unwrap();
    let mut params = json!({
        "cwd": work,
        "setup": {"mock_script": script_path, "yolo": true},
    });
    params
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let created = client.call("session.create", params).await.unwrap();
    let session = created["session_id"].as_str().unwrap().to_owned();
    client
        .call(
            "prompt.submit",
            json!({"session_id": session, "text": "update"}),
        )
        .await
        .unwrap();
    let mut asked = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_mins(1);
    loop {
        tokio::select! {
            request = inbox.requests.recv() => {
                let request = request.expect("the connection stays open");
                assert_eq!(request.method, "sudo.request", "{request:?}");
                assert!(request.params["command"].as_str().unwrap().contains("elevated-"));
                asked += 1;
                client.respond(&request.id, Ok(json!({"password": PASSWORD})));
            }
            event = inbox.events.recv() => {
                let event = event.expect("the connection stays open");
                if event.event == "turn.finished" {
                    let answer = event.data["final_response"].as_str().unwrap_or_default();
                    return (answer.to_owned(), asked);
                }
            }
            () = tokio::time::sleep_until(deadline) => panic!("the turn did not finish"),
        }
    }
}

fn session(extra: Value) -> (String, usize) {
    let home = mock_home();
    let work = home.path().join("work");
    std::fs::create_dir_all(work.join("bin")).unwrap();
    let sudo = work.join("bin/sudo");
    std::fs::write(&sudo, fake_sudo(&work)).unwrap();
    std::fs::set_permissions(&sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let (client, mut inbox) = Client::connect(
            &daemon::record(home.path()).endpoint,
            Purpose::Interactive,
            vec!["default".into()],
            &daemon::hello(true),
        )
        .await
        .unwrap();
        run(&client, &mut inbox, &work, extra).await
    })
}

/// Scenario: Interactive sudo, in a daemon-hosted session: the clients are
/// asked once (`sudo.request`) and the second command reuses the answer.
#[test]
fn an_interactive_daemon_session_asks_its_clients_once() {
    let (answer, asked) = session(json!({}));
    assert_eq!(answer, "both ran");
    assert_eq!(asked, 1);
}

/// A one-shot session has no secure prompt: nobody is asked and the
/// command fails as sudo does without a password.
#[test]
fn a_one_shot_session_never_asks() {
    let (answer, asked) = session(json!({"source_tag": "oneshot"}));
    assert_eq!(answer, "refused");
    assert_eq!(asked, 0);
}
