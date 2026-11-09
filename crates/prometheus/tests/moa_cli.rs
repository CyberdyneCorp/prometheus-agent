//! Mixture of Agents from the CLI (spec: mixture-of-agents, One-shot /moa
//! command; Preset management commands; MoA billing and accounting).

mod common;

use std::path::Path;

use common::{append_config, mock_home, newest_session, ok, run, stderr};

/// A `default` preset on mock models (references `ref-a`, `ref-b`,
/// aggregator `agg`), a `review` preset aggregating elsewhere, and a
/// script naming the model that answered.
fn moa_home(home: &Path) -> String {
    ok(&run(home, &["config", "set", "model.default", "alpha"]));
    append_config(
        home,
        "\n[moa.presets.default]\n\
         reference_models = [{provider = \"mock\", model = \"ref-a\"}, {provider = \"mock\", model = \"ref-b\"}]\n\
         aggregator = {provider = \"mock\", model = \"agg\"}\n\
         [moa.presets.review]\n\
         reference_models = [{provider = \"mock\", model = \"ref-a\"}]\n\
         aggregator = {provider = \"openrouter\", model = \"some-model\"}\n",
    );
    let path = home.join("moa.toml");
    std::fs::write(
        &path,
        "[[rule]]\nmodel = \"alpha\"\nrepeat = true\ntext = \"answered by alpha\"\n\n\
         [[rule]]\nmodel = \"ref-a\"\nrepeat = true\ntext = \"advice A\"\n\n\
         [[rule]]\nmodel = \"ref-b\"\nrepeat = true\ntext = \"advice B\"\n\n\
         [[rule]]\nmodel = \"agg\"\nrepeat = true\ntext = \"answered by moa\"\n",
    )
    .unwrap();
    path.to_string_lossy().into_owned()
}

/// Scenario: Deleting the default preset refused.
#[test]
fn deleting_the_default_preset_is_refused_with_exit_1() {
    let home = mock_home();
    moa_home(home.path());
    let out = run(home.path(), &["moa", "delete", "default"]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("moa.default_preset"),
        "{}",
        stderr(&out)
    );
    // Another preset goes; an unknown one is not found.
    ok(&run(home.path(), &["moa", "delete", "review"]));
    let missing = run(home.path(), &["moa", "delete", "review"]);
    assert_eq!(missing.status.code(), Some(66), "{}", stderr(&missing));
}

#[test]
fn moa_list_marks_the_billed_aggregator_and_the_cadence() {
    let home = mock_home();
    moa_home(home.path());
    let text = ok(&run(home.path(), &["moa", "list"]));
    assert!(text.contains("default (default)"), "{text}");
    assert!(
        text.contains("aggregator: mock/agg (acts with the tools and is billed for the tool loop)"),
        "{text}"
    );
    assert!(
        text.contains("references: mock/ref-a, mock/ref-b (advise on user_turn)"),
        "{text}"
    );
    assert!(
        text.contains(
            "Aggregator is on openrouter; the whole tool loop will be billed there, not to mock."
        ),
        "{text}"
    );
    let doc: serde_json::Value =
        serde_json::from_str(&ok(&run(home.path(), &["--json", "moa", "list"]))).unwrap();
    assert_eq!(doc["schema"], "prometheus.moa_presets.v1");
    assert_eq!(doc["default_preset"], "default");
    let names: Vec<&str> = doc["presets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["default", "review"]);
}

/// Scenario: Ledger rows per slot.
#[test]
fn a_moa_turn_bills_every_slot_to_its_own_model() {
    let home = mock_home();
    let script = moa_home(home.path());
    let out = ok(&run(
        home.path(),
        &[
            "-z",
            "plan it",
            "--model",
            "moa/default",
            "--mock-script",
            &script,
        ],
    ));
    assert!(out.contains("answered by moa"), "{out}");
    let id = newest_session(home.path());
    let doc: serde_json::Value = serde_json::from_str(&ok(&run(
        home.path(),
        &["--json", "usage", "--session", &id, "--by", "model"],
    )))
    .unwrap();
    let rows = doc["profiles"][0]["rows"].as_array().unwrap();
    let mut keys: Vec<(String, u64)> = rows
        .iter()
        .map(|r| {
            (
                r["key"].as_str().unwrap().to_owned(),
                r["calls"].as_u64().unwrap(),
            )
        })
        .collect();
    keys.sort();
    assert_eq!(
        keys,
        [
            ("mock/agg".to_owned(), 1),
            ("mock/ref-a".to_owned(), 1),
            ("mock/ref-b".to_owned(), 1),
        ],
        "{doc}"
    );
}

/// Saved traces are part of the session's JSONL export (spec:
/// mixture-of-agents, MoA traces).
#[test]
fn a_session_export_carries_its_moa_traces() {
    let home = mock_home();
    let script = moa_home(home.path());
    append_config(home.path(), "[moa]\nsave_traces = true\n");
    ok(&run(
        home.path(),
        &[
            "-z",
            "plan it",
            "--model",
            "moa/default",
            "--mock-script",
            &script,
        ],
    ));
    let id = newest_session(home.path());
    let trace = home
        .path()
        .join("profiles/default/moa-traces")
        .join(format!("{id}.jsonl"));
    assert!(trace.exists(), "{}", trace.display());
    let export = ok(&run(home.path(), &["sessions", "export", &id]));
    let traces: Vec<serde_json::Value> = export
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|r| r["type"] == "moa_trace")
        .collect();
    assert_eq!(traces.len(), 1, "{export}");
    assert_eq!(traces[0]["trace"]["preset"], "default");
    assert_eq!(
        traces[0]["trace"]["references"].as_array().unwrap().len(),
        2
    );
    assert_eq!(
        traces[0]["trace"]["aggregator"]["output"],
        "answered by moa"
    );
}

/// Scenario: Route restored after one turn.
#[cfg(unix)]
#[test]
fn moa_runs_one_turn_on_the_default_preset_then_the_route_goes_back() {
    use common::pty::{exit, prompt, spawn};
    use expectrl::Expect;

    let home = mock_home();
    let script = moa_home(home.path());
    let mut p = spawn(home.path(), &["--no-color", "--mock-script", &script]);
    prompt(&mut p);
    p.send_line("/moa").unwrap();
    p.expect("usage: /moa <prompt>").unwrap();
    prompt(&mut p);
    p.send_line("/moa design a migration plan").unwrap();
    p.expect("answered by moa").unwrap();
    p.expect("model: back to mock/alpha").unwrap();
    prompt(&mut p);
    p.send_line("next").unwrap();
    p.expect("answered by alpha").unwrap();
    prompt(&mut p);
    exit(p);
}

/// Scenario: Route restored after one turn, in a session the gateway
/// daemon runs (the TUI's and messaging chats' turns run there too).
#[cfg(all(unix, feature = "gateway"))]
#[test]
fn moa_in_a_daemon_session_returns_to_the_route_after_its_turn() {
    use common::{daemon, pty};
    use expectrl::Expect;

    let home = mock_home();
    let script = moa_home(home.path());
    append_config(
        home.path(),
        &format!("[providers.mock]\nscript = \"{script}\"\n"),
    );
    let _daemon = daemon::start(home.path(), &[]);
    let mut p = pty::spawn(home.path(), &["--no-color"]);
    p.expect("attached to the gateway daemon").unwrap();
    pty::prompt(&mut p);
    p.send_line("/moa design a migration plan").unwrap();
    p.expect("answered by moa").unwrap();
    pty::prompt(&mut p);
    p.send_line("next").unwrap();
    p.expect("answered by alpha").unwrap();
    pty::prompt(&mut p);
    pty::exit(p);
}

/// The preset editors' methods (spec: mixture-of-agents, Preset management
/// commands): list, put and delete through the daemon, the default preset
/// protected.
#[cfg(feature = "gateway")]
#[test]
fn preset_editors_list_write_and_delete_through_the_daemon() {
    use common::daemon;
    use serde_json::json;

    let home = mock_home();
    moa_home(home.path());
    let _daemon = daemon::start(home.path(), &[]);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (client, _inbox) = daemon::connect(home.path(), "default").await;
        let names = |list: &serde_json::Value| -> Vec<String> {
            list["presets"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["name"].as_str().unwrap().to_owned())
                .collect()
        };
        let listed = client.call("moa.presets.list", json!({})).await.unwrap();
        assert_eq!(names(&listed), ["default", "review"]);
        let preset = json!({
            "references": [{"provider": "mock", "model": "ref-a"}],
            "aggregator": {"provider": "mock", "model": "agg"},
            "fanout": "every_n:2"
        });
        let put = client
            .call("moa.presets.put", json!({"name": "team", "preset": preset}))
            .await
            .unwrap();
        assert_eq!(names(&put), ["default", "review", "team"]);
        let team = &put["presets"][2];
        assert_eq!(team["fanout"], "every_n:2");
        let refused = client
            .call("moa.presets.delete", json!({"name": "default"}))
            .await
            .unwrap_err();
        assert_eq!(refused.data.unwrap().code, "moa.default_preset_protected");
        let nested = client
            .call(
                "moa.presets.put",
                json!({"name": "loop", "preset": {
                    "references": [{"provider": "mock", "model": "a"}],
                    "aggregator": {"provider": "moa", "model": "team"}
                }}),
            )
            .await
            .unwrap_err();
        assert!(
            nested.message.contains("moa.presets.loop.aggregator"),
            "{}",
            nested.message
        );
        let deleted = client
            .call("moa.presets.delete", json!({"name": "team"}))
            .await
            .unwrap();
        assert_eq!(names(&deleted), ["default", "review"]);
    });
}
