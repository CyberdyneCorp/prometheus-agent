//! Diagnostics introduced by an edit, late verdicts, wait modes and
//! server lifecycle (spec: lsp-diagnostics, Diagnostics introduced by an
//! edit; Wait modes and timeouts; Built-in server registry; Server
//! overrides and custom servers).

use std::fs;
use std::sync::Arc;

use pmt_config::schema::Config;
use pmt_tools::environments::{Environments, SessionEnv};
use pmt_tools::lsp::status::Status;
use pmt_tools::result::Status as ResultStatus;
use serde_json::{Value, json};

use crate::support::{Fixture, eventually, findings, patch, read, write};

#[path = "../common/dispatcher.rs"]
mod common;

/// The rendered findings of a tool result.
fn inline(result: &Value) -> Vec<String> {
    result["lsp_diagnostics"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|i| i.as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_patch_reports_only_the_undefined_name_it_introduced() {
    let fx = Fixture::new();
    let lsp = fx.diagnostics(fx.config(""), &fx.app);
    let files = fx.files(&fx.app, Some(lsp.clone()));
    // Warm the server on another file first.
    write(&files, &fx.app.join("warm.py"), "x = 1\n").await;
    let _ = lsp.settle().await;
    let app = fx.app.join("app.py");
    fs::write(&app, "import os\nprint(BAD_old)\n").unwrap();
    read(&files, &app).await;
    let result = patch(&files, &app, "import os\n", "import os\nBAD_foo()\n").await;
    let mut found = inline(&result);
    if found.is_empty() {
        // A loaded machine can miss the inline budget: then the verdict
        // is the next result's.
        found = lsp.settle().await.into_iter().collect();
    }
    let text = found.join("\n");
    assert!(
        text.contains("ERROR [2:1] \"foo\" is not defined [undefined] (pyright)"),
        "{result}\n{text}"
    );
    assert!(
        !text.contains("old"),
        "pre-existing diagnostics are not reported: {text}"
    );
    assert_eq!(result["lint"]["status"], "ok");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cold_servers_verdict_reaches_the_next_tool_result_with_the_earlier_edit_label() {
    let fx = Fixture::new();
    let lsp = fx.diagnostics(
        fx.config_for("pyright", "FAKE_LSP_DELAY_MS = \"400\"", "", ""),
        &fx.app,
    );
    let environments = Environments::process();
    let session = "ses_lsp_cold";
    let _attached = environments.attach(
        session,
        SessionEnv {
            files: Some(fx.files(&fx.app, Some(lsp.clone()))),
            lsp: Some(lsp.clone()),
            ..SessionEnv::default()
        },
    );
    let dispatcher =
        common::dispatcher(&environments, session, &Config::default(), &[], &fx.base).await;
    let app = fx.app.join("app.py");
    let written = common::call(
        &dispatcher,
        "write_file",
        json!({"path": app.display().to_string(), "content": "BAD_cold()\n"}),
    )
    .await;
    assert_eq!(written.status, ResultStatus::Ok);
    let body: Value = serde_json::from_str(&written.text_content()).unwrap();
    assert!(body.get("lsp_diagnostics").is_none(), "{body}");
    eventually("the late verdict", || !lsp.has_pending()).await;
    let next = common::call(
        &dispatcher,
        "read_file",
        json!({"path": app.display().to_string()}),
    )
    .await;
    let text = next.text_content();
    assert!(
        text.contains(&format!(
            "LSP diagnostics introduced by your earlier edit of {}:\nERROR [1:1] \"cold\" is not defined",
            app.display()
        )),
        "{text}"
    );
    let after = common::call(
        &dispatcher,
        "read_file",
        json!({"path": app.display().to_string()}),
    )
    .await;
    assert!(
        !after.text_content().contains("LSP diagnostics"),
        "delivered once"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_edit_during_a_check_is_compared_with_the_first_edits_baseline() {
    let fx = Fixture::new();
    let lsp = fx.diagnostics(
        fx.config_for("pyright", "FAKE_LSP_DELAY_MS = \"300\"", "", ""),
        &fx.app,
    );
    let files = fx.files(&fx.app, Some(lsp.clone()));
    let app = fx.app.join("app.py");
    fs::write(&app, "BAD_one\n").unwrap();
    read(&files, &app).await;
    let first = patch(&files, &app, "BAD_one\n", "BAD_one\nBAD_two\n").await;
    assert!(first.get("lsp_diagnostics").is_none(), "{first}");
    let second = patch(&files, &app, "BAD_two\n", "BAD_two\nBAD_three\n").await;
    assert!(second.get("lsp_diagnostics").is_none(), "{second}");
    // The turn ends: its final answer waits for the pending checks.
    let settled = lsp
        .settle()
        .await
        .expect("findings before the final answer");
    assert!(
        settled.contains("\"two\" is not defined") && settled.contains("\"three\""),
        "{settled}"
    );
    assert!(!settled.contains("\"one\""), "{settled}");
    assert_eq!(
        settled.matches("LSP diagnostics introduced").count(),
        1,
        "{settled}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_server_that_times_out_is_skipped_until_a_restart() {
    let fx = Fixture::new();
    let config = fx.config_for(
        "pyright",
        "FAKE_LSP_HANG = \"1\"",
        "",
        "warmup_timeout_ms = 300",
    );
    let lsp = fx.diagnostics(config, &fx.app);
    let files = fx.files(&fx.app, Some(lsp.clone()));
    write(&files, &fx.app.join("a.py"), "BAD_x\n").await;
    assert_eq!(lsp.settle().await, None);
    assert_eq!(fx.starts(), 1);
    write(&files, &fx.app.join("b.py"), "BAD_y\n").await;
    assert_eq!(lsp.settle().await, None);
    assert_eq!(
        fx.starts(),
        1,
        "a broken server is not contacted:\n{}",
        fx.log()
    );
    Status::of_profile(&fx.profile)
        .request_restart(Some("pyright"))
        .unwrap();
    write(&files, &fx.app.join("c.py"), "BAD_z\n").await;
    assert_eq!(lsp.settle().await, None);
    assert_eq!(fx.starts(), 2, "a restart clears the mark:\n{}", fx.log());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rust_file_in_a_trusted_cargo_workspace_is_checked_by_rust_analyzer() {
    let fx = Fixture::new();
    fs::write(fx.app.join("Cargo.toml"), "[package]\nname = \"app\"\n").unwrap();
    fs::create_dir_all(fx.app.join("src")).unwrap();
    let lsp = fx.diagnostics(fx.config_for("rust-analyzer", "", "", ""), &fx.app);
    let files = fx.files(&fx.app, Some(lsp.clone()));
    let lib = fx.app.join("src/lib.rs");
    fs::write(&lib, "pub fn a() {}\n").unwrap();
    read(&files, &lib).await;
    let result = patch(&files, &lib, "pub fn a() {}", "pub fn a() { BAD_b(); }").await;
    let found = findings(&result, &lsp).await;
    assert!(
        found.contains("\"b\" is not defined [undefined] (rust-analyzer)"),
        "{found}"
    );
    assert!(
        fx.log().contains(&format!("start {}", fx.app.display())),
        "{}",
        fx.log()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_custom_server_checks_its_extensions_before_the_builtins() {
    let fx = Fixture::new();
    let config = fx.config_for("mylang", "", "extensions = [\".ml2\"]", "");
    let lsp = fx.diagnostics(config, &fx.app);
    let files = fx.files(&fx.app, Some(lsp.clone()));
    let result = write(&files, &fx.app.join("a.ml2"), "BAD_q\n").await;
    let found = findings(&result, &lsp).await;
    assert!(
        found.contains("\"q\" is not defined [undefined] (mylang)"),
        "{found}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_server_stops_and_starts_again_on_demand() {
    let fx = Fixture::new();
    let lsp = fx.diagnostics(fx.config("idle_timeout_secs = 1"), &fx.app);
    let files = fx.files(&fx.app, Some(lsp.clone()));
    write(&files, &fx.app.join("a.py"), "x = 1\n").await;
    let _ = lsp.settle().await;
    let status = Status::of_profile(&fx.profile);
    assert_eq!(status.live().len(), 1, "the running server is listed");
    eventually("the idle shutdown", || status.live().is_empty()).await;
    write(&files, &fx.app.join("b.py"), "x = 2\n").await;
    let _ = lsp.settle().await;
    assert_eq!(fx.starts(), 2, "{}", fx.log());
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_language_server_the_result_carries_only_the_syntax_check() {
    let fx = Fixture::new();
    let lsp = fx.diagnostics(fx.config(""), &fx.app);
    let files = fx.files(&fx.app, Some(lsp));
    let result = write(&files, &fx.app.join("notes.txt"), "BAD_x\n").await;
    assert!(result.get("lsp_diagnostics").is_none(), "{result}");
    let broken = write(&files, &fx.app.join("bad.py"), "def (:\n").await;
    assert_eq!(broken["lint"]["status"], "error");
    assert!(broken.get("lsp_diagnostics").is_none(), "{broken}");
    assert_eq!(
        fx.starts(),
        0,
        "a syntax error is fixed before the server is asked"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn full_wait_mode_also_reports_what_the_edit_broke_elsewhere() {
    let fx = Fixture::new();
    let lsp = fx.diagnostics(fx.config("wait_mode = \"full\""), &fx.app);
    let files = fx.files(&fx.app, Some(lsp.clone()));
    let result = write(&files, &fx.app.join("a.py"), "# BREAKS b.py\n").await;
    let found = findings(&result, &lsp).await;
    let b = fx.app.join("b.py");
    assert!(
        found.contains(&format!(
            "{}: ERROR [1:1] broken by an edit elsewhere (pyright)",
            b.display()
        )),
        "{found}"
    );
}

#[test]
fn a_disabled_lsp_builds_no_diagnostics() {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _guard = runtime.enter();
    let fx = Fixture::new();
    let mut config = fx.config("");
    config.enabled = false;
    let setup = pmt_tools::lsp::LspSetup {
        installs: pmt_tools::lsp::install::Installs::of_home(&fx.prometheus, None),
        workspaces: pmt_tools::lsp::workspace::Workspaces::new(
            &pmt_tools::lsp::workspace::TrustInputs {
                start_dir: &fx.app,
                automatic: true,
                user_home: Some(&fx.home),
                exclude_roots: &[],
                trusted_workspaces: &[],
            },
        ),
        config,
        profile_dir: fx.profile.clone(),
        session_id: "s".into(),
        launcher: Arc::new(pmt_tools::lsp::launch::Unconfined),
        toolchain: pmt_tools::lsp::Toolchain::default(),
        handle: runtime.handle().clone(),
    };
    assert!(pmt_tools::lsp::SessionDiagnostics::new(setup, true).is_none());
}
