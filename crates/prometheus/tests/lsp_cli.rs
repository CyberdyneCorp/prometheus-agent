//! `prometheus lsp` and `[lsp]` configuration through the binary (spec:
//! lsp-diagnostics, LSP CLI; Workspace gating; Built-in server registry).

mod common;

use common::{append_config, command, ok, run, stderr, temp_home};
use pmt_tools::lsp::status::{Entry, Status};
use serde_json::Value;

/// `args` with `PATH` holding nothing but an empty directory.
fn run_without_servers(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    let empty = home.join("empty-path");
    std::fs::create_dir_all(&empty).unwrap();
    command(home)
        .env("PATH", &empty)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn which_names_a_missing_server_and_exits_66() {
    let home = temp_home();
    let out = run_without_servers(home.path(), &["lsp", "which", "gopls"]);
    assert_eq!(out.status.code(), Some(66), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("no gopls binary found"),
        "{}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("prometheus lsp install gopls"),
        "{}",
        stderr(&out)
    );
    let manual = run_without_servers(home.path(), &["lsp", "which", "rust-analyzer"]);
    assert_eq!(manual.status.code(), Some(66));
    assert!(
        stderr(&manual).contains("rustup component add rust-analyzer"),
        "{}",
        stderr(&manual)
    );
    let unknown = run_without_servers(home.path(), &["lsp", "which", "pyrite"]);
    assert_eq!(unknown.status.code(), Some(2), "{}", stderr(&unknown));
}

#[test]
fn which_prints_the_configured_command() {
    let home = temp_home();
    let server = home.path().join("bin/mylang-ls");
    std::fs::create_dir_all(server.parent().unwrap()).unwrap();
    std::fs::write(&server, "").unwrap();
    ok(&run(home.path(), &["lsp", "list"]));
    append_config(
        home.path(),
        &format!(
            "\n[lsp.servers.mylang]\ncommand = [\"{}\", \"--stdio\"]\nextensions = [\".ml2\"]\n",
            server.display().to_string().replace('\\', "/")
        ),
    );
    let text = ok(&run(home.path(), &["lsp", "which", "mylang"]));
    assert_eq!(text.trim(), server.display().to_string().replace('\\', "/"));
}

#[test]
fn list_shows_the_registry_with_install_state_and_trust() {
    let home = temp_home();
    let text = ok(&run_without_servers(home.path(), &["lsp", "list"]));
    for id in [
        "rust-analyzer",
        "pyright",
        "gopls",
        "prisma-language-server",
    ] {
        assert!(text.contains(id), "{text}");
    }
    let rust = text
        .lines()
        .find(|l| l.starts_with("rust-analyzer"))
        .unwrap();
    assert!(
        rust.contains("manual") && rust.contains("not installed"),
        "{rust}"
    );
    assert!(rust.contains("trusted workspaces only"), "{rust}");
    let json: Value = serde_json::from_str(&ok(&run_without_servers(
        home.path(),
        &["--json", "lsp", "list"],
    )))
    .unwrap();
    assert_eq!(json["schema"], "prometheus.lsp_list.v1");
    let pyright = json["servers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == "pyright")
        .unwrap();
    assert_eq!(pyright["method"], "npm");
    assert_eq!(pyright["executes_project_code"], false);
    let installed: Value = serde_json::from_str(&ok(&run_without_servers(
        home.path(),
        &["--json", "lsp", "list", "--installed-only"],
    )))
    .unwrap();
    assert_eq!(installed["servers"], serde_json::json!([]));
}

#[test]
fn status_lists_running_servers_and_restart_clears_them() {
    let home = temp_home();
    let text = ok(&run(home.path(), &["lsp", "status"]));
    assert!(text.contains("no running language servers"), "{text}");
    // A server entry of a running process (this test stands in for it).
    let me = std::process::id();
    Status::of_profile(&home.path().join("profiles/default")).write(&Entry {
        id: "rust-analyzer".into(),
        workspace: "/w/app".into(),
        pid: me,
        started: pmt_core::runtime::process_start_time(me).unwrap(),
        state: "ready".into(),
        trusted_only: true,
        session_id: "ses_x".into(),
    });
    let text = ok(&run(home.path(), &["lsp", "status"]));
    assert!(text.contains("rust-analyzer  /w/app  pid "), "{text}");
    assert!(text.contains("trusted workspaces only"), "{text}");
    let json: Value =
        serde_json::from_str(&ok(&run(home.path(), &["--json", "lsp", "status"]))).unwrap();
    assert_eq!(json["schema"], "prometheus.lsp_status.v1");
    assert_eq!(json["servers"][0]["pid"], me);
    // Restarting another server leaves this one alone.
    let text = ok(&run(home.path(), &["lsp", "restart", "gopls"]));
    assert!(text.contains("stopped 0"), "{text}");
    let status = Status::of_profile(&home.path().join("profiles/default"));
    assert!(status.restart_mark(Some("gopls")) > 0);
}

#[test]
fn installing_a_manual_server_prints_how() {
    let home = temp_home();
    let text = ok(&run(home.path(), &["lsp", "install", "zls"]));
    assert!(text.contains("zls: install it manually"), "{text}");
}

#[test]
fn a_non_list_exclude_roots_and_an_unknown_server_fail_with_78() {
    let home = temp_home();
    ok(&run(home.path(), &["lsp", "list"]));
    append_config(home.path(), "\n[lsp]\nexclude_roots = \"~/big\"\n");
    let out = run(home.path(), &["lsp", "list"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(stderr(&out).contains("exclude_roots"), "{}", stderr(&out));

    let home = temp_home();
    ok(&run(home.path(), &["lsp", "list"]));
    append_config(home.path(), "\n[lsp.servers.pyrite]\ndisabled = true\n");
    let out = run(home.path(), &["lsp", "list"]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("not a built-in language server"),
        "{}",
        stderr(&out)
    );
}
