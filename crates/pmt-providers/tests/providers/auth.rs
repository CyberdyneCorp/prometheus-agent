//! Credentials and subscription sign-in against a scripted test identity
//! provider and recorded provider fixtures (spec: model-providers,
//! Credentials are bound to their authoritative hosts; Subscription and
//! OAuth sign-in registry; Device-code and browser PKCE sign-in flows;
//! Anthropic Claude Max sign-in; OpenAI Codex ChatGPT plan sign-in).

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use pmt_config::schema::SecretsBackend;
use pmt_config::secrets::SecretStore;
use pmt_config::{Config, Home, ProfileName};
use pmt_core::error::Error;
use pmt_core::secret::SecretString;
use pmt_providers::auth::browser::Place;
use pmt_providers::auth::{self, Credentials, FlowChoice, SignInUi, Status};
use pmt_providers::resolve::{CredentialExtras, NoExtras, Resolver, Target, TcpProbe, Trace};
use pmt_providers::{ProviderRegistry, Route, StreamEvent};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::idp::{Action, Idp};
use crate::support::{caller, fast, fixture, request, sse};

static PROBE: TcpProbe = TcpProbe {
    timeout: Duration::from_millis(200),
};

/// Collects what a sign-in shows; forwards authorization URLs.
#[derive(Default)]
struct Ui {
    lines: Mutex<Vec<String>>,
    urls: Mutex<Option<mpsc::Sender<String>>>,
}

impl Ui {
    fn with_urls() -> (Self, mpsc::Receiver<String>) {
        let (tx, rx) = mpsc::channel();
        let ui = Self {
            urls: Mutex::new(Some(tx)),
            ..Self::default()
        };
        (ui, rx)
    }

    fn text(&self) -> String {
        self.lines.lock().unwrap().join("\n")
    }
}

impl SignInUi for Ui {
    fn say(&self, text: &str) {
        let url = text
            .strip_prefix("Open this URL")
            .and_then(|t| t.split("\n  ").nth(1));
        if let (Some(url), Some(tx)) = (url, self.urls.lock().unwrap().as_ref()) {
            tx.send(url.to_owned()).unwrap();
        }
        self.lines.lock().unwrap().push(text.to_owned());
    }
}

const LOCAL: Place = Place {
    remote: false,
    can_open_browser: false,
    timeout: Duration::from_secs(20),
};

fn http() -> reqwest::Client {
    pmt_net::client(&pmt_net::OutboundPolicy::default()).unwrap()
}

fn registry(config: &str) -> (Config, ProviderRegistry) {
    let config: Config = toml::from_str(config).unwrap();
    pmt_config::provider_urls::check(&config).unwrap();
    let registry = ProviderRegistry::for_profile(&config, Vec::new()).unwrap();
    (config, registry)
}

/// A profile secrets store in a temporary home.
struct Profile {
    _dir: tempfile::TempDir,
    credentials: Credentials,
}

fn profile() -> Profile {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let home = Home::open(dir.path()).unwrap();
    let profile = home.profile(&ProfileName::default_profile()).unwrap();
    let store = SecretStore::open(&profile, SecretsBackend::File);
    Profile {
        _dir: dir,
        credentials: Credentials::new(store),
    }
}

async fn route(
    config: &Config,
    registry: &ProviderRegistry,
    secrets: &BTreeMap<String, SecretString>,
    extras: &dyn CredentialExtras,
    provider: &str,
) -> Result<Route, Error> {
    let resolver = Resolver {
        registry,
        config,
        secrets,
        extras,
        probe: &PROBE,
        ad_hoc: None,
    };
    let target = Target {
        provider: provider.to_owned(),
        model: Some("m".to_owned()),
        ..Target::default()
    };
    let mut route = resolver
        .resolve_target(&target, &mut Trace::default())
        .await?;
    fast(&mut route);
    Ok(route)
}

fn jwt(claims: &Value) -> String {
    let part = |v: &Value| URL_SAFE_NO_PAD.encode(v.to_string());
    format!("{}.{}.sig", part(&json!({"alg": "none"})), part(claims))
}

fn custom_device_config(idp: &Idp) -> String {
    format!(
        "[providers.acme]\nbase_url = \"https://api.acme.test/v1\"\nauth = \"oauth\"\n\
         [providers.acme.oauth]\nclient_id = \"acme-cli\"\ndevice_authorization_url = \"{}\"\n\
         token_url = \"{}\"\nallowed_hosts = [\"127.0.0.1\"]\n",
        idp.url("/device"),
        idp.url("/token")
    )
}

fn device_start(interval: u64) -> Action {
    Action::Reply(
        200,
        json!({"device_code": "dev-123", "user_code": "ABCD-EFGH",
               "verification_uri": "https://acme.test/activate", "expires_in": 600, "interval": interval}),
    )
}

#[tokio::test]
async fn transient_polling_errors_are_tolerated() {
    let idp = Idp::start(|req, seen| match (req.path.as_str(), seen) {
        ("/device", _) => device_start(1),
        ("/token", 0..=2) => Action::Reset,
        ("/token", _) => Action::Reply(
            200,
            json!({"access_token": "at-acme", "refresh_token": "rt-acme", "expires_in": 3600}),
        ),
        _ => Action::Reply(404, json!({})),
    })
    .await;
    let (_, registry) = registry(&custom_device_config(&idp));
    let ui = Ui::default();
    let done = auth::sign_in(
        &http(),
        registry.lookup("acme").unwrap(),
        FlowChoice::Default,
        &ui,
        LOCAL,
    )
    .await
    .unwrap();
    assert_eq!(done.tokens.access_token.expose(), "at-acme");
    assert_eq!(done.obtained.source, "device");
    let polls = idp.received("/token");
    assert_eq!(polls.len(), 4);
    assert_eq!(
        polls[3].field("grant_type").as_deref(),
        Some("urn:ietf:params:oauth:grant-type:device_code")
    );
    assert!(
        ui.text().contains("https://acme.test/activate"),
        "{}",
        ui.text()
    );
    assert!(ui.text().contains("ABCD-EFGH"));
}

#[tokio::test]
async fn seven_consecutive_poll_failures_end_the_sign_in() {
    let idp = Idp::start(|req, _| match req.path.as_str() {
        "/device" => device_start(0),
        _ => Action::Reset,
    })
    .await;
    let (_, registry) = registry(&custom_device_config(&idp));
    let err = auth::sign_in(
        &http(),
        registry.lookup("acme").unwrap(),
        FlowChoice::Device,
        &Ui::default(),
        LOCAL,
    )
    .await
    .unwrap_err();
    assert_eq!(err.code().code, "auth.login_failed");
    assert_eq!(idp.received("/token").len(), 7);
}

#[tokio::test]
async fn expired_device_code_is_login_expired() {
    let idp = Idp::start(|req, _| match req.path.as_str() {
        "/device" => device_start(0),
        _ => Action::Reply(400, json!({"error": "expired_token"})),
    })
    .await;
    let (_, registry) = registry(&custom_device_config(&idp));
    let err = auth::sign_in(
        &http(),
        registry.lookup("acme").unwrap(),
        FlowChoice::Default,
        &Ui::default(),
        LOCAL,
    )
    .await
    .unwrap_err();
    assert_eq!(err.code().code, "auth.login_expired");
    assert_eq!(err.exit_code(), 78);
}

#[tokio::test]
async fn endpoints_off_the_allowed_hosts_are_never_contacted() {
    let idp = Idp::start(|_, _| Action::Reply(500, json!({}))).await;
    let config = custom_device_config(&idp).replace(
        "allowed_hosts = [\"127.0.0.1\"]",
        "allowed_hosts = [\"login.acme.test\"]",
    );
    let (_, registry) = registry(&config);
    let err = auth::sign_in(
        &http(),
        registry.lookup("acme").unwrap(),
        FlowChoice::Default,
        &Ui::default(),
        LOCAL,
    )
    .await
    .unwrap_err();
    assert_eq!(err.code().code, "auth.host_not_allowed");
    assert!(idp.received("/device").is_empty());
}

/// Plays the browser: answers the authorization URL with a forged callback
/// first, then the genuine one; returns the URL's query and both answers.
fn browser(
    urls: mpsc::Receiver<String>,
) -> std::thread::JoinHandle<(BTreeMap<String, String>, String, String)> {
    std::thread::spawn(move || {
        let url = urls.recv_timeout(Duration::from_secs(20)).unwrap();
        let query: BTreeMap<String, String> = url::Url::parse(&url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        let redirect = url::Url::parse(&query["redirect_uri"]).unwrap();
        let get = |target: &str| {
            use std::io::{Read as _, Write as _};
            let mut stream =
                std::net::TcpStream::connect(("127.0.0.1", redirect.port().unwrap())).unwrap();
            write!(stream, "GET {target} HTTP/1.1\r\nhost: localhost\r\n\r\n").unwrap();
            let mut answer = String::new();
            stream.read_to_string(&mut answer).unwrap();
            answer
        };
        let forged = get(&format!("{}?code=evil&state=forged", redirect.path()));
        let genuine = get(&format!(
            "{}?code=good-code&state={}",
            redirect.path(),
            query["state"]
        ));
        (query, forged, genuine)
    })
}

fn claude_config(idp: &Idp, inference: &str) -> String {
    format!(
        "[providers.anthropic]\nbase_url = \"{inference}\"\nkey_hosts = [\"127.0.0.1\"]\n\
         wire_format = \"anthropic_messages\"\n\
         [providers.anthropic.oauth]\nauthorize_url = \"{}\"\ntoken_url = \"{}\"\nallowed_hosts = [\"127.0.0.1\"]\n",
        idp.url("/authorize"),
        idp.url("/token")
    )
}

/// Signs in to Claude through the browser flow; returns what the browser
/// and the token endpoint saw.
async fn claude_sign_in(
    config: &str,
    creds: &Credentials,
) -> (BTreeMap<String, String>, String, String) {
    let (_, registry) = registry(config);
    let (ui, urls) = Ui::with_urls();
    let browser = browser(urls);
    let done = auth::sign_in(
        &http(),
        registry.lookup("anthropic").unwrap(),
        FlowChoice::Browser,
        &ui,
        LOCAL,
    )
    .await
    .unwrap();
    let text = ui.text();
    let notes = text.find("Claude Pro plans are not eligible").unwrap();
    assert!(
        notes < text.find("Open this URL").unwrap(),
        "plan notes come first:\n{text}"
    );
    creds
        .save("anthropic", "default", &done.tokens, &done.obtained)
        .unwrap();
    browser.join().unwrap()
}

fn claude_idp() -> impl Fn(&crate::idp::Received, usize) -> Action + Send + Sync + 'static {
    |req, _| match req.path.as_str() {
        "/token" => Action::Reply(
            200,
            json!({"access_token": "sk-ant-oat01-claude-max-token", "refresh_token": "sk-ant-ort01-refresh", "expires_in": 28_800}),
        ),
        _ => Action::Reply(404, json!({})),
    }
}

#[tokio::test]
async fn claude_max_sign_in_uses_pkce_and_the_subscription_identity() {
    let idp = Idp::start(claude_idp()).await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header(
            "authorization",
            "Bearer sk-ant-oat01-claude-max-token",
        ))
        .and(header("anthropic-beta", "oauth-2025-04-20"))
        .respond_with(sse("anthropic_tool_call.sse"))
        .mount(&server)
        .await;
    let config = claude_config(&idp, &server.uri());
    let profile = profile();
    let (query, forged, genuine) = claude_sign_in(&config, &profile.credentials).await;
    // Forged callback rejected with 400; only the genuine code was exchanged.
    assert!(forged.starts_with("HTTP/1.1 400"), "{forged}");
    assert!(genuine.starts_with("HTTP/1.1 200"), "{genuine}");
    let exchanges = idp.received("/token");
    assert_eq!(exchanges.len(), 1);
    let exchange = &exchanges[0];
    assert_eq!(exchange.field("code").as_deref(), Some("good-code"));
    assert_eq!(
        exchange.field("state").as_deref(),
        Some(query["state"].as_str())
    );
    assert_eq!(query["code_challenge_method"], "S256");
    let verifier = exchange.field("code_verifier").unwrap();
    assert_eq!(
        pmt_providers::auth::pkce::challenge(&verifier),
        query["code_challenge"]
    );
    // The eligible plan streams, billed to the subscription.
    let (config, registry) = registry(&config);
    let keys = BTreeMap::from([(
        "ANTHROPIC_API_KEY".to_owned(),
        SecretString::new("sk-ant-api03-key"),
    )]);
    let route = route(&config, &registry, &keys, &profile.credentials, "anthropic")
        .await
        .unwrap();
    assert_eq!(route.billing_source(), "subscription:anthropic/default");
    let mut stream = caller().send(&route, &request("hi"), None).await.unwrap();
    assert!(matches!(
        stream.collect().await.last(),
        Some(StreamEvent::Stop { .. })
    ));
    let sent = &server.received_requests().await.unwrap()[0];
    assert!(sent.headers.get("x-api-key").is_none());
}

#[tokio::test]
async fn ineligible_claude_plan_is_reported_clearly_and_never_dead() {
    let idp = Idp::start(claude_idp()).await;
    let server = MockServer::start().await;
    for (fixture_name, status, model_path) in [
        (
            "anthropic_oauth_plan_ineligible.json",
            403,
            "/pro/v1/messages",
        ),
        (
            "anthropic_oauth_no_extra_usage.json",
            400,
            "/max/v1/messages",
        ),
    ] {
        Mock::given(method("POST"))
            .and(path(model_path))
            .respond_with(ResponseTemplate::new(status).set_body_string(fixture(fixture_name)))
            .mount(&server)
            .await;
    }
    for account in ["pro", "max"] {
        let config = claude_config(&idp, &format!("{}/{account}", server.uri()));
        let profile = profile();
        claude_sign_in(&config, &profile.credentials).await;
        let (config, registry) = registry(&config);
        let keys = BTreeMap::from([(
            "ANTHROPIC_API_KEY".to_owned(),
            SecretString::new("sk-ant-api03-key"),
        )]);
        let route = route(&config, &registry, &keys, &profile.credentials, "anthropic")
            .await
            .unwrap();
        let failure = caller()
            .send(&route, &request("hi"), None)
            .await
            .unwrap_err();
        let err = failure.to_route_error(&route);
        assert_eq!(
            err.code().code,
            "auth.plan_not_eligible",
            "{account}: {}",
            err.message()
        );
        assert!(
            err.message().contains("prometheus auth add anthropic"),
            "{}",
            err.message()
        );
        profile.credentials.note_failure(&route, &err).unwrap();
        let sub = profile
            .credentials
            .subscription("anthropic", "default")
            .unwrap()
            .unwrap();
        assert_eq!(sub.status, Status::Ok);
        assert!(sub.ineligible);
        // The API key now carries requests.
        let next = route_for_api_key(&config, &registry, &keys, &profile.credentials).await;
        assert_eq!(next.billing_source(), "api_key:anthropic");
    }
}

async fn route_for_api_key(
    config: &Config,
    registry: &ProviderRegistry,
    keys: &BTreeMap<String, SecretString>,
    creds: &Credentials,
) -> Route {
    route(config, registry, keys, creds, "anthropic")
        .await
        .unwrap()
}

#[tokio::test]
async fn subscription_is_preferred_over_an_api_key_unless_configured() {
    let idp = Idp::start(claude_idp()).await;
    let profile = profile();
    let config = claude_config(&idp, "https://api.anthropic.com");
    claude_sign_in(&config, &profile.credentials).await;
    let keys = BTreeMap::from([(
        "ANTHROPIC_API_KEY".to_owned(),
        SecretString::new("sk-ant-api03-key"),
    )]);
    let (cfg, reg) = registry(&config);
    let preferred = route(&cfg, &reg, &keys, &profile.credentials, "anthropic")
        .await
        .unwrap();
    assert_eq!(
        preferred.credential.describe(),
        "subscription:anthropic/default"
    );
    let api_first = config.replace(
        "[providers.anthropic]\n",
        "[providers.anthropic]\nprefer_credential = \"api_key\"\n",
    );
    let (cfg, reg) = registry(&api_first);
    let keyed = route(&cfg, &reg, &keys, &profile.credentials, "anthropic")
        .await
        .unwrap();
    assert_eq!(keyed.credential.describe(), "secret:ANTHROPIC_API_KEY");
}

fn codex_config(idp: &Idp, inference: &str, port: u16) -> String {
    format!(
        "[providers.openai-codex]\nbase_url = \"{inference}\"\nkey_hosts = [\"127.0.0.1\"]\n\
         [providers.openai-codex.oauth]\nauthorize_url = \"{}\"\ntoken_url = \"{}\"\n\
         device_authorization_url = \"{}\"\ndevice_poll_url = \"{}\"\nredirect_port = {port}\n\
         allowed_hosts = [\"127.0.0.1\"]\n",
        idp.url("/oauth/authorize"),
        idp.url("/oauth/token"),
        idp.url("/api/accounts/deviceauth/usercode"),
        idp.url("/api/accounts/deviceauth/token"),
    )
}

fn chatgpt_idp() -> impl Fn(&crate::idp::Received, usize) -> Action + Send + Sync + 'static {
    |req, seen| match (req.path.as_str(), seen) {
        ("/api/accounts/deviceauth/usercode", _) => Action::Reply(
            200,
            json!({"device_auth_id": "dauth-1", "user_code": "CODX-1234", "interval": "0"}),
        ),
        ("/api/accounts/deviceauth/token", 0) => Action::Reply(404, json!({})),
        ("/api/accounts/deviceauth/token", _) => Action::Reply(
            200,
            json!({"authorization_code": "ac-1", "code_verifier": "server-verifier", "code_challenge": "x"}),
        ),
        ("/oauth/token", _) => {
            let access = jwt(
                &json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acct-77", "chatgpt_plan_type": "plus"}}),
            );
            Action::Reply(
                200,
                json!({"access_token": access, "refresh_token": "rt-chatgpt", "expires_in": 864_000}),
            )
        }
        _ => Action::Reply(404, json!({})),
    }
}

#[tokio::test]
async fn chatgpt_browser_port_busy_continues_with_the_device_code() {
    let idp = Idp::start(chatgpt_idp()).await;
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let (_, registry) = registry(&codex_config(
        &idp,
        "https://chatgpt.com/backend-api/codex",
        port,
    ));
    let ui = Ui::default();
    let done = auth::sign_in(
        &http(),
        registry.lookup("openai-codex").unwrap(),
        FlowChoice::Browser,
        &ui,
        LOCAL,
    )
    .await
    .unwrap();
    assert!(
        ui.text().contains(&format!("Port {port} is in use")),
        "{}",
        ui.text()
    );
    assert!(ui.text().contains("CODX-1234"));
    assert_eq!(done.obtained.source, "device");
    assert_eq!(done.obtained.account_id.as_deref(), Some("acct-77"));
    assert_eq!(done.obtained.plan.as_deref(), Some("plus"));
    let exchange = &idp.received("/oauth/token")[0];
    assert_eq!(exchange.field("code").as_deref(), Some("ac-1"));
    assert_eq!(
        exchange.field("code_verifier").as_deref(),
        Some("server-verifier")
    );
    assert_eq!(
        exchange.field("client_id").as_deref(),
        Some("app_EMoamEEZ73f0CkXaXp7hrann")
    );
}

#[tokio::test]
async fn chatgpt_requests_use_responses_with_the_account_id() {
    let idp = Idp::start(chatgpt_idp()).await;
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/codex/responses"))
        .and(header("chatgpt-account-id", "acct-77"))
        .respond_with(sse("responses_tool_call.sse"))
        .mount(&server)
        .await;
    let config = codex_config(&idp, &format!("{}/codex", server.uri()), 0);
    let (cfg, reg) = registry(&config);
    let done = auth::sign_in(
        &http(),
        reg.lookup("openai-codex").unwrap(),
        FlowChoice::Default,
        &Ui::default(),
        LOCAL,
    )
    .await
    .unwrap();
    let profile = profile();
    profile
        .credentials
        .save("openai-codex", "personal", &done.tokens, &done.obtained)
        .unwrap();
    let route = route(
        &cfg,
        &reg,
        &BTreeMap::new(),
        &profile.credentials,
        "openai-codex",
    )
    .await
    .unwrap();
    assert_eq!(route.wire_format, pmt_config::schema::WireFormat::Responses);
    assert_eq!(route.billing_source(), "subscription:openai-codex/personal");
    let mut stream = caller().send(&route, &request("hi"), None).await.unwrap();
    assert!(matches!(
        stream.collect().await.last(),
        Some(StreamEvent::Stop { .. })
    ));
    let sent = &server.received_requests().await.unwrap()[0];
    let bearer = sent.headers.get("authorization").unwrap().to_str().unwrap();
    assert_eq!(
        bearer,
        format!("Bearer {}", done.tokens.access_token.expose())
    );
}

#[tokio::test]
async fn one_providers_credentials_never_reach_another_host() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse("chat_text.sse"))
        .mount(&server)
        .await;
    let port = server.address().port();
    // deepseek's key may go to its own (overridden) host 127.0.0.1 only; the
    // `mirror` provider names the same secret but lives on `localhost`.
    let config = format!(
        "[providers.deepseek]\nbase_url = \"http://127.0.0.1:{port}/ds\"\nkey_hosts = [\"127.0.0.1\"]\n\
         [providers.mirror]\nbase_url = \"http://localhost:{port}/mirror\"\nkey_secret = \"DEEPSEEK_API_KEY\"\n\
         [providers.openai-codex]\nbase_url = \"http://localhost:{port}/codex\"\n"
    );
    let (cfg, reg) = registry(&config);
    let keys = BTreeMap::from([(
        "DEEPSEEK_API_KEY".to_owned(),
        SecretString::new("sk-deepseek-0123456789"),
    )]);
    let profile = profile();
    let tokens = auth::token::TokenSet {
        access_token: SecretString::new("chatgpt-access-token"),
        refresh_token: None,
        expires_at: None,
        id_token: None,
    };
    profile
        .credentials
        .save(
            "openai-codex",
            "default",
            &tokens,
            &auth::Obtained::default(),
        )
        .unwrap();
    for provider in ["deepseek", "mirror"] {
        let route = route(&cfg, &reg, &keys, &NoExtras, provider).await.unwrap();
        caller()
            .send(&route, &request("hi"), None)
            .await
            .unwrap()
            .collect()
            .await;
    }
    let err = route(&cfg, &reg, &keys, &profile.credentials, "openai-codex")
        .await
        .unwrap_err();
    assert_eq!(err.code().code, "provider.missing_credential");
    let sent = server.received_requests().await.unwrap();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].url.path(), "/ds/chat/completions");
    assert_eq!(
        sent[0].headers.get("authorization").unwrap(),
        "Bearer sk-deepseek-0123456789"
    );
    // The mirror on another host got no credential at all.
    assert_eq!(sent[1].url.path(), "/mirror/chat/completions");
    assert!(sent[1].headers.get("authorization").is_none());
}
