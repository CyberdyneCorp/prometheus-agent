//! The M2 MCP client end to end (spec: plugin-system, MCP OAuth for HTTP
//! servers; MCP mTLS and identity header; mcp CLI; MCP catalog entries;
//! MCP sampling; MCP elicitation; MCP tool costs and spend limits;
//! tool-system, Registry toolset aliases): presets, tool checklists,
//! OAuth logins and parked servers against a mock authorization server,
//! per-profile identity headers, catalog installs, and sampling,
//! elicitation and spend limits in unattended sessions.

mod common;

use std::io::{BufRead as _, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};

use self::common::{
    append_config, command, mock_home, newest_session, ok, run, session_messages, stderr,
};
use serde_json::json;
use wiremock::matchers::{basic_auth, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_KEY: &str = include_str!("../../pmt-update/tests/fixtures/test.key");

fn fixture() -> String {
    pmt_mcp::testkit::fixture().to_string_lossy().into_owned()
}

/// The fixture serving streamable HTTP (killed on drop).
struct HttpFixture {
    child: Child,
    url: String,
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn http_fixture(log: &Path, bearer: Option<&str>) -> HttpFixture {
    let mut args = vec![
        "--http".to_owned(),
        "127.0.0.1:0".to_owned(),
        "--log".to_owned(),
        log.to_string_lossy().into_owned(),
    ];
    if let Some(token) = bearer {
        args.extend(["--bearer".to_owned(), token.to_owned()]);
    }
    let mut child = std::process::Command::new(fixture())
        .args(&args)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let addr = line.trim().strip_prefix("listening ").unwrap().to_owned();
    HttpFixture {
        child,
        url: format!("http://{addr}/mcp"),
    }
}

fn script(home: &Path, body: &str) -> String {
    let path = home.join("script.toml");
    std::fs::write(&path, body).unwrap();
    path.to_string_lossy().into_owned()
}

fn one_shot(home: &Path, profile: Option<&str>, script: &str) -> String {
    let mut cmd = command(home);
    if let Some(profile) = profile {
        cmd.args(["--profile", profile]);
    }
    let out = cmd
        .args(["-z", "go", "--mock-script", script])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn tool_results(home: &Path) -> Vec<String> {
    session_messages(home, &newest_session(home))
        .into_iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| m["content"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn config(home: &Path) -> String {
    std::fs::read_to_string(home.join("profiles/default/config.toml")).unwrap()
}

/// One call of `tool` and a final answer.
fn call_script(home: &Path, tool: &str, arguments: &str) -> String {
    script(
        home,
        &format!(
            r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "{tool}"
arguments = {arguments}

[[rule]]
user = "question 0"
text = "sampled answer"

[[rule]]
text = "finished"
"#
        ),
    )
}

#[test]
fn preset_fills_transport() {
    let home = mock_home();
    let out = run(
        home.path(),
        &["mcp", "add", "my-codex", "--preset", "codex"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let text = config(home.path());
    assert!(text.contains("[mcp.servers.my-codex]"), "{text}");
    assert!(text.contains("transport = \"stdio\""), "{text}");
    assert!(text.contains("command = \"codex\""), "{text}");
    assert!(text.contains("args = [\"mcp-server\"]"), "{text}");
    // Explicit flags win over the preset.
    ok(&run(
        home.path(),
        &[
            "mcp",
            "add",
            "mine",
            "--preset",
            "codex",
            "--command",
            "/opt/codex",
            "--args",
            "serve",
        ],
    ));
    let text = config(home.path());
    assert!(text.contains("command = \"/opt/codex\""), "{text}");
    assert!(text.contains("args = [\"serve\"]"), "{text}");
}

#[test]
fn the_tool_checklist_writes_include_only_for_a_partial_selection() {
    let home = mock_home();
    let fixture = fixture();
    ok(&run(
        home.path(),
        &["mcp", "add", "fx", "--command", &fixture],
    ));
    // Toggle tool 2 (`add`) off.
    let out = common::run_with_stdin(home.path(), &["mcp", "tools", "fx"], "2\n\n");
    assert!(out.status.success(), "{}", stderr(&out));
    let text = config(home.path());
    assert!(text.contains("include = ["), "{text}");
    assert!(!text.contains("\"add\""), "{text}");
    assert!(text.contains("\"echo\""), "{text}");
    // Everything selected again removes the list.
    ok(&common::run_with_stdin(
        home.path(),
        &["mcp", "tools", "fx"],
        "all\n\n",
    ));
    assert!(!config(home.path()).contains("include ="));
}

#[test]
fn bare_server_name_selects_its_toolset() {
    let home = mock_home();
    let fixture = fixture();
    ok(&run(
        home.path(),
        &[
            "mcp",
            "add",
            "github",
            "--command",
            &fixture,
            "--trust",
            "full",
        ],
    ));
    ok(&run(
        home.path(),
        &[
            "config",
            "set",
            "tools.enabled_toolsets",
            "[\"core\", \"github\"]",
        ],
    ));
    let script = call_script(home.path(), "mcp__github__echo", "{ text = \"via alias\" }");
    assert_eq!(one_shot(home.path(), None, &script), "finished\n");
    assert!(
        tool_results(home.path())
            .iter()
            .any(|r| r.contains("via alias"))
    );
    let listed = ok(&run(home.path(), &["toolsets", "list"]));
    assert!(listed.contains("mcp-github"), "{listed}");
    assert!(
        !listed
            .lines()
            .any(|l| l.trim_start_matches(['*', ' ']).starts_with("github ")),
        "{listed}"
    );
}

#[test]
fn colliding_server_name_selects_the_builtin_and_doctor_warns() {
    let home = mock_home();
    let fixture = fixture();
    ok(&run(
        home.path(),
        &["mcp", "add", "memory", "--command", &fixture],
    ));
    let doctor = run(home.path(), &["doctor", "plugins"]);
    let text = format!("{}{}", common::stdout(&doctor), stderr(&doctor));
    assert!(
        text.contains("MCP server 'memory' has the name of built-in toolset 'memory'"),
        "{text}"
    );
    assert!(text.contains("mcp-memory"), "{text}");
}

#[test]
fn profile_identity_per_profile() {
    let home = mock_home();
    let log = home.path().join("fixture.log");
    let server = http_fixture(&log, None);
    for profile in ["alice", "bob"] {
        ok(&run(home.path(), &["profile", "create", profile]));
        let config = home
            .path()
            .join("profiles")
            .join(profile)
            .join("config.toml");
        let before = std::fs::read_to_string(&config).unwrap_or_default();
        let text = before
            + &format!(
                "\n[model]\nprovider = \"mock\"\ndefault = \"mock\"\n\n[mcp.servers.team_api]\ntransport = \"http\"\nurl = \"{}\"\ntrust = \"full\"\nidentity_header = {{ name = \"X-User-Id\", value_from = \"profile\" }}\n",
                server.url
            );
        std::fs::write(&config, text).unwrap();
        let script = call_script(home.path(), "mcp__team_api__echo", "{ text = \"hi\" }");
        assert_eq!(one_shot(home.path(), Some(profile), &script), "finished\n");
    }
    let seen = std::fs::read_to_string(&log).unwrap();
    assert!(seen.contains("header x-user-id: alice"), "{seen}");
    assert!(seen.contains("header x-user-id: bob"), "{seen}");
}

#[test]
fn a_missing_client_certificate_fails_naming_it() {
    let home = mock_home();
    append_config(
        home.path(),
        "\n[mcp.servers.tls]\ntransport = \"http\"\nurl = \"https://127.0.0.1:9/mcp\"\nclient_cert = \"/nonexistent/client.pem\"\n",
    );
    let out = run(home.path(), &["mcp", "test", "tls"]);
    assert_eq!(out.status.code(), Some(69));
    assert!(
        stderr(&out)
            .contains("MCP server 'tls': client certificate not found: /nonexistent/client.pem"),
        "{}",
        stderr(&out)
    );
}

/// A mock OAuth authorization server for MCP server `url`.
struct AuthServer {
    runtime: tokio::runtime::Runtime,
    server: MockServer,
}

impl AuthServer {
    fn start(registration: bool) -> Self {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let server = runtime.block_on(MockServer::start());
        let issuer = server.uri();
        let mut metadata = json!({
            "issuer": issuer,
            "authorization_endpoint": format!("{issuer}/authorize"),
            "token_endpoint": format!("{issuer}/token"),
            "device_authorization_endpoint": format!("{issuer}/device"),
            "authorization_response_iss_parameter_supported": true,
        });
        if registration {
            metadata["registration_endpoint"] = json!(format!("{issuer}/register"));
        }
        let auth = Self { runtime, server };
        auth.json("/.well-known/oauth-authorization-server", 200, &metadata);
        auth.json("/register", 201, &json!({"client_id": "dyn-client"}));
        auth
    }

    fn json(&self, at: &str, status: u16, body: &serde_json::Value) {
        self.runtime.block_on(
            Mock::given(path(at))
                .respond_with(ResponseTemplate::new(status).set_body_json(body))
                .mount(&self.server),
        );
    }

    fn token(&self, status: u16, body: &serde_json::Value) {
        self.runtime.block_on(
            Mock::given(method("POST"))
                .and(path("/token"))
                .respond_with(ResponseTemplate::new(status).set_body_json(body))
                .mount(&self.server),
        );
    }
}

fn oauth_server(home: &Path, name: &str, url: &str) {
    append_config(
        home,
        &format!(
            "\n[mcp.servers.{name}]\ntransport = \"http\"\nurl = \"{url}\"\nauth = \"oauth\"\ntrust = \"full\"\n"
        ),
    );
}

/// Runs `mcp login <name>` and pastes the redirect of the authorization
/// URL it prints (code `c-1`, issuer `iss`); the URL's query and the run.
fn paste_login(
    home: &Path,
    name: &str,
    iss: &str,
) -> (
    std::collections::HashMap<String, String>,
    std::process::Output,
) {
    let mut child = command(home)
        .args(["mcp", "login", name])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut authorize = String::new();
    while !authorize.trim_start().starts_with("http") {
        authorize.clear();
        assert!(
            stdout.read_line(&mut authorize).unwrap() > 0,
            "no URL printed"
        );
    }
    let url = reqwest::Url::parse(authorize.trim()).unwrap();
    let query: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
    let redirect = format!(
        "{}?code=c-1&state={}&iss={iss}",
        query["redirect_uri"], query["state"]
    );
    writeln!(child.stdin.take().unwrap(), "{redirect}").unwrap();
    (query, child.wait_with_output().unwrap())
}

/// A pre-registered confidential client (`oauth.client_id` and
/// `oauth.client_secret_name`) at an authorization server whose `path`
/// endpoint answers `body` only when the client authenticates with its
/// secret, and `invalid_client` otherwise.
fn confidential_client(home: &Path, auth: &AuthServer, at: &str, body: &serde_json::Value) {
    auth.runtime.block_on(async {
        Mock::given(method("POST"))
            .and(path(at))
            .and(basic_auth("pre-client", "client-s3cret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .with_priority(1)
            .mount(&auth.server)
            .await;
        Mock::given(method("POST"))
            .and(path(at))
            .respond_with(
                ResponseTemplate::new(401).set_body_json(json!({"error": "invalid_client"})),
            )
            .with_priority(2)
            .mount(&auth.server)
            .await;
    });
    oauth_server(home, "linear", &format!("{}/mcp", auth.server.uri()));
    append_config(
        home,
        "\n[mcp.servers.linear.oauth]\nclient_id = \"pre-client\"\nclient_secret_name = \"LINEAR_CLIENT_SECRET\"\n",
    );
    ok(&common::run_with_stdin(
        home,
        &["secrets", "set", "LINEAR_CLIENT_SECRET"],
        "client-s3cret\n",
    ));
}

fn tokens() -> serde_json::Value {
    json!({"access_token": "at-1", "refresh_token": "rt-1", "expires_in": 3600})
}

/// Scenario (plugin-system, MCP OAuth for HTTP servers): a server without
/// dynamic registration signs in with the configured confidential client,
/// whose secret authenticates the code exchange.
#[test]
fn a_preregistered_confidential_client_signs_in_with_its_secret() {
    let home = mock_home();
    let auth = AuthServer::start(false);
    confidential_client(home.path(), &auth, "/token", &tokens());
    let (query, out) = paste_login(home.path(), "linear", &auth.server.uri());
    assert_eq!(query["client_id"], "pre-client");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let secrets = ok(&run(home.path(), &["secrets", "list"]));
    assert!(secrets.contains("MCP_OAUTH_LINEAR_ACCESS"), "{secrets}");
}

/// The device flow authenticates a confidential client on the device
/// authorization request and on every token poll.
#[test]
fn a_confidential_client_device_login_sends_its_secret() {
    let home = mock_home();
    let auth = AuthServer::start(false);
    confidential_client(home.path(), &auth, "/token", &tokens());
    let started = json!({
        "device_code": "dc-1",
        "user_code": "ABCD-EFGH",
        "verification_uri": format!("{}/activate", auth.server.uri()),
        "interval": 0,
    });
    auth.runtime.block_on(
        Mock::given(method("POST"))
            .and(path("/device"))
            .and(basic_auth("pre-client", "client-s3cret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&started))
            .mount(&auth.server),
    );
    let out = run(home.path(), &["mcp", "login", "linear", "--flow", "device"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        common::stdout(&out).contains("ABCD-EFGH"),
        "{}",
        common::stdout(&out)
    );
    let secrets = ok(&run(home.path(), &["secrets", "list"]));
    assert!(secrets.contains("MCP_OAUTH_LINEAR_ACCESS"), "{secrets}");
}

#[test]
fn dcr_less_server_without_client() {
    let home = mock_home();
    let auth = AuthServer::start(false);
    let url = format!("{}/mcp", auth.server.uri());
    oauth_server(home.path(), "drive", &url);
    let out = run(home.path(), &["mcp", "login", "drive"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("MCP server 'drive' needs a pre-registered OAuth client"),
        "{}",
        stderr(&out)
    );
    assert!(stderr(&out).contains("mcp.servers.drive.oauth.client_id"));
}

#[test]
fn a_login_takes_a_pasted_redirect_and_the_server_then_answers() {
    let home = mock_home();
    let log = home.path().join("fixture.log");
    let server = http_fixture(&log, Some("at-1"));
    let auth = AuthServer::start(true);
    auth.token(
        200,
        &json!({"access_token": "at-1", "refresh_token": "rt-1", "expires_in": 3600}),
    );
    // Discovery runs against the authorization server's origin (no
    // protected resource metadata there: the origin is the issuer); the
    // token is then used against the fixture, which requires it.
    let mcp_url = format!("{}/mcp", auth.server.uri());
    oauth_server(home.path(), "linear", &mcp_url);
    let (query, out) = paste_login(home.path(), "linear", &auth.server.uri());
    assert_eq!(query["client_id"], "dyn-client");
    assert_eq!(query["code_challenge_method"], "S256");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let secrets = ok(&run(home.path(), &["secrets", "list"]));
    assert!(secrets.contains("MCP_OAUTH_LINEAR_ACCESS"), "{secrets}");
    assert!(secrets.contains("MCP_OAUTH_LINEAR_REFRESH"), "{secrets}");
    // The token now authenticates against a server requiring it.
    let config_path = home.path().join("profiles/default/config.toml");
    let text = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(&config_path, text.replace(&mcp_url, &server.url)).unwrap();
    let tested = run(home.path(), &["mcp", "test", "linear"]);
    assert!(tested.status.success(), "{}", stderr(&tested));
}

#[test]
fn a_redirect_naming_another_issuer_is_refused() {
    let home = mock_home();
    let auth = AuthServer::start(true);
    oauth_server(home.path(), "linear", &format!("{}/mcp", auth.server.uri()));
    let (_, out) = paste_login(home.path(), "linear", "https://evil.example");
    assert_eq!(out.status.code(), Some(78));
    assert!(String::from_utf8_lossy(&out.stderr).contains("RFC 9207"));
}

#[test]
fn dead_refresh_token_parks_the_server() {
    let home = mock_home();
    let auth = AuthServer::start(true);
    auth.token(400, &json!({"error": "invalid_grant"}));
    let url = format!("{}/mcp", auth.server.uri());
    oauth_server(home.path(), "linear", &url);
    // A sign-in whose access token has expired.
    let state = home.path().join("profiles/default/state/mcp-oauth");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("linear.json"),
        json!({
            "issuer": auth.server.uri(),
            "token_endpoint": format!("{}/token", auth.server.uri()),
            "client_id": "dyn-client",
            "expires_at": 1,
            "resource": url,
        })
        .to_string(),
    )
    .unwrap();
    for (name, value) in [
        ("MCP_OAUTH_LINEAR_ACCESS", "old"),
        ("MCP_OAUTH_LINEAR_REFRESH", "revoked"),
    ] {
        ok(&common::run_with_stdin(
            home.path(),
            &["secrets", "set", name],
            &format!("{value}\n"),
        ));
    }
    let status = ok(&run(home.path(), &["--json", "mcp", "status"]));
    let status: serde_json::Value = serde_json::from_str(&status).unwrap();
    let row = &status["servers"][0];
    assert_eq!(row["state"], "unavailable", "{status}");
    assert_eq!(
        row["reason"],
        "needs re-authorization: prometheus mcp login linear"
    );
    let record = std::fs::read_to_string(state.join("linear.json")).unwrap();
    assert!(record.contains("parked"), "{record}");
    let secrets = ok(&run(home.path(), &["secrets", "list"]));
    assert!(
        !secrets.contains("MCP_OAUTH_LINEAR_REFRESH"),
        "the dead token is dropped"
    );
    // Later starts stay parked without asking the server again.
    let calls = auth
        .runtime
        .block_on(auth.server.received_requests())
        .unwrap();
    let token_calls = calls.iter().filter(|r| r.url.path() == "/token").count();
    ok(&run(home.path(), &["mcp", "status"]));
    let after = auth
        .runtime
        .block_on(auth.server.received_requests())
        .unwrap();
    assert_eq!(
        after.iter().filter(|r| r.url.path() == "/token").count(),
        token_calls
    );
    let doctor = run(home.path(), &["doctor", "plugins"]);
    let text = format!("{}{}", common::stdout(&doctor), stderr(&doctor));
    assert!(text.contains("prometheus mcp login linear"), "{text}");
}

fn sign(data: &[u8]) -> String {
    let key = minisign::SecretKeyBox::from_string(TEST_KEY)
        .unwrap()
        .into_unencrypted_secret_key()
        .unwrap();
    minisign::sign(None, &key, data, None, None)
        .unwrap()
        .into_string()
}

/// Serves `catalog` (signed) as the profile's live catalog.
fn serve_catalog(
    home: &Path,
    catalog: &serde_json::Value,
) -> (tokio::runtime::Runtime, MockServer) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let server = runtime.block_on(MockServer::start());
    let bytes = serde_json::to_vec(catalog).unwrap();
    let signature = sign(&bytes);
    runtime.block_on(async {
        Mock::given(path("/catalog.json"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes))
            .mount(&server)
            .await;
        Mock::given(path("/catalog.json.minisig"))
            .respond_with(ResponseTemplate::new(200).set_body_string(signature))
            .mount(&server)
            .await;
    });
    ok(&run(
        home,
        &[
            "config",
            "set",
            "plugins.catalog_url",
            &format!("{}/catalog.json", server.uri()),
        ],
    ));
    (runtime, server)
}

#[test]
fn large_surface_uses_an_exclude_list() {
    let home = mock_home();
    let fixture = fixture();
    let catalog = json!({
        "schema": "prometheus.plugin_catalog.v1",
        "plugins": [],
        "mcp": [
            {
                "id": "bigsurface", "manifest_version": 1, "transport": "stdio",
                "command": fixture, "args": ["--log", "${logFile}"],
                "description": "Many tools",
                "setup": [{"name": "logFile", "prompt": "Log file"}, {"name": "BIG_TOKEN", "prompt": "Token", "secret": true}],
                "tools": {"default_excluded": ["*_delete*"]},
                "post_install": "Remember to enable it."
            },
            {"id": "future", "manifest_version": 9, "transport": "stdio", "command": "x", "description": "From the future"}
        ],
        "removed": []
    });
    let _served = serve_catalog(home.path(), &catalog);
    let listed = ok(&run(home.path(), &["mcp", "catalog"]));
    assert!(listed.contains("bigsurface  stdio"), "{listed}");
    assert!(
        listed.contains("future") && listed.contains("requires a newer Prometheus"),
        "{listed}"
    );
    let log = home.path().join("big.log");
    let field = format!("logFile={}", log.display());
    let out = run(
        home.path(),
        &[
            "mcp",
            "install",
            "bigsurface",
            "--field",
            &field,
            "--field",
            "BIG_TOKEN=s3cret",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let printed = common::stdout(&out);
    assert!(
        !printed.contains("toggle a tool"),
        "no checklist: {printed}"
    );
    assert!(printed.contains("Remember to enable it."));
    let text = config(home.path());
    assert!(text.contains("exclude = [\"*_delete*\"]"), "{text}");
    assert!(text.contains("enabled = false"), "{text}");
    assert!(
        text.contains(&log.display().to_string()),
        "non-secret values are written: {text}"
    );
    assert!(
        !text.contains("s3cret"),
        "secrets stay in the store: {text}"
    );
    assert!(ok(&run(home.path(), &["secrets", "list"])).contains("BIG_TOKEN"));
    let newer = run(home.path(), &["mcp", "install", "future"]);
    assert_eq!(newer.status.code(), Some(78));
    assert!(stderr(&newer).contains("requires a newer Prometheus"));
}

fn fx_with(home: &Path, extra: &str) -> PathBuf {
    let log = home.join("fx.log");
    append_config(
        home,
        &format!(
            "\n[mcp.servers.fx]\ncommand = \"{}\"\nargs = [\"--extras\", \"--log\", \"{}\"]\ntrust = \"full\"\n{extra}",
            fixture().replace('\\', "\\\\"),
            log.display().to_string().replace('\\', "\\\\")
        ),
    );
    log
}

#[test]
fn unattended_elicitation_declined() {
    let home = mock_home();
    fx_with(home.path(), "");
    let script = call_script(home.path(), "mcp__fx__ask", "{}");
    assert_eq!(one_shot(home.path(), None, &script), "finished\n");
    let results = tool_results(home.path());
    assert!(
        results.iter().any(|r| r.contains("\"action\":\"decline\"")),
        "{results:?}"
    );
}

#[test]
fn sampling_runs_on_the_session_route() {
    let home = mock_home();
    fx_with(home.path(), "");
    let script = call_script(home.path(), "mcp__fx__sample", "{ n = 1 }");
    assert_eq!(one_shot(home.path(), None, &script), "finished\n");
    let results = tool_results(home.path());
    assert!(
        results
            .iter()
            .any(|r| r.contains("ok ") && r.contains("sampled answer")),
        "{results:?}"
    );
}

#[test]
fn a_costed_call_crossing_the_limit_is_refused_where_no_one_can_answer() {
    let home = mock_home();
    let log = fx_with(
        home.path(),
        "spend_limit = 10\n[mcp.servers.fx.tool_costs]\nrender = 20\n",
    );
    let script = call_script(home.path(), "mcp__fx__render", "{ prompt = \"cat\" }");
    // A one-shot run cannot ask: the call is refused and the run ends
    // saying an approval was needed.
    let out = command(home.path())
        .args(["-z", "go", "--mock-script", &script])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("a tool call needed an approval"),
        "{}",
        stderr(&out)
    );
    // The one-shot run stops its turn as soon as the gate refuses, so
    // whether the refused call's result row is stored first is a race;
    // the refusal text is pinned where it is built (pmt-mcp
    // `budget_crossing_asks_even_when_trusted`, the gate's once-only
    // test). What must hold here: the server never ran the call.
    let calls = std::fs::read_to_string(log).unwrap_or_default();
    assert!(!calls.contains("call render"), "{calls}");
}

#[test]
fn a_malformed_cost_skips_the_server_naming_the_key() {
    let home = mock_home();
    fx_with(
        home.path(),
        "[mcp.servers.fx.tool_costs]\nrender = \"ten\"\n",
    );
    let status = ok(&run(home.path(), &["--json", "mcp", "status"]));
    let status: serde_json::Value = serde_json::from_str(&status).unwrap();
    assert_eq!(status["servers"][0]["state"], "error");
    assert!(
        status["servers"][0]["reason"]
            .as_str()
            .unwrap()
            .starts_with("mcp.servers.fx.tool_costs.render:"),
        "{status}"
    );
}

/// Scenario (tool-system, MCP tool list changes respect the frozen
/// surface): a server's `list_changed` leaves the running session's tool
/// array alone until its next compaction boundary, which rebuilds it.
#[cfg(unix)]
#[test]
fn a_tool_list_change_reaches_the_session_at_its_compaction_boundary() {
    use expectrl::Expect as _;

    use self::common::pty::{exit, prompt, spawn};

    let home = mock_home();
    fx_with(home.path(), "");
    let script = script(
        home.path(),
        r#"
[[rule]]
user = "go"
[[rule.tool_calls]]
name = "mcp__fx__change_tools"
arguments = {}

[[rule]]
user = "probe"
tools = "mcp__fx__new_tool"
text = "NEW TOOL OFFERED"
repeat = true

[[rule]]
user = "probe"
text = "OLD TOOLS ONLY"
repeat = true

[[rule]]
text = "fine"
repeat = true
"#,
    );
    let mut p = spawn(home.path(), &["--mock-script", &script]);
    prompt(&mut p);
    p.send_line("go").unwrap();
    let notice = "MCP server 'fx' changed its tools";
    let answered = p.expect("fine").unwrap();
    if !String::from_utf8_lossy(answered.before()).contains(notice) {
        p.expect(notice).unwrap();
    }
    // The registry moved on, the session's array did not.
    p.send_line("probe").unwrap();
    p.expect("OLD TOOLS ONLY").unwrap();
    prompt(&mut p);
    let id = newest_session(home.path());
    let tools = exposed(home.path(), &id);
    assert!(
        tools.contains(&"mcp__fx__delete_issue".to_owned()),
        "{tools:?}"
    );
    assert!(
        !tools.contains(&"mcp__fx__new_tool".to_owned()),
        "{tools:?}"
    );
    p.send_line("/compress here 1").unwrap();
    p.expect("compress: compressed").unwrap();
    prompt(&mut p);
    p.send_line("probe").unwrap();
    p.expect("NEW TOOL OFFERED").unwrap();
    prompt(&mut p);
    let tools = exposed(home.path(), &id);
    assert!(tools.contains(&"mcp__fx__new_tool".to_owned()), "{tools:?}");
    assert!(
        !tools.contains(&"mcp__fx__delete_issue".to_owned()),
        "{tools:?}"
    );
    exit(p);
}

/// The tools `tools list --session` reports exposed for session `id`.
#[cfg(unix)]
fn exposed(home: &Path, id: &str) -> Vec<String> {
    ok(&run(home, &["tools", "list", "--session", id]))
        .lines()
        .filter(|l| l.contains(" exposed"))
        .filter_map(|l| l.split_whitespace().next().map(str::to_owned))
        .collect()
}

/// A local git repository with one commit; answers its `file://` URL and
/// the commit SHA.
fn local_repo(home: &Path) -> (String, String) {
    let repo = home.join("server-src");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("README"), "server").unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.org"])
            .args(args)
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", stderr(&out));
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    git(&["init", "--quiet"]);
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "v1"]);
    (
        format!("file://{}", repo.display()),
        git(&["rev-parse", "HEAD"]),
    )
}

/// A catalog entry's bootstrap command runs third-party code: it sees none
/// of the exported environment (an API key) and reaches the network only
/// through the egress proxy, which `security.egress.mode = "none"` closes.
#[cfg(unix)]
#[test]
fn a_bootstrap_command_gets_no_exported_secret_and_obeys_egress() {
    let home = mock_home();
    append_config(home.path(), "\n[security.egress]\nmode = \"none\"\n");
    let (repo, sha) = local_repo(home.path());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let catalog = json!({
        "schema": "prometheus.plugin_catalog.v1",
        "plugins": [],
        "mcp": [{
            "id": "boot", "manifest_version": 1, "transport": "stdio",
            "command": fixture(), "description": "Bootstrapped",
            "source": {"repo": repo, "ref": sha, "bootstrap": [
                "printf '%s' \"$PMT_TEST_API_KEY\" > key.txt",
                "printf '%s' \"$HTTP_PROXY\" > proxy.txt",
                format!("curl -sS --max-time 5 http://127.0.0.1:{port}/ > curl.txt 2>&1 || true"),
            ]}
        }],
        "removed": []
    });
    let _served = serve_catalog(home.path(), &catalog);
    let out = command(home.path())
        .args(["mcp", "install", "boot"])
        .env("PMT_TEST_API_KEY", "sk-leaked")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let dir = home.path().join("profiles/default/mcp/boot");
    assert_eq!(std::fs::read_to_string(dir.join("key.txt")).unwrap(), "");
    let proxy = std::fs::read_to_string(dir.join("proxy.txt")).unwrap();
    assert!(proxy.starts_with("http://127.0.0.1:"), "{proxy}");
    let curl = std::fs::read_to_string(dir.join("curl.txt")).unwrap_or_default();
    assert!(listener.accept().is_err(), "the server was reached: {curl}");
    let audit = std::fs::read_to_string(
        home.path()
            .join("profiles/default/logs/security-audit.jsonl"),
    )
    .unwrap_or_default();
    assert!(
        audit.contains("egress_denied") && audit.contains("mcp-bootstrap:boot"),
        "{audit}\n{curl}"
    );
}
