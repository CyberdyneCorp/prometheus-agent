//! `browser_vault` against a real browser and local pages (spec:
//! security-boundaries, `browser_vault` fills by handle; Vault codes, cards
//! and addresses).

use std::sync::Arc;

use pmt_config::schema::VaultKeyStorage;
use pmt_config::secrets::MemoryKeychain;
use pmt_core::secret::SecretString;
use pmt_tools::browser::vault::{MaskedPrompt, VaultSession};
use pmt_tools::dispatch::SeamFuture;
use pmt_tools::result::{Status, ToolResult};
use pmt_vault::{Card, Item, Kind, Login, Secret, Vault};
use serde_json::{Value, json};
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::{Fixture, run};

const PASSWORD: &str = "vault-pw-7731";

/// `/login` (identifier already typed), `/otp` and `/checkout`.
async fn pages() -> MockServer {
    let server = MockServer::start().await;
    for (route, body) in [
        (
            "/login",
            "<!doctype html><title>Sign in</title><input id=user autocomplete=username value=octocat><input id=pw type=password>",
        ),
        (
            "/otp",
            "<!doctype html><title>Code</title><input id=otp autocomplete=one-time-code>",
        ),
        (
            "/checkout",
            "<!doctype html><title>Pay</title><input id=n autocomplete=cc-name><input id=c autocomplete=cc-number>\
             <input id=e autocomplete=cc-exp><input id=v autocomplete=cc-csc>",
        ),
    ] {
        Mock::given(path(route))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(body.to_owned(), "text/html; charset=utf-8"),
            )
            .mount(&server)
            .await;
    }
    server
}

/// Answers every masked prompt with `answer`.
struct Human(&'static str);

impl MaskedPrompt for Human {
    fn ask<'a>(&'a self, _: &'a str, _: &'a str) -> SeamFuture<'a, Option<SecretString>> {
        Box::pin(async move { Some(SecretString::new(self.0)) })
    }
}

struct Setup {
    fixture: Fixture,
    manager: Arc<pmt_tools::browser::BrowserManager>,
    exec: Arc<pmt_tools::browser::BrowserExec>,
    vault: Arc<Vault>,
}

fn setup() -> Setup {
    let fixture = Fixture::new();
    let manager = fixture.manager();
    let exec = Arc::new(fixture.exec(&manager, false));
    let vault = Arc::new(Vault::of(
        &fixture.profile,
        Arc::new(MemoryKeychain::default()),
    ));
    Setup {
        fixture,
        manager,
        exec,
        vault,
    }
}

impl Setup {
    fn session(&self, prompt: Option<Arc<dyn MaskedPrompt>>) -> VaultSession {
        VaultSession {
            vault: Arc::clone(&self.vault),
            storage: VaultKeyStorage::File,
            browser: Arc::clone(&self.exec),
            prompt,
        }
    }

    fn login(&self, origin: &str, totp: Option<&str>) -> String {
        let item = Item {
            handle: Kind::Login.new_handle(),
            label: "Site".to_owned(),
            secret: Secret::Login(Login {
                origins: vec![origin.to_owned()],
                identifier: "octocat".to_owned(),
                password: PASSWORD.to_owned(),
                totp: totp.map(str::to_owned),
            }),
        };
        self.vault.add(item, VaultKeyStorage::File).unwrap().handle
    }

    async fn open(&self, url: &str) {
        let (result, body) = run(&self.exec, &format!("new_tab('{url}'); wait_for_load()")).await;
        assert_eq!(result.status, Status::Ok, "{body}");
    }

    async fn print(&self, expression: &str) -> String {
        let (_, body) = run(&self.exec, &format!("print(js({expression:?}))")).await;
        body["output"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .to_owned()
    }
}

fn body(result: &ToolResult) -> Value {
    serde_json::from_str(&result.text_content()).unwrap_or(Value::Null)
}

/// Scenario: Result shape; filled values are redacted from later reads.
#[tokio::test]
async fn a_login_fills_by_handle_and_returns_only_the_count_and_origin() {
    let s = setup();
    let site = pages().await;
    let origin = site.uri();
    let handle = s.login(&origin, None);
    s.open(&format!("{origin}/login")).await;
    let result = s
        .session(None)
        .call("ses_b", &json!({"action": "fill", "handle": handle}))
        .await;
    assert_eq!(result.status, Status::Ok, "{}", result.text_content());
    assert_eq!(body(&result), json!({"filled_fields": 1, "origin": origin}));
    assert!(!result.text_content().contains(PASSWORD));
    assert_eq!(
        s.print("document.querySelector('#pw').value").await,
        "[REDACTED:vault]"
    );
    assert_eq!(
        s.print("document.querySelector('#pw').value.length").await,
        PASSWORD.len().to_string()
    );
    let listed = s
        .session(None)
        .call("ses_b", &json!({"action": "list"}))
        .await;
    assert!(listed.text_content().contains("octocat"));
    assert!(!listed.text_content().contains(PASSWORD));
    s.manager.shutdown().await;
    drop(s.fixture);
}

/// Scenario: Origin mismatch.
#[tokio::test]
async fn a_fill_on_another_origin_is_refused_and_types_nothing() {
    let s = setup();
    let site = pages().await;
    // Saved for `localhost`, the page is on 127.0.0.1: another origin.
    let other = site.uri().replace("127.0.0.1", "localhost");
    let handle = s.login(&other, None);
    s.open(&format!("{}/login", site.uri())).await;
    let result = s
        .session(None)
        .call("ses_b", &json!({"action": "fill", "handle": handle}))
        .await;
    assert_eq!(result.status, Status::Blocked, "{}", result.text_content());
    assert!(result.text_content().contains("nothing was typed"));
    assert_eq!(
        s.print("document.querySelector('#pw').value.length").await,
        "0"
    );
    s.manager.shutdown().await;
}

/// A login with a TOTP seed types the current code on its origin without
/// asking; without a seed and without a person the code cannot be asked.
#[tokio::test]
async fn enter_code_types_the_totp_code_on_the_items_origin() {
    let s = setup();
    let site = pages().await;
    let origin = site.uri();
    let seeded = s.login(&origin, Some("JBSWY3DPEHPK3PXP"));
    let plain = s.login(&origin, None);
    s.open(&format!("{origin}/otp")).await;
    let result = s
        .session(None)
        .call("ses_b", &json!({"action": "enter_code", "handle": seeded}))
        .await;
    assert_eq!(
        body(&result)["filled_fields"],
        1,
        "{}",
        result.text_content()
    );
    assert_eq!(
        s.print("/^[0-9]{6}$/.test(document.querySelector('#otp').value)")
            .await,
        "True"
    );
    let unattended = s
        .session(None)
        .call("ses_b", &json!({"action": "enter_code", "handle": plain}))
        .await;
    assert!(unattended.text_content().contains("prompt_unavailable"));
    let device = s
        .session(None)
        .call(
            "ses_b",
            &json!({"action": "enter_code", "method": "passkey"}),
        )
        .await;
    assert!(device.text_content().contains("ask the user"));
    s.manager.shutdown().await;
}

/// `save_login` asks the person for the password; the model never sees it,
/// and contexts without a person answer `prompt_unavailable`.
#[tokio::test]
async fn save_login_asks_the_person_for_the_password() {
    let s = setup();
    let site = pages().await;
    s.open(&format!("{}/login", site.uri())).await;
    let unattended = s
        .session(None)
        .call(
            "ses_b",
            &json!({"action": "save_login", "identifier": "octocat"}),
        )
        .await;
    assert!(unattended.text_content().contains("prompt_unavailable"));
    let human: Arc<dyn MaskedPrompt> = Arc::new(Human("typed-by-human"));
    let saved = s
        .session(Some(human))
        .call(
            "ses_b",
            &json!({"action": "save_login", "identifier": "octocat"}),
        )
        .await;
    assert_eq!(saved.status, Status::Ok, "{}", saved.text_content());
    assert!(!saved.text_content().contains("typed-by-human"));
    let handle = body(&saved)["saved"]["handle"].as_str().unwrap().to_owned();
    match s.vault.get(&handle).unwrap().secret {
        Secret::Login(login) => {
            assert_eq!(login.password, "typed-by-human");
            assert_eq!(login.origins, [site.uri()]);
        }
        _ => panic!("not a login"),
    }
    s.manager.shutdown().await;
}

/// An approved card fill (the dispatcher asked) types every card field.
#[tokio::test]
async fn a_card_fills_the_checkout_fields() {
    let s = setup();
    let site = pages().await;
    let card = Item {
        handle: Kind::Card.new_handle(),
        label: "Visa".to_owned(),
        secret: Secret::Card(Card {
            holder: "Ann Lee".to_owned(),
            number: "4111111111111111".to_owned(),
            expiry: "12/29".to_owned(),
            cvc: "123".to_owned(),
        }),
    };
    let handle = s.vault.add(card, VaultKeyStorage::File).unwrap().handle;
    s.open(&format!("{}/checkout", site.uri())).await;
    let result = s
        .session(None)
        .call("ses_b", &json!({"action": "fill", "handle": handle}))
        .await;
    assert_eq!(
        body(&result)["filled_fields"],
        4,
        "{}",
        result.text_content()
    );
    assert_eq!(
        s.print("document.querySelector('#c').value").await,
        "[REDACTED:vault]"
    );
    s.manager.shutdown().await;
}

/// Moves the page to `to` while the person is typing the code, then
/// answers it: the check before the prompt saw the old origin.
struct NavigatingHuman {
    exec: Arc<pmt_tools::browser::BrowserExec>,
    to: String,
}

impl MaskedPrompt for NavigatingHuman {
    fn ask<'a>(&'a self, _: &'a str, _: &'a str) -> SeamFuture<'a, Option<SecretString>> {
        Box::pin(async move {
            let (result, body) = run(
                &self.exec,
                &format!("goto_url('{}'); wait_for_load()", self.to),
            )
            .await;
            assert_eq!(result.status, Status::Ok, "{body}");
            Some(SecretString::new("424242"))
        })
    }
}

/// Scenario: Origin mismatch, when the page navigates to another origin
/// between the origin check and the fill: nothing is typed there.
#[tokio::test]
async fn a_page_that_moves_to_another_origin_before_the_fill_gets_nothing() {
    let s = setup();
    let site = pages().await;
    let origin = site.uri();
    let handle = s.login(&origin, None);
    s.open(&format!("{origin}/otp")).await;
    // Another port is another origin.
    let other = pages().await;
    let elsewhere = format!("{}/otp", other.uri());
    let human: Arc<dyn MaskedPrompt> = Arc::new(NavigatingHuman {
        exec: Arc::clone(&s.exec),
        to: elsewhere,
    });
    let result = s
        .session(Some(human))
        .call("ses_b", &json!({"action": "enter_code", "handle": handle}))
        .await;
    assert_eq!(result.status, Status::Blocked, "{}", result.text_content());
    assert!(result.text_content().contains("no code was typed"));
    assert_eq!(s.print("location.origin").await, other.uri());
    assert_eq!(
        s.print("document.querySelector('#otp').value.length").await,
        "0"
    );
    s.manager.shutdown().await;
}
