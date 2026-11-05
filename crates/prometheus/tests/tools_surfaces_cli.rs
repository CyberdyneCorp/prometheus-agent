//! Per-surface toolsets, the tool configuration summary and post-setup
//! through the built binary (spec: tool-system, Per-surface default
//! toolsets; Platform bundles; Tool configuration summary and
//! post-setup).

mod common;

use std::path::Path;

use serde_json::Value;

use common::{append_config, command, mock_home, ok, run, stderr, stdout};

fn config(home: &Path) -> String {
    std::fs::read_to_string(home.join("profiles/default/config.toml")).unwrap()
}

fn summary(home: &Path) -> Vec<Value> {
    let doc: Value =
        serde_json::from_str(&ok(&run(home, &["--json", "tools", "summary"]))).unwrap();
    doc["surfaces"].as_array().unwrap().clone()
}

fn surface<'a>(summaries: &'a [Value], key: &str) -> &'a Value {
    summaries
        .iter()
        .find(|s| s["surface"] == key)
        .unwrap_or_else(|| panic!("no surface {key}: {summaries:?}"))
}

#[test]
fn the_summary_lists_each_surfaces_toolsets_providers_and_unavailable_tools() {
    let home = mock_home();
    let home = home.path();
    append_config(
        home,
        "\n[toolsets.surfaces]\ncron = [\"web\", \"file\"]\n\n[toolsets.surfaces.gateway]\ndiscord = [\"search\"]\n",
    );
    let text = ok(&run(home, &["tools", "summary"]));
    for line in [
        "cli: toolsets core\n",
        "api: toolsets api_server\n",
        "cron: toolsets web, file\n",
        "acp: toolsets acp\n",
        "gateway.webhook: toolsets webhook-safe\n",
        "gateway.discord: toolsets search\n",
        "  provider web_search: none (no web backend configured)\n",
        "  web_search unavailable:no web backend configured\n",
    ] {
        assert!(text.contains(line), "{line:?} in:\n{text}");
    }

    let summaries = summary(home);
    let webhook = surface(&summaries, "gateway.webhook");
    assert_eq!(webhook["toolsets"], serde_json::json!(["webhook-safe"]));
    let providers: Vec<&str> = webhook["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["category"].as_str().unwrap())
        .collect();
    assert_eq!(providers, ["web_search", "web_extract"]);
    let unavailable: Vec<&str> = webhook["unavailable"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["tool"].as_str().unwrap())
        .collect();
    assert!(!unavailable.contains(&"terminal"), "{unavailable:?}");
    let cli = surface(&summaries, "cli");
    assert!(
        cli["providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["category"] == "browser" && p["provider"] == "local"),
        "{cli}"
    );
}

/// Scenario (Per-surface default toolsets): an unknown surface key fails
/// validation with exit 78.
#[test]
fn an_unknown_surface_key_fails_validation() {
    for table in [
        "\n[toolsets.surfaces]\ndesk = [\"web\"]\n",
        "\n[toolsets.surfaces.gateway]\nmyspace = [\"web\"]\n",
    ] {
        let home = mock_home();
        append_config(home.path(), table);
        let out = run(home.path(), &["config", "validate"]);
        assert_eq!(out.status.code(), Some(78), "{table}: {}", stderr(&out));
    }
    let home = mock_home();
    append_config(
        home.path(),
        "\n[toolsets.surfaces.gateway]\nmyspace = [\"web\"]\n",
    );
    let out = run(home.path(), &["config", "validate"]);
    assert!(
        stderr(&out).contains("toolsets.surfaces.gateway.myspace: unknown surface"),
        "{}",
        stderr(&out)
    );
}

/// A platform's toolsets are saved under `[toolsets.surfaces.gateway]`,
/// starting from the platform bundle, and listed for that surface.
#[test]
fn a_platform_surface_is_saved_and_listed_from_its_bundle() {
    let home = mock_home();
    let home = home.path();
    ok(&run(
        home,
        &[
            "tools",
            "enable",
            "x_search",
            "--surface",
            "gateway.telegram",
        ],
    ));
    let text = config(home);
    assert!(text.contains("[toolsets.surfaces.gateway]"), "{text}");
    assert!(
        text.contains("telegram = [\"platform-telegram\", \"x_search\"]"),
        "{text}"
    );
    let doc: Value = serde_json::from_str(&ok(&run(
        home,
        &[
            "--json",
            "toolsets",
            "list",
            "--surface",
            "gateway.telegram",
        ],
    )))
    .unwrap();
    let active: Vec<&str> = doc["toolsets"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["active"] == true)
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(active, ["platform-telegram", "x_search"]);
    let out = run(
        home,
        &["tools", "enable", "web", "--surface", "gateway.myspace"],
    );
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
}

#[test]
fn post_setup_of_a_toolset_without_steps_or_an_unknown_one() {
    let home = mock_home();
    let out = ok(&run(home.path(), &["tools", "post-setup", "file"]));
    assert_eq!(out, "toolset file: setup complete\n");
    let out = run(home.path(), &["tools", "post-setup", "fiel"]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("did you mean 'file'"),
        "{}",
        stderr(&out)
    );
}

/// Scenario (Tool configuration summary and post-setup, Browser needs a
/// runtime): without a terminal the install waits for `--yes` (exit 4,
/// nothing downloaded); with it the pinned runtime is fetched from the
/// configured mirror and checked against its pinned digest (the local
/// mirror serves other bytes, so it is refused). The install itself is
/// covered against a test index in `tools_cmd::setup::tests`.
#[test]
fn post_setup_installs_the_browser_runtime_only_after_confirmation() {
    let home = mock_home();
    let home = home.path();
    // A CDP endpoint: no Chromium needed; an empty PATH: no host Python,
    // so the step is the managed `cpython`.
    ok(&run(
        home,
        &["config", "set", "browser.cdp_url", "http://127.0.0.1:9"],
    ));
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let server = runtime.block_on(wiremock::MockServer::start());
    runtime.block_on(
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_bytes(b"not the archive".to_vec()),
            )
            .mount(&server),
    );
    std::fs::write(
        home.join("daemon.toml"),
        format!("[runtimes.mirrors]\nruntimes = \"{}/rt\"\n", server.uri()),
    )
    .unwrap();
    let empty = home.join("empty-path");
    std::fs::create_dir_all(&empty).unwrap();
    let post_setup = |extra: &[&str]| {
        command(home)
            .env("PATH", &empty)
            .args(["tools", "post-setup", "browser"])
            .args(extra)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    };
    let requests = || {
        runtime
            .block_on(server.received_requests())
            .unwrap_or_default()
    };

    let out = post_setup(&[]);
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("browser: install cpython "),
        "{}",
        stdout(&out)
    );
    assert!(
        stderr(&out).contains("confirmation required"),
        "{}",
        stderr(&out)
    );
    assert!(requests().is_empty());

    let out = post_setup(&["--yes"]);
    let pinned = pmt_update::runtimes::index::RuntimeIndex::embedded()
        .artifact("cpython", &pmt_update::runtimes::platform())
        .is_ok();
    if pinned {
        assert_eq!(out.status.code(), Some(77), "{}", stderr(&out));
        assert!(
            requests()
                .iter()
                .any(|r| r.url.path().starts_with("/rt/cpython/")),
            "{:?}",
            requests()
        );
    } else {
        assert_eq!(out.status.code(), Some(69), "{}", stderr(&out));
    }
    assert!(!home.join("runtimes/cpython").exists());
}
