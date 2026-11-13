//! The dashboard's REST inventory and its additional sign-in providers
//! (spec: web-dashboard, REST endpoint inventory; Additional dashboard
//! sign-in providers; Registering a dashboard with the account portal).

#![cfg(feature = "dashboard")]

mod common;

use serde_json::{Value, json};

use common::api::Request;
use common::dashboard::{self, Signed, call, certificate, get, https, sign_in};
use common::{daemon, mock_home, ok, run, run_with_stdin};

fn repo_file(path: &str) -> Value {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    serde_json::from_str(&std::fs::read_to_string(root.join(path)).unwrap()).unwrap()
}

/// `path` of the `OpenAPI` document with its parameters filled in.
fn concrete(path: &str) -> String {
    path.split('/')
        .map(|segment| match segment {
            "{profile}" => "default",
            s if s.starts_with('{') => "nothing",
            s => s,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Scenario: Config schema served from the typed config. Every endpoint of
/// the checked-in `OpenAPI` document is served; anything else answers 404.
#[test]
fn the_inventory_is_served_and_nothing_else() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let signed = sign_in(&opened);
    let schema = get(
        opened.port,
        "/api/profiles/default/config/schema",
        &signed.cookie,
    );
    assert_eq!(schema.status, 200, "{schema:?}");
    assert_eq!(schema.json(), repo_file("schema/config.schema.json"));

    let api = repo_file("schema/dashboard-api.json");
    for (path, item) in api["paths"].as_object().unwrap() {
        for method in item.as_object().unwrap().keys() {
            let method = method.to_ascii_uppercase();
            let answer = call(
                opened.port,
                &method,
                &concrete(path),
                &signed,
                Some(&json!({})),
            );
            let body: Value = serde_json::from_str(&answer.body).unwrap_or_default();
            assert_ne!(
                body["message"], "no such endpoint",
                "{method} {path} is listed but not served"
            );
        }
    }
    for (method, path) in [
        ("GET", "/api/profiles/default/nothing"),
        ("POST", "/api/profiles/default/config"),
        ("GET", "/api/kanban/unlisted"),
    ] {
        let answer = call(opened.port, method, path, &signed, None);
        assert_eq!(answer.status, 404, "{method} {path}: {answer:?}");
    }
}

fn session_count(port: u16, signed: &Signed) -> usize {
    let answer = get(port, "/api/profiles/default/sessions", &signed.cookie);
    answer.json()["sessions"].as_array().unwrap().len()
}

/// Scenario: Bulk maintenance needs confirmation (refused for observers).
#[test]
fn bulk_maintenance_needs_confirmation_and_a_driver() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    let signed = sign_in(&opened);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut ws = dashboard::rpc(opened.port, &signed).await;
        let frame = json!({"jsonrpc": "2.0", "id": 1, "method": "session.create", "params": {"profile": "default"}});
        dashboard::request_on(&mut ws, &frame).await;
    });
    assert_eq!(session_count(opened.port, &signed), 1);
    let path = "/api/profiles/default/maintenance/prune-sessions";
    for body in [None, Some(json!({})), Some(json!({"confirm": "yes"}))] {
        let answer = call(opened.port, "POST", path, &signed, body.as_ref());
        assert_eq!(answer.status, 409, "{answer:?}");
    }
    assert_eq!(
        session_count(opened.port, &signed),
        1,
        "nothing was deleted"
    );

    let paired = ok(&run(
        home.path(),
        &[
            "--json",
            "daemon",
            "pair-device",
            "viewer",
            "--role",
            "observer",
        ],
    ));
    let token = serde_json::from_str::<Value>(&paired).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    let observer = Request::new("POST", path)
        .header("Authorization", &format!("Bearer {token}"))
        .json(&json!({"confirm": true}))
        .send(opened.port);
    assert_eq!(observer.status, 403, "{observer:?}");

    for action in ["vacuum", "rebuild-fts", "prune-snapshots"] {
        let answer = call(
            opened.port,
            "POST",
            &format!("/api/profiles/default/maintenance/{action}"),
            &signed,
            Some(&json!({"confirm": true})),
        );
        assert_eq!(answer.status, 200, "{action}: {answer:?}");
    }
    let pruned = call(
        opened.port,
        "POST",
        path,
        &signed,
        Some(&json!({"confirm": true, "days": 1})),
    );
    assert_eq!(pruned.status, 200, "{pruned:?}");
    assert_eq!(
        pruned.json()["deleted"],
        0,
        "the session is younger than a day"
    );
}

fn token_home(home: &std::path::Path) -> rustls::pki_types::CertificateDer<'static> {
    let cert = certificate(home);
    std::fs::write(
        home.join("daemon.toml"),
        format!(
            "[dashboard]\n[dashboard.auth]\nmode = \"token\"\n[dashboard.tls]\ncert = {:?}\nkey = {:?}\n",
            home.join("cert.pem").to_string_lossy(),
            home.join("key.pem").to_string_lossy()
        ),
    )
    .unwrap();
    cert
}

fn bearer_get(port: u16, path: &str, token: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    )
}

/// Scenario: Token deployment.
#[test]
fn the_deployment_token_opens_the_api_over_tls_and_nothing_else_does() {
    let home = mock_home();
    let cert = token_home(home.path());
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "DASHBOARD_SESSION_TOKEN"],
        "s3cret-deployment-token\n",
    ));
    let _daemon = daemon::start(home.path(), &[]);
    let opened = dashboard::open(home.path(), &[]);
    assert!(opened.url.starts_with("https://"), "{opened:?}");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let path = "/api/profiles/default/sessions";
    let (code, text) = rt.block_on(https(
        opened.port,
        &cert,
        &bearer_get(opened.port, path, "s3cret-deployment-token"),
    ));
    assert_eq!(code, 200, "{text}");
    let (code, _) = rt.block_on(https(
        opened.port,
        &cert,
        &bearer_get(opened.port, path, "wrong"),
    ));
    assert_eq!(code, 401);
    let health = format!(
        "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        opened.port
    );
    let (_, text) = rt.block_on(https(opened.port, &cert, &health));
    assert!(
        text.contains(r#""auth_providers":["login_link","token"]"#),
        "{text}"
    );
}

/// Scenario: Token mode without the secret.
#[test]
fn token_mode_without_its_secret_exits_78_naming_it() {
    let home = mock_home();
    token_home(home.path());
    let _daemon = daemon::start(home.path(), &[]);
    let out = run(
        home.path(),
        &["dashboard", "--no-open", "--host", "0.0.0.0", "--port", "0"],
    );
    assert_eq!(out.status.code(), Some(78), "{out:?}");
    assert!(
        common::stderr(&out).contains("DASHBOARD_SESSION_TOKEN"),
        "{out:?}"
    );
}

/// A fake account portal registering OAuth clients for its signed-in
/// account.
async fn portal() -> wiremock::MockServer {
    use wiremock::matchers::{body_partial_json, header, method, path};
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(method("POST"))
        .and(path("/api/oauth/clients"))
        .and(header("authorization", "Bearer nous-token"))
        .and(body_partial_json(
            json!({"redirect_uris": ["https://dash.example.com/auth/callback"]}),
        ))
        .respond_with(
            wiremock::ResponseTemplate::new(201)
                .set_body_json(json!({"client_id": "cl_dash_1", "name": "home lab"})),
        )
        .expect(1)
        .mount(&server)
        .await;
    server
}

fn portal_sign_in(home: &std::path::Path) {
    let meta = r#"{"provider":"nous","label":"default","source":"device","obtained_at":"2026-01-01T00:00:00Z","status":"ok"}"#;
    for (name, value) in [
        ("OAUTH_NOUS_DEFAULT_ACCESS", "nous-token"),
        ("OAUTH_NOUS_DEFAULT_META", meta),
    ] {
        ok(&run_with_stdin(
            home,
            &["secrets", "set", name],
            &format!("{value}\n"),
        ));
    }
}

/// Scenario: Register with a public callback.
#[test]
fn register_saves_the_portal_client_id() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let server = rt.block_on(portal());
    let home = mock_home();
    std::fs::write(
        home.path().join("daemon.toml"),
        format!(
            "# kept\n[dashboard.auth.portal]\nissuer = \"{}\"\n",
            server.uri()
        ),
    )
    .unwrap();
    let register = [
        "dashboard",
        "register",
        "--name",
        "home lab",
        "--redirect-uri",
        "https://dash.example.com/auth/callback",
    ];
    let unsigned = run(home.path(), &register);
    assert_eq!(unsigned.status.code(), Some(78), "{unsigned:?}");
    assert!(
        common::stderr(&unsigned)
            .contains("sign in to the portal first: prometheus auth login portal"),
        "{unsigned:?}"
    );
    let plain = run(
        home.path(),
        &[
            "dashboard",
            "register",
            "--redirect-uri",
            "http://dash.example.com/auth/callback",
        ],
    );
    assert_eq!(plain.status.code(), Some(2), "{plain:?}");

    portal_sign_in(home.path());
    let text = ok(&run(home.path(), &register));
    assert!(text.contains("home lab"), "{text}");
    let saved = std::fs::read_to_string(home.path().join("daemon.toml")).unwrap();
    assert!(saved.starts_with("# kept\n"), "{saved}");
    let config: toml::Table = toml::from_str(&saved).unwrap();
    assert_eq!(
        config["dashboard"]["auth"]["portal"]["client_id"].as_str(),
        Some("cl_dash_1")
    );
    rt.block_on(server.verify());
}

/// Portal sign-in: only an account owning the dashboard's registered
/// client signs in (as an observer unless listed as a driver).
#[test]
fn a_portal_sign_in_needs_the_account_to_own_the_client() {
    let issuer = common::oidc::start("acct_42");
    for (client, signed_in) in [(common::oidc::OWNED_CLIENT, true), ("cl_foreign", false)] {
        let home = mock_home();
        std::fs::write(
            home.path().join("daemon.toml"),
            common::oidc::portal_toml(&issuer, client),
        )
        .unwrap();
        let _daemon = daemon::start(home.path(), &[]);
        let opened = dashboard::open(home.path(), &[]);
        let done = dashboard::remote_callback(&opened);
        if signed_in {
            assert_eq!(done.status, 303, "{done:?}");
            let viewer = dashboard::sign_in_remote(&opened);
            let me = get(opened.port, "/api/session", &viewer.cookie).json();
            assert_eq!(
                (me["subject"].as_str(), me["role"].as_str()),
                (Some("acct_42"), Some("observer"))
            );
        } else {
            assert_eq!(done.status, 403, "{done:?}");
        }
    }
}
