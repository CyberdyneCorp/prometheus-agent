//! `pmt-mcp-fixture`: the tests' reference MCP server (see
//! `pmt_mcp::fixture`).
//!
//! `[--log <file>] [--mode normal|crash-on-start|hang] [--extras]
//! [--offers resources,prompts] [--http <addr> [--bearer <token>]]
//! [--start-delay-ms <n>]`:
//! stdio by default; `--http` serves streamable HTTP
//! and prints `listening <addr>`; `crash-on-start` exits at once and
//! `hang` never answers `initialize`; `--start-delay-ms` waits that long
//! before serving (a slow server).

use std::io::Write as _;

use pmt_mcp::fixture::{Fixture, serve_http};
use rmcp::ServiceExt as _;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let offers = value("--offers").unwrap_or_default();
    let fixture = Fixture {
        log: value("--log").map(Into::into),
        extras: args.iter().any(|a| a == "--extras"),
        resources: offers.split(',').any(|o| o == "resources"),
        prompts: offers.split(',').any(|o| o == "prompts"),
        ..Fixture::default()
    };
    match value("--mode").as_deref() {
        Some("crash-on-start") => std::process::exit(2),
        Some("hang") => loop {
            tokio::time::sleep(std::time::Duration::from_hours(1)).await;
        },
        _ => {}
    }
    if let Some(delay) = value("--start-delay-ms").and_then(|d| d.parse().ok()) {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
    if let Some(addr) = value("--http") {
        let listener = tokio::net::TcpListener::bind(&addr).await.expect("bind");
        let bound = listener.local_addr().expect("address");
        println!("listening {bound}");
        let _ = std::io::stdout().flush();
        serve_http(fixture, listener, value("--bearer"))
            .await
            .expect("serve");
        return;
    }
    let running = fixture
        .serve(rmcp::transport::stdio())
        .await
        .expect("initialize");
    let _ = running.waiting().await;
}
