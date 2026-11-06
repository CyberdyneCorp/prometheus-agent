//! The M2 subscription sign-ins against mock identity providers (spec:
//! model-providers, xAI SuperGrok sign-in; Nous Portal subscription
//! sign-in; Qwen Portal and MiniMax portal sign-in; OpenRouter OAuth key
//! minting).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use pmt_providers::auth::borrowed::Borrowed;
use pmt_providers::auth::browser::Place;
use pmt_providers::auth::{self, FlowChoice, SignInUi};
use pmt_providers::resolve::CredentialExtras;
use serde_json::json;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::accounts::{Setup, http};
use crate::idp::{Action, Idp};
use crate::support::{caller, request, sse};

/// Collects what a sign-in shows; forwards authorization URLs.
struct Ui {
    lines: Mutex<Vec<String>>,
    urls: mpsc::Sender<String>,
}

impl SignInUi for Ui {
    fn say(&self, text: &str) {
        if let Some(url) = text
            .strip_prefix("Open this URL")
            .and_then(|t| t.split("\n  ").nth(1))
        {
            self.urls.send(url.to_owned()).unwrap();
        }
        self.lines.lock().unwrap().push(text.to_owned());
    }
}

fn ui() -> (Ui, mpsc::Receiver<String>) {
    let (urls, rx) = mpsc::channel();
    (
        Ui {
            lines: Mutex::default(),
            urls,
        },
        rx,
    )
}

const LOCAL: Place = Place {
    remote: false,
    can_open_browser: false,
    timeout: Duration::from_secs(20),
};

/// `GET target` on the loopback `port`; the raw answer.
fn get(port: u16, target: &str) -> String {
    use std::io::{Read as _, Write as _};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(stream, "GET {target} HTTP/1.1\r\nhost: localhost\r\n\r\n").unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    answer
}

/// Plays the browser of a state-carrying PKCE sign-in: calls back with the
/// authorization URL's `state` and code `good-code`.
fn approve(urls: mpsc::Receiver<String>) -> std::thread::JoinHandle<BTreeMap<String, String>> {
    std::thread::spawn(move || {
        let url = urls.recv_timeout(Duration::from_secs(20)).unwrap();
        let query: BTreeMap<String, String> = url::Url::parse(&url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        let redirect = url::Url::parse(&query["redirect_uri"]).unwrap();
        let answer = get(
            redirect.port().unwrap(),
            &format!(
                "{}?code=good-code&state={}",
                redirect.path(),
                query["state"]
            ),
        );
        assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
        query
    })
}

/// Answers the token endpoint (and a device-code start) with tokens.
fn portal_idp() -> impl Fn(&crate::idp::Received, usize) -> Action + Send + Sync + 'static {
    |req, _| match req.path.as_str() {
        "/device" => Action::Reply(
            200,
            json!({"device_code": "dc-1", "user_code": "NOUS-1234", "verification_uri": "https://portal.example/device", "interval": 0, "expires_in": 600}),
        ),
        "/token" => Action::Reply(
            200,
            json!({"access_token": "portal-access", "refresh_token": "portal-refresh", "expires_in": 3600}),
        ),
        _ => Action::Reply(404, json!({})),
    }
}

/// `[providers.<id>]` pointed at `inference` with its sign-in at `idp`.
fn portal_config(id: &str, idp: &Idp, inference: &str) -> String {
    format!(
        "[providers.{id}]\nbase_url = \"{inference}\"\nkey_hosts = [\"127.0.0.1\"]\nmax_retries = 0\n\
         [providers.{id}.oauth]\nauthorize_url = \"{}\"\ntoken_url = \"{}\"\ndevice_authorization_url = \"{}\"\nallowed_hosts = [\"127.0.0.1\"]\n",
        idp.url("/authorize"),
        idp.url("/token"),
        idp.url("/device"),
    )
}

/// Signs in to `id` with its default flow and stores it as `label`.
async fn sign_in(setup: &Setup, id: &str, label: &str, browser: bool) -> String {
    let (ui, urls) = ui();
    let player = browser.then(|| approve(urls));
    let provider = setup.registry.lookup(id).unwrap();
    let done = auth::sign_in(&http(), provider, FlowChoice::Default, &ui, LOCAL)
        .await
        .unwrap();
    if let Some(player) = player {
        let query = player.join().unwrap();
        assert_eq!(query["code_challenge_method"], "S256");
    }
    setup
        .credentials
        .save(id, label, &done.tokens, &done.obtained)
        .unwrap();
    ui.lines.lock().unwrap().join("\n")
}

#[tokio::test]
async fn tier_restriction_after_sign_in() {
    let idp = Idp::start(portal_idp()).await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex("^/xai/"))
        .respond_with(ResponseTemplate::new(403).set_body_json(json!({
            "error": {"message": "Your subscription tier does not include API access"}
        })))
        .mount(&server)
        .await;
    let config = portal_config("xai-oauth", &idp, &format!("{}/xai/v1", server.uri()));
    let setup = Setup::new(&config, &[]);
    let shown = sign_in(&setup, "xai-oauth", "personal", true).await;
    assert!(shown.contains("SuperGrok or X Premium+"), "{shown}");
    let accounts = setup.accounts(Borrowed::none());
    let route = setup.route(&accounts, "xai-oauth").await.unwrap();
    assert_eq!(route.billing_source(), "subscription:xai-oauth/personal");
    let failed = caller()
        .with_credentials(Arc::clone(&accounts) as _)
        .send(&route, &request("x"), None)
        .await
        .unwrap_err();
    let error = failed.to_route_error(&route);
    assert_eq!(error.code().code, "auth.plan_not_eligible");
    assert_eq!(error.hint(), Some("prometheus auth add xai"));
    let sub = setup
        .credentials
        .subscription("xai-oauth", "personal")
        .unwrap()
        .unwrap();
    assert_eq!(
        sub.status,
        auth::Status::Ok,
        "a tier refusal is not a dead token"
    );
}

#[tokio::test]
async fn one_sign_in_serves_models_and_tools() {
    let idp = Idp::start(portal_idp()).await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex("^/nous/"))
        .respond_with(sse("chat_text.sse"))
        .mount(&server)
        .await;
    let config = format!(
        "{}[web]\nsearch_backend = \"nous\"\n",
        portal_config("nous", &idp, &format!("{}/nous/v1", server.uri()))
    );
    let setup = Setup::new(&config, &[]);
    let shown = sign_in(&setup, "nous", "default", false).await;
    assert!(shown.contains("NOUS-1234"), "{shown}");
    let device = &idp.received("/device")[0];
    assert_eq!(device.field("scope").as_deref(), Some("inference:invoke"));
    let accounts = setup.accounts(Borrowed::none());
    let route = setup.route(&accounts, "nous").await.unwrap();
    let mut stream = caller()
        .with_credentials(Arc::clone(&accounts) as _)
        .send(&route, &request("x"), None)
        .await
        .unwrap();
    stream.collect().await;
    assert_eq!(route.billing_source(), "subscription:nous/default");
    let search = auth::tools::tool_credential(&setup.registry, &setup.keys, &*accounts, "nous")
        .unwrap()
        .unwrap();
    assert_eq!(search.billing_source, "subscription:nous/default");
    assert_eq!(search.token.expose(), "portal-access");
    let routing = auth::tools::tool_routing(&setup.config, "nous");
    assert!(routing.iter().any(|r| r.tool == "web_search" && r.routed));
}

#[tokio::test]
async fn tools_share_the_xai_subscription() {
    let idp = Idp::start(portal_idp()).await;
    let config = portal_config("xai-oauth", &idp, "https://api.x.ai/v1");
    let setup = Setup::new(&config, &[]);
    sign_in(&setup, "xai-oauth", "grok", true).await;
    let accounts = setup.accounts(Borrowed::none());
    assert!(!setup.keys.contains_key("XAI_API_KEY"));
    let used = auth::tools::tool_credential(&setup.registry, &setup.keys, &*accounts, "xai")
        .unwrap()
        .unwrap();
    assert_eq!(used.billing_source, "subscription:xai-oauth/grok");
    assert_eq!(used.token.expose(), "portal-access");
}

#[tokio::test]
async fn portal_and_api_key_providers_stay_separate() {
    let idp = Idp::start(portal_idp()).await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse("chat_text.sse"))
        .mount(&server)
        .await;
    let config = format!(
        "{}{}[providers.alibaba]\nbase_url = \"{uri}/alibaba/v1\"\nkey_hosts = [\"127.0.0.1\"]\n\
         [providers.minimax]\nbase_url = \"{uri}/minimax\"\nkey_hosts = [\"127.0.0.1\"]\n",
        portal_config("qwen-oauth", &idp, &format!("{}/qwen/v1", server.uri())),
        portal_config("minimax-oauth", &idp, &format!("{}/mm", server.uri())),
        uri = server.uri()
    );
    let setup = Setup::new(
        &config,
        &[
            ("DASHSCOPE_API_KEY", "sk-dashscope-0123456789"),
            ("MINIMAX_API_KEY", "sk-minimax-0123456789"),
        ],
    );
    sign_in(&setup, "qwen-oauth", "default", true).await;
    sign_in(&setup, "minimax-oauth", "default", true).await;
    for (_, exchange) in [("qwen", 0), ("minimax", 1)] {
        let token = &idp.received("/token")[exchange];
        assert_eq!(token.field("code").as_deref(), Some("good-code"));
        assert!(token.field("code_verifier").is_some());
    }
    let accounts = setup.accounts(Borrowed::none());
    let label = |provider: &'static str| {
        let setup = &setup;
        let accounts = &accounts;
        async move {
            setup
                .route(accounts, provider)
                .await
                .unwrap()
                .billing_source()
        }
    };
    assert_eq!(label("qwen-oauth").await, "subscription:qwen-oauth/default");
    assert_eq!(label("alibaba").await, "api_key:alibaba");
    assert_eq!(
        label("minimax-oauth").await,
        "subscription:minimax-oauth/default"
    );
    assert_eq!(label("minimax").await, "api_key:minimax");
    // Signing in to a portal never changes the API-key provider's credential.
    assert!(accounts.subscription("alibaba").unwrap().is_none());
    assert!(accounts.subscription("minimax").unwrap().is_none());
    // Refresh tokens live in the secrets store only.
    assert!(!setup.plain_files().contains("portal-refresh"));
}

#[tokio::test]
async fn wrong_callback_path_ignored() {
    let idp = Idp::start(|req, _| match req.path.as_str() {
        "/keys" => Action::Reply(200, json!({"key": "sk-or-v1-minted-0123456789"})),
        _ => Action::Reply(404, json!({})),
    })
    .await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex("^/or/v1/"))
        .respond_with(
            ResponseTemplate::new(401).set_body_json(json!({"error": {"message": "key revoked"}})),
        )
        .mount(&server)
        .await;
    let config = format!(
        "[providers.openrouter]\nbase_url = \"{}/or/v1\"\nkey_hosts = [\"127.0.0.1\"]\nmax_retries = 0\n\
         [providers.openrouter.oauth]\nauthorize_url = \"{}\"\ntoken_url = \"{}\"\nallowed_hosts = [\"127.0.0.1\"]\n",
        server.uri(),
        idp.url("/auth"),
        idp.url("/keys"),
    );
    let mut setup = Setup::new(&config, &[]);
    let (ui, urls) = ui();
    let player = std::thread::spawn(move || {
        let url = urls.recv_timeout(Duration::from_secs(20)).unwrap();
        let query: BTreeMap<String, String> = url::Url::parse(&url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        let callback = url::Url::parse(&query["callback_url"]).unwrap();
        let port = callback.port().unwrap();
        let wrong = get(port, "/cb/other?code=evil");
        let right = get(port, &format!("{}?code=or-code", callback.path()));
        (query, callback.path().to_owned(), wrong, right)
    });
    let provider = setup.registry.lookup("openrouter").unwrap();
    let key = auth::openrouter::mint_key(&http(), provider, &ui, LOCAL)
        .await
        .unwrap();
    let (query, path, wrong, right) = player.join().unwrap();
    assert!(wrong.starts_with("HTTP/1.1 404"), "{wrong}");
    assert!(right.starts_with("HTTP/1.1 200"), "{right}");
    assert!(path.starts_with("/cb/") && path.len() > 20, "{path}");
    assert_eq!(query["code_challenge_method"], "S256");
    let exchanges = idp.received("/keys");
    assert_eq!(exchanges.len(), 1, "only the nonce path exchanged a code");
    assert_eq!(exchanges[0].field("code").as_deref(), Some("or-code"));
    assert!(exchanges[0].field("code_verifier").is_some());
    assert_eq!(key.expose(), "sk-or-v1-minted-0123456789");

    // The key joins the pool with source `oauth`; a 401 marks it dead.
    setup.keys.insert("OPENROUTER_API_KEY".to_owned(), key);
    setup
        .store
        .set_key_origin("openrouter", "secret:OPENROUTER_API_KEY", "oauth")
        .unwrap();
    let accounts = setup.accounts(Borrowed::none());
    let route = setup.route(&accounts, "openrouter").await.unwrap();
    let caller = caller().with_credentials(Arc::clone(&accounts) as _);
    let failed = caller.send(&route, &request("x"), None).await.unwrap_err();
    assert_eq!(
        failed.to_error("openrouter").code().code,
        "auth.reauth_required"
    );
    assert_eq!(
        setup
            .store
            .key_origin("openrouter", "secret:OPENROUTER_API_KEY")
            .unwrap(),
        Some(("oauth".to_owned(), true))
    );
    let again = caller.send(&route, &request("x"), None).await.unwrap_err();
    assert_eq!(
        again.to_error("openrouter").code().code,
        "auth.reauth_required"
    );
    let posts = server.received_requests().await.unwrap().len();
    assert_eq!(posts, 1, "a dead key is never sent again");
}
