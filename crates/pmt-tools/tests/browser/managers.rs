//! Password-manager login sources of `browser_vault` against stub `op` and
//! `bw` CLIs (spec: security-boundaries, Password manager login sources).
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use pmt_config::schema::VaultKeyStorage;
use pmt_config::secrets::MemoryKeychain;
use pmt_core::secret::SecretString;
use pmt_tools::browser::managers::{Manager, Source};
use pmt_tools::browser::vault::{MaskedPrompt, VaultSession};
use pmt_tools::dispatch::SeamFuture;
use pmt_tools::result::Status;
use pmt_vault::Vault;
use serde_json::{Value, json};
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::{Fixture, run};

/// A stub `bw`: unlocks with the master `m-pass` into the session
/// `sess-123`, which lists one login saved for `$SITE`.
fn bw(dir: &Path, site: &str) -> PathBuf {
    let script = format!(
        r#"#!/bin/sh
case "$1" in
  unlock) [ "$PMT_BW_MASTER" = "m-pass" ] && echo sess-123 || exit 1 ;;
  list) [ "$BW_SESSION" = "sess-123" ] || exit 1
        echo '[{{"id":"it1","name":"Site","type":1,"login":{{"username":"octocat","password":"bw-pw-1","uris":[{{"uri":"{site}/login"}}]}}}}]' ;;
  get) [ "$BW_SESSION" = "sess-123" ] || exit 1
       echo '{{"id":"it1","name":"Site","type":1,"login":{{"username":"octocat","password":"bw-pw-1","uris":[{{"uri":"{site}/login"}}]}}}}' ;;
esac
"#
    );
    stub(dir, "bw", &script)
}

/// A stub `op` that only a service-account token works with.
fn op(dir: &Path) -> PathBuf {
    stub(
        dir,
        "op",
        r#"#!/bin/sh
[ "$OP_SERVICE_ACCOUNT_TOKEN" = "ops_token" ] || exit 1
echo '[{"id":"o1","title":"GitHub","urls":[{"href":"https://github.com/login"}],"additional_information":"octocat"}]'
"#,
    )
}

fn stub(dir: &Path, name: &str, script: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Answers `master` and counts the questions.
struct Human(&'static str, AtomicUsize);

impl MaskedPrompt for Human {
    fn ask<'a>(&'a self, _: &'a str, _: &'a str) -> SeamFuture<'a, Option<SecretString>> {
        self.1.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Some(SecretString::new(self.0)) })
    }
}

fn session(
    fixture: &Fixture,
    exec: &Arc<pmt_tools::browser::BrowserExec>,
    prompt: Option<Arc<dyn MaskedPrompt>>,
    managers: Vec<Arc<Source>>,
) -> VaultSession {
    VaultSession {
        vault: Arc::new(Vault::of(
            &fixture.profile,
            Arc::new(MemoryKeychain::default()),
        )),
        storage: VaultKeyStorage::File,
        browser: Arc::clone(exec),
        prompt,
        managers,
    }
}

fn body(result: &pmt_tools::result::ToolResult) -> Value {
    serde_json::from_str(&result.text_content()).unwrap_or_default()
}

#[tokio::test]
async fn headless_session() {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let exec = Arc::new(fixture.exec(&manager, false));
    let program = bw(&fixture.profile, "https://example.com");
    let source = Arc::new(Source::new(Manager::Bitwarden, program, None));
    let cron = session(&fixture, &exec, None, vec![source]);
    let listed = cron.call("ses_b", &json!({"action": "list"})).await;
    assert_eq!(
        body(&listed)["sources"],
        json!([{"source": "bitwarden", "status": "unavailable_in_this_session"}])
    );
    let unlock = cron
        .call("ses_b", &json!({"action": "unlock", "source": "bitwarden"}))
        .await;
    assert_eq!(body(&unlock)["status"], "unavailable_in_this_session");

    // 1Password with a service-account token works headless.
    let op = Source::new(
        Manager::OnePassword,
        op(&fixture.profile),
        Some(SecretString::new("ops_token")),
    );
    let cron = session(&fixture, &exec, None, vec![Arc::new(op)]);
    let listed = body(&cron.call("ses_b", &json!({"action": "list"})).await);
    assert_eq!(listed["sources"][0]["status"], "unlocked");
    assert_eq!(listed["items"][0]["handle"], "op_o1");
    assert_eq!(listed["items"][0]["origins"], json!(["https://github.com"]));
}

#[tokio::test]
async fn an_unlocked_manager_login_fills_only_on_its_saved_website() {
    let server = MockServer::start().await;
    Mock::given(path("/login"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<!doctype html><title>Sign in</title><input id=user autocomplete=username><input id=pw type=password>",
            "text/html; charset=utf-8",
        ))
        .mount(&server)
        .await;
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let exec = Arc::new(fixture.exec(&manager, false));
    let program = bw(&fixture.profile, &server.uri());
    let human = Arc::new(Human("m-pass", AtomicUsize::new(0)));
    let source = Arc::new(Source::new(Manager::Bitwarden, program, None));
    let attended = session(&fixture, &exec, Some(human.clone()), vec![source]);
    let before = body(&attended.call("ses_b", &json!({"action": "list"})).await);
    assert_eq!(before["sources"][0]["status"], "locked");
    let unlocked = attended
        .call("ses_b", &json!({"action": "unlock", "source": "bitwarden"}))
        .await;
    assert_eq!(
        body(&unlocked)["status"],
        "unlocked",
        "{}",
        unlocked.text_content()
    );
    assert_eq!(human.1.load(Ordering::SeqCst), 1);
    let listed = attended.call("ses_b", &json!({"action": "list"})).await;
    assert!(listed.text_content().contains("bw_it1") && !listed.text_content().contains("bw-pw-1"));

    let (opened, out) = run(
        &exec,
        &format!("new_tab('{}/login'); wait_for_load()", server.uri()),
    )
    .await;
    assert_eq!(opened.status, Status::Ok, "{out}");
    let filled = attended
        .call("ses_b", &json!({"action": "fill", "handle": "bw_it1"}))
        .await;
    assert_eq!(
        body(&filled)["filled_fields"],
        2,
        "{}",
        filled.text_content()
    );

    // Another origin of the same host (another port) is not the saved site.
    let other = MockServer::start().await;
    Mock::given(path("/login"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<!doctype html><input id=user autocomplete=username><input id=pw type=password>",
            "text/html; charset=utf-8",
        ))
        .mount(&other)
        .await;
    let (opened, out) = run(
        &exec,
        &format!("new_tab('{}/login'); wait_for_load()", other.uri()),
    )
    .await;
    assert_eq!(opened.status, Status::Ok, "{out}");
    let refused = attended
        .call("ses_b", &json!({"action": "fill", "handle": "bw_it1"}))
        .await;
    assert_eq!(
        refused.status,
        Status::Blocked,
        "{}",
        refused.text_content()
    );
}
