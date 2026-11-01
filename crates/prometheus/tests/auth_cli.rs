//! `prometheus auth add|list|status|remove|import` end to end against a
//! mock identity provider (spec: model-providers, Credential management
//! commands; Subscription and OAuth sign-in registry; Device-code and
//! browser PKCE sign-in flows; OpenAI Codex ChatGPT plan sign-in).

mod common;

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{command, ok, run, run_with_stdin, stderr, stdout, temp_home};

const OPENAI_KEY: &str = "sk-proj-abcdefghijklmnop0123456789";
const REFRESH: &str = "rt-chatgpt-refresh-0123456789abcdef";

fn write_config(home: &Path, text: &str) {
    let path = home.join("profiles/default/config.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn json_out(out: &Output) -> Value {
    serde_json::from_str(&ok(out)).unwrap()
}

fn jwt(claims: &Value) -> String {
    let part = |v: &Value| URL_SAFE_NO_PAD.encode(v.to_string());
    format!("{}.{}.sig", part(&json!({"alg": "none"})), part(claims))
}

fn chatgpt_access() -> String {
    jwt(
        &json!({"https://api.openai.com/auth": {"chatgpt_account_id": "acct-cli", "chatgpt_plan_type": "pro"}}),
    )
}

/// A Codex CLI `auth.json` holding `tokens`.
fn codex_file(home: &Path, tokens: &Value) -> PathBuf {
    let path = home.join("codex-auth.json");
    std::fs::write(
        &path,
        json!({"OPENAI_API_KEY": null, "tokens": tokens}).to_string(),
    )
    .unwrap();
    path
}

/// Every 5-character window of `secret`: the listing may show at most a
/// key's last 4 characters.
fn windows(secret: &str) -> Vec<&str> {
    (0..=secret.len() - 5).map(|i| &secret[i..i + 5]).collect()
}

#[test]
fn key_supplied_on_argv_is_refused() {
    let home = temp_home();
    let out = run(home.path(), &["auth", "add", "openai", "sk-abc123"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("read from the hidden prompt or stdin"),
        "{}",
        stderr(&out)
    );
    assert!(!stderr(&out).contains("sk-abc123"));
}

#[test]
fn listing_never_prints_secrets() {
    let home = temp_home();
    ok(&run_with_stdin(
        home.path(),
        &["auth", "add", "openai"],
        &format!("{OPENAI_KEY}\n"),
    ));
    let access = chatgpt_access();
    let file = codex_file(
        home.path(),
        &json!({"access_token": access, "refresh_token": REFRESH}),
    );
    let import = run(
        home.path(),
        &[
            "auth",
            "import",
            "codex-cli",
            "--path",
            file.to_str().unwrap(),
            "--label",
            "personal",
        ],
    );
    ok(&import);
    assert!(
        stderr(&import).contains("share one rotating refresh-token family"),
        "{}",
        stderr(&import)
    );
    let out = run(home.path(), &["auth", "list", "--json"]);
    let doc = json_out(&out);
    assert_eq!(doc["schema"], "prometheus.auth.v1");
    let rows = doc["credentials"].as_array().unwrap();
    let kinds: Vec<(&str, &str, &str)> = rows
        .iter()
        .map(|r| {
            (
                r["provider"].as_str().unwrap(),
                r["kind"].as_str().unwrap(),
                r["label"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        kinds,
        [
            ("openai-api", "api_key", "default"),
            ("openai-codex", "subscription", "personal")
        ]
    );
    assert_eq!(rows[0]["source"], "file");
    assert_eq!(rows[0]["status"], "ok");
    assert_eq!(rows[1]["source"], "import:codex-cli");
    assert_eq!(rows[1]["plan"], "pro");
    let text = stdout(&out) + &stdout(&run(home.path(), &["auth", "list"]));
    for secret in [OPENAI_KEY, access.as_str(), REFRESH] {
        for window in windows(secret) {
            assert!(
                !text.contains(window),
                "listing shows part of a secret: {window}"
            );
        }
    }
}

#[test]
fn incomplete_external_file_is_refused() {
    let home = temp_home();
    let file = codex_file(home.path(), &json!({"access_token": chatgpt_access()}));
    let out = run(
        home.path(),
        &[
            "auth",
            "import",
            "codex-cli",
            "--path",
            file.to_str().unwrap(),
        ],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("prometheus auth add openai-codex"),
        "{}",
        stderr(&out)
    );
    let secrets = json_out(&run(home.path(), &["secrets", "list", "--json"]));
    assert_eq!(secrets["secrets"], json!([]));
}

#[test]
fn caveat_text_comes_from_data() {
    let home = temp_home();
    let built_in = stdout(&run(home.path(), &["auth", "status", "anthropic"]));
    assert!(
        built_in.contains("Claude Pro plans are not eligible"),
        "{built_in}"
    );
    write_config(
        home.path(),
        "[providers.anthropic]\nplan_notes = [\"Custom note\"]\n",
    );
    let out = run(home.path(), &["auth", "status", "anthropic", "--json"]);
    let doc = json_out(&out);
    assert_eq!(doc["schema"], "prometheus.auth_status.v1");
    assert_eq!(doc["plan_notes"], json!(["Custom note"]));
    let text = stdout(&run(home.path(), &["auth", "status", "anthropic"]));
    assert!(text.contains("note: Custom note"), "{text}");
    assert!(!text.contains("Claude Pro"), "{text}");
}

/// Runs a browser sign-in: reads stderr until the authorization URL,
/// answers its callback like the browser would, and returns stderr.
fn browser_sign_in(cmd: &mut std::process::Command) -> (Option<i32>, String) {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let mut seen = String::new();
    let mut url = None;
    for line in lines.by_ref() {
        let line = line.unwrap();
        seen.push_str(&line);
        seen.push('\n');
        if seen.contains("Open this URL") && line.starts_with("  http") {
            url = Some(line.trim().to_owned());
        }
        if line.starts_with("Waiting for the callback") {
            break;
        }
    }
    let url = url::Url::parse(&url.unwrap_or_else(|| panic!("no URL in:\n{seen}"))).unwrap();
    let query: std::collections::BTreeMap<String, String> =
        url.query_pairs().into_owned().collect();
    let redirect = url::Url::parse(&query["redirect_uri"]).unwrap();
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", redirect.port().unwrap())).unwrap();
    write!(
        stream,
        "GET {}?code=browser-code&state={} HTTP/1.1\r\nhost: localhost\r\n\r\n",
        redirect.path(),
        query["state"]
    )
    .unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    for line in lines {
        seen.push_str(&line.unwrap());
        seen.push('\n');
    }
    (child.wait().unwrap().code(), seen)
}

async fn token_endpoint(server: &MockServer, access: &str) {
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"access_token": access, "refresh_token": REFRESH, "expires_in": 3600}),
        ))
        .mount(server)
        .await;
}

fn claude_config(server: &MockServer) -> String {
    format!(
        "[providers.anthropic.oauth]\nauthorize_url = \"{0}/oauth/authorize\"\ntoken_url = \"{0}/oauth/token\"\nallowed_hosts = [\"127.0.0.1\"]\n",
        server.uri()
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn plan_caveats_are_shown_before_the_sign_in_url() {
    let server = MockServer::start().await;
    token_endpoint(&server, "sk-ant-oat01-cli-token-0123456789").await;
    let home = temp_home();
    write_config(home.path(), &claude_config(&server));
    let (code, text) =
        browser_sign_in(command(home.path()).args(["auth", "add", "anthropic", "--type", "oauth"]));
    assert_eq!(code, Some(0), "{text}");
    let notes = text
        .find("note: Claude Pro plans are not eligible")
        .expect(&text);
    let extra = text.find("extra-usage credits").expect(&text);
    let url = text.find("Open this URL").expect(&text);
    assert!(notes < url && extra < url, "{text}");
    assert!(
        text.contains("signed in to anthropic as 'default'"),
        "{text}"
    );
    let doc = json_out(&run(
        home.path(),
        &["auth", "status", "anthropic", "--json"],
    ));
    assert_eq!(doc["credentials"][0]["kind"], "subscription");
    assert_eq!(doc["credentials"][0]["source"], "browser");
    // The verifier went only to the token endpoint, with the browser's code.
    let exchange = &server.received_requests().await.unwrap()[0];
    let body: Value = serde_json::from_slice(&exchange.body).unwrap();
    assert_eq!(body["code"], "browser-code");
    assert!(!text.contains(body["code_verifier"].as_str().unwrap()));
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_session_prints_the_tunnel_hint() {
    let server = MockServer::start().await;
    token_endpoint(&server, "sk-ant-oat01-cli-token-0123456789").await;
    let home = temp_home();
    write_config(home.path(), &claude_config(&server));
    let mut cmd = command(home.path());
    cmd.env("SSH_CONNECTION", "203.0.113.5 50000 198.51.100.7 22")
        .args(["auth", "add", "anthropic", "--type", "oauth"]);
    let (code, text) = browser_sign_in(&mut cmd);
    assert_eq!(code, Some(0), "{text}");
    let port = text
        .lines()
        .find_map(|l| l.strip_prefix("Waiting for the callback on http://localhost:"))
        .and_then(|rest| rest.split('/').next())
        .expect(&text)
        .to_owned();
    assert!(
        text.contains(&format!("ssh -N -L {port}:127.0.0.1:{port}")),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn browser_flow_port_busy_continues_with_the_device_code() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/usercode"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"device_auth_id": "dauth-cli", "user_code": "CLI1-CODE", "interval": "0"}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/accounts/deviceauth/token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"authorization_code": "ac-cli", "code_verifier": "v-cli"})),
        )
        .mount(&server)
        .await;
    token_endpoint(&server, &chatgpt_access()).await;
    // Another process holds the registered redirect port 1455 (if something
    // already does, the port is just as busy).
    let _taken = std::net::TcpListener::bind("127.0.0.1:1455");
    let home = temp_home();
    write_config(
        home.path(),
        &format!(
            "[providers.openai-codex.oauth]\nauthorize_url = \"{0}/oauth/authorize\"\ntoken_url = \"{0}/oauth/token\"\n\
             device_authorization_url = \"{0}/api/accounts/deviceauth/usercode\"\n\
             device_poll_url = \"{0}/api/accounts/deviceauth/token\"\nallowed_hosts = [\"127.0.0.1\"]\n",
            server.uri()
        ),
    );
    let out = run(home.path(), &["auth", "add", "openai-codex", "--browser"]);
    let text = stderr(&out);
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("Port 1455 is in use"), "{text}");
    assert!(text.contains("CLI1-CODE"), "{text}");
    let doc = json_out(&run(home.path(), &["auth", "list", "--json"]));
    assert_eq!(doc["credentials"][0]["provider"], "openai-codex");
    assert_eq!(doc["credentials"][0]["source"], "device");
    assert_eq!(doc["credentials"][0]["plan"], "pro");
}

#[tokio::test(flavor = "multi_thread")]
async fn remove_by_label_or_index_and_revoke_sign_ins() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/oauth/revoke"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    let home = temp_home();
    write_config(
        home.path(),
        &format!(
            "[providers.openai-codex.oauth]\nrevoke_url = \"{}/oauth/revoke\"\nallowed_hosts = [\"auth.openai.com\", \"127.0.0.1\"]\n",
            server.uri()
        ),
    );
    let file = codex_file(
        home.path(),
        &json!({"access_token": chatgpt_access(), "refresh_token": REFRESH}),
    );
    ok(&run(
        home.path(),
        &[
            "auth",
            "import",
            "codex-cli",
            "--path",
            file.to_str().unwrap(),
        ],
    ));
    ok(&run_with_stdin(
        home.path(),
        &["auth", "add", "deepseek"],
        "sk-deepseek-0123456789\n",
    ));
    let missing = run(home.path(), &["auth", "remove", "deepseek", "work"]);
    assert_eq!(missing.status.code(), Some(66), "{}", stderr(&missing));
    let removed = json_out(&run(
        home.path(),
        &["auth", "rm", "openai-codex", "default", "--json"],
    ));
    assert_eq!(removed["revoked"], true);
    let revoke = &server.received_requests().await.unwrap()[0];
    let fields: Vec<(String, String)> = url::form_urlencoded::parse(&revoke.body)
        .into_owned()
        .collect();
    assert!(
        fields.contains(&("token".to_owned(), REFRESH.to_owned())),
        "{fields:?}"
    );
    ok(&run(home.path(), &["auth", "remove", "deepseek", "1"]));
    let secrets = json_out(&run(home.path(), &["secrets", "list", "--json"]));
    assert_eq!(secrets["secrets"], json!([]));
}
