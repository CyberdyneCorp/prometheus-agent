//! `prometheus security scan` against a wiremock OSV (spec:
//! security-boundaries, Dependency vulnerability scan).

mod common;

use std::path::Path;

use common::{append_config, run, stderr, stdout, temp_home};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A bridge environment pinning requests 2.0.0 and a pinned npx MCP server.
fn profile(home: &Path, osv: &str) {
    let env = home.join("profiles/default/python/envs/tools-0123456789ab");
    std::fs::create_dir_all(&env).unwrap();
    std::fs::write(
        env.join("bridge.lock"),
        "requests==2.0.0 \\\n    --hash=sha256:00\n",
    )
    .unwrap();
    std::fs::create_dir_all(home.join("profiles/default")).unwrap();
    std::fs::write(
        home.join("profiles/default/config.toml"),
        "schema_version = 1\n",
    )
    .unwrap();
    append_config(
        home,
        &format!(
            "[security.scan]\nosv_url = \"{osv}\"\n\n[mcp.servers.gh]\ncommand = \"npx\"\nargs = [\"-y\", \"@acme/mcp-gh@1.0.0\"]\n"
        ),
    );
}

async fn osv() -> MockServer {
    let server = MockServer::start().await;
    // Packages are sent in component order: the MCP server's, then the
    // python environment's.
    Mock::given(method("POST"))
        .and(path("/v1/querybatch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{}, {"vulns": [{"id": "GHSA-reqs-0001"}]}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/vulns/GHSA-reqs-0001"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "GHSA-reqs-0001",
            "summary": "Proxy-Authorization header leak",
            "database_specific": {"severity": "HIGH"},
            "affected": [{"package": {"name": "requests", "ecosystem": "PyPI"},
                          "ranges": [{"type": "ECOSYSTEM", "events": [{"introduced": "0"}, {"fixed": "2.31.0"}]}]}]
        })))
        .mount(&server)
        .await;
    server
}

#[tokio::test(flavor = "multi_thread")]
async fn ci_gate() {
    let server = osv().await;
    let home = temp_home();
    profile(home.path(), &server.uri());
    let root = home.path().to_owned();
    let out =
        pmt_core::spawn::blocking(move || run(&root, &["security", "scan", "--fail-on", "high"]))
            .await
            .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("python environment"), "{text}");
    assert!(
        text.contains("GHSA-reqs-0001 requests@2.0.0  high  fixed in 2.31.0"),
        "{text}"
    );
    let sent = server.received_requests().await.unwrap();
    let batch: serde_json::Value = serde_json::from_slice(&sent[0].body).unwrap();
    assert_eq!(
        batch["queries"][0],
        json!({"package": {"name": "@acme/mcp-gh", "ecosystem": "npm"}, "version": "1.0.0"})
    );

    // Below the gate: listed, exit 0.
    let root = home.path().to_owned();
    let out = pmt_core::spawn::blocking(move || run(&root, &["security", "scan"]))
        .await
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
}

#[test]
fn unreachable_osv_exits_69() {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let home = temp_home();
    profile(home.path(), &format!("http://127.0.0.1:{port}"));
    let out = run(home.path(), &["security", "scan"]);
    assert_eq!(out.status.code(), Some(69), "{}", stderr(&out));
    assert!(stderr(&out).contains("OSV database is unreachable"));
}
