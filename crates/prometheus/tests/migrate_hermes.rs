//! `prometheus migrate from-hermes`: the command surface, the read-only
//! source, profile mapping, dry run, the mapping report and config /
//! secrets translation (spec: hermes-migration), against fixture Hermes
//! homes.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::hermes::{self, Hermes};
use serde_json::Value;

fn migrate(home: &Path, hermes: &Path, extra: &[&str]) -> Output {
    let mut cmd = common::command(home);
    cmd.args(["--json", "migrate", "from-hermes", "--hermes-home"])
        .arg(hermes)
        .args(extra)
        .stdin(Stdio::null());
    cmd.output().unwrap()
}

fn report(out: &Output) -> Value {
    assert!(
        out.status.success(),
        "exit {:?}\nstdout {}\nstderr {}",
        out.status.code(),
        common::stdout(out),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// The report entry of `item` in profile `profile`.
fn entry<'a>(report: &'a Value, profile: &str, item: &str) -> &'a Value {
    let p = report["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["hermes_profile"] == profile)
        .unwrap_or_else(|| panic!("no profile {profile}: {report:#}"));
    p["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["item"] == item)
        .unwrap_or_else(|| panic!("no entry {item}: {:#}", p["entries"]))
}

fn config(home: &Path, profile: &str) -> String {
    std::fs::read_to_string(home.join("profiles").join(profile).join("config.toml")).unwrap()
}

/// A secret of the profile's file store (`secrets get` reveals values
/// only to a terminal).
fn secret(home: &Path, profile: &str, name: &str) -> Option<String> {
    let path = home.join("profiles").join(profile).join("secrets.toml");
    let table: toml::Table = toml::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    table.get(name)?.as_str().map(str::to_owned)
}

/// A Hermes root profile with config, secrets, credentials and an MCP
/// server.
fn rich_home(dir: &Path) -> Hermes {
    let h = Hermes::new(&dir.join("hermes"));
    h.write(
        "default",
        "config.yaml",
        "_config_version: 52\n\
         model:\n  default: claude-sonnet-4\n  provider: anthropic\n\
         max_turns: 60\n\
         human_delay:\n  mode: natural\n\
         tool_output:\n  max_bytes: 50000\n\
         approvals:\n  cron_mode: off\n\
         display:\n  skin: charizard\n\
         credential_pool_strategies:\n  openrouter: round_robin\n\
         mcp_servers:\n  github:\n    url: https://mcp.example.test/mcp\n    headers:\n      Authorization: Bearer ${GITHUB_TOKEN}\n",
    );
    h.write(
        "default",
        ".env",
        "OPENAI_API_KEY=sk-abc\nANTHROPIC_API_KEY=sk-ant-1\nHERMES_CRON_MAX_PARALLEL=4\n\
         ACME_WIDGET=xyz\nGITHUB_TOKEN=ghp-secret-1\nHERMES_SEARCH_SLOW_MS=900\nPYTHONPATH=/x\n",
    );
    h.write(
        "default",
        "auth.json",
        r#"{"version": 1, "providers": {}, "credential_pool": {"openrouter": [
            {"id": "a", "label": "first", "auth_type": "api_key", "priority": 0, "source": "manual", "access_token": "sk-or-1"},
            {"id": "b", "label": "second", "auth_type": "api_key", "priority": 1, "source": "manual", "access_token": "sk-or-2"}
        ]}}"#,
    );
    h
}

#[test]
fn an_unknown_component_exits_2_listing_the_valid_names() {
    let tmp = common::temp_home();
    let out = common::run(tmp.path(), &["migrate", "from-hermes", "--only", "sesions"]);
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains(
            "config, secrets, identity, memory, skills, sessions, cron, mcp, plugins, gateway"
        ),
        "{err}"
    );
}

#[test]
fn a_missing_hermes_home_exits_66() {
    let tmp = common::temp_home();
    let out = common::run(
        tmp.path(),
        &[
            "migrate",
            "from-hermes",
            "--hermes-home",
            "/nowhere",
            "--yes",
        ],
    );
    assert_eq!(out.status.code(), Some(66));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("No Hermes home found at /nowhere"), "{err}");
}

#[test]
fn the_source_tree_is_unchanged_by_a_migration_and_a_rerun() {
    let tmp = common::temp_home();
    let h = rich_home(tmp.path());
    h.write("default", "SOUL.md", "I am Hermes.\n");
    h.write("default", "memories/MEMORY.md", "one\n§\ntwo\n");
    h.write(
        "default",
        "skills/notes/todo/SKILL.md",
        "---\nname: todo\ndescription: Todos.\n---\nList.\n",
    );
    h.write(
        "default",
        "skills/.bundled_manifest",
        "todo:00000000000000000000000000000000\n",
    );
    h.write("default", "scripts/check.sh", "echo ok\n");
    h.write(
        "default",
        "cron/jobs.json",
        r#"{"jobs": [{"id": "j", "name": "check", "prompt": "Check", "script": "check.sh", "enabled": true,
            "schedule": {"kind": "cron", "expr": "0 9 * * *"}}]}"#,
    );
    h.write(
        "default",
        "platforms/pairing/telegram-approved.json",
        r#"{"1": {"user_name": "a"}}"#,
    );
    h.write(
        "default",
        "plugins/demo/plugin.yaml",
        "name: demo\nversion: 1.0.0\n",
    );
    h.write(
        "default",
        "plugins/demo/__init__.py",
        "def register(ctx):\n    pass\n",
    );
    h.write(
        "coder",
        "config.yaml",
        "_config_version: 52\nmax_turns: 10\n",
    );
    {
        let db = h.db("default", 32);
        hermes::add_session(
            &db,
            &hermes::Session {
                id: "s1",
                started_at: 1_700_000_000.0,
                ..Default::default()
            },
        );
        hermes::add_message(&db, "s1", "user", "hello", 1_700_000_001.0);
    }
    let before = hermes::attributes(&h.root);
    report(&migrate(tmp.path(), &h.root, &["--all", "--yes"]));
    report(&migrate(tmp.path(), &h.root, &["--all", "--yes"]));
    assert_eq!(hermes::attributes(&h.root), before);
}

#[test]
fn a_running_hermes_is_snapshotted_without_blocking_it() {
    let tmp = common::temp_home();
    let h = Hermes::new(&tmp.path().join("hermes"));
    let db = h.db("default", 32);
    hermes::add_session(
        &db,
        &hermes::Session {
            id: "live",
            started_at: 1_700_000_000.0,
            ..Default::default()
        },
    );
    hermes::add_message(&db, "live", "user", "before the snapshot", 1_700_000_001.0);
    // Hermes is running: its gateway pid is alive and it holds a write
    // transaction on state.db while the migration runs.
    h.write(
        "default",
        "gateway.pid",
        &format!("{{\"pid\": {}}}", std::process::id()),
    );
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    hermes::add_message(&db, "live", "user", "uncommitted", 1_700_000_002.0);
    let out = migrate(tmp.path(), &h.root, &["--yes"]);
    let report = report(&out);
    // Hermes is not blocked: its transaction still commits.
    db.execute_batch("COMMIT").unwrap();
    let warnings = report["warnings"].to_string();
    assert!(warnings.contains("Hermes is running"), "{warnings}");
    assert!(warnings.contains("picked up by a re-run"), "{warnings}");
    assert_eq!(entry(&report, "default", "live")["disposition"], "mapped");
    let shown = common::run(tmp.path(), &["sessions", "export", "live"]);
    let text = common::stdout(&shown);
    assert!(
        text.contains("before the snapshot") && !text.contains("uncommitted"),
        "{text}"
    );
}

#[test]
fn all_profiles_map_by_name_and_tombstones_are_skipped() {
    let tmp = common::temp_home();
    let h = Hermes::new(&tmp.path().join("hermes"));
    h.write("default", "config.yaml", "_config_version: 52\n");
    h.write("coder", "SOUL.md", "coder\n");
    h.write("ops", ".env", "OPS_TOKEN=x\n");
    h.write("old", "config.yaml", "_config_version: 52\n");
    std::fs::create_dir_all(h.root.join("profiles/.deleted/old")).unwrap();
    let report = report(&migrate(tmp.path(), &h.root, &["--all", "--yes"]));
    for name in ["default", "coder", "ops"] {
        assert!(
            tmp.path()
                .join("profiles")
                .join(name)
                .join("config.toml")
                .is_file(),
            "{name}"
        );
    }
    assert!(!tmp.path().join("profiles/old").exists());
    let skipped = &report["skipped_profiles"];
    assert!(
        skipped
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "old" && s["reason"] == "tombstoned"),
        "{skipped}"
    );
}

#[test]
fn a_reserved_profile_name_needs_a_target_and_the_others_proceed() {
    let tmp = common::temp_home();
    let h = Hermes::new(&tmp.path().join("hermes"));
    h.write("tools", "SOUL.md", "tools\n");
    h.write("writer", "SOUL.md", "writer\n");
    let out = migrate(tmp.path(), &h.root, &["--all", "--yes"]);
    assert_eq!(out.status.code(), Some(1), "{}", common::stdout(&out));
    let all: Value = serde_json::from_slice(&out.stdout).unwrap();
    let tools = all["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["hermes_profile"] == "tools")
        .unwrap();
    assert_eq!(
        tools["error"],
        "profile name 'tools' is reserved in Prometheus; use --into-profile"
    );
    assert!(tmp.path().join("profiles/writer/SOUL.md").is_file());
    let into = migrate(
        tmp.path(),
        &h.root,
        &["--profile", "tools", "--into-profile", "toolbox", "--yes"],
    );
    report(&into);
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("profiles/toolbox/SOUL.md")).unwrap(),
        "tools\n"
    );
}

/// The binary with HOME in `dir` and `--home` naming a Prometheus home
/// that does not exist.
fn fresh_machine(dir: &Path) -> (Command, PathBuf) {
    let prometheus = dir.join("prometheus-home");
    let mut cmd = Command::new(common::BIN);
    for (var, _) in std::env::vars_os() {
        if var.to_string_lossy().starts_with("PMT_")
            || var.to_string_lossy().starts_with("PROMETHEUS_")
        {
            cmd.env_remove(&var);
        }
    }
    let user = dir.join("user");
    std::fs::create_dir_all(&user).unwrap();
    cmd.env("HOME", &user).env("USERPROFILE", &user);
    cmd.arg("--home").arg(&prometheus);
    (cmd, prometheus)
}

#[test]
fn a_dry_run_on_a_fresh_machine_writes_nothing() {
    let tmp = common::temp_home();
    let h = rich_home(tmp.path());
    h.write("coder", "SOUL.md", "coder\n");
    {
        let db = h.db("default", 32);
        hermes::add_session(
            &db,
            &hermes::Session {
                id: "s1",
                started_at: 1_700_000_000.0,
                ..Default::default()
            },
        );
    }
    let (mut cmd, prometheus) = fresh_machine(tmp.path());
    let out = cmd
        .args([
            "migrate",
            "from-hermes",
            "--dry-run",
            "--all",
            "--hermes-home",
        ])
        .arg(&h.root)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = common::stdout(&out);
    assert!(text.contains("Plan from"), "{text}");
    assert!(
        text.contains("profile default -> default") && text.contains("profile coder -> coder"),
        "{text}"
    );
    assert!(text.contains("sessions  1 mapped"), "{text}");
    assert!(
        !prometheus.exists(),
        "the dry run created the Prometheus home"
    );
}

#[test]
fn non_interactive_runs_need_yes_and_write_nothing() {
    let tmp = common::temp_home();
    let h = rich_home(tmp.path());
    let (mut cmd, prometheus) = fresh_machine(tmp.path());
    let out = cmd
        .args(["migrate", "from-hermes", "--hermes-home"])
        .arg(&h.root)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(4));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("refusing to migrate non-interactively without --yes"),
        "{err}"
    );
    assert!(!prometheus.exists());
}

#[test]
fn the_report_names_unsupported_keys_and_secrets_but_no_secret_value() {
    let tmp = common::temp_home();
    let h = rich_home(tmp.path());
    let out = migrate(tmp.path(), &h.root, &["--yes"]);
    let report = report(&out);
    let delay = entry(&report, "default", "human_delay.mode");
    assert_eq!(delay["disposition"], "unsupported");
    assert!(!config(tmp.path(), "default").contains("human_delay"));
    let key = entry(&report, "default", ".env:OPENAI_API_KEY");
    assert_eq!(key["disposition"], "secret");
    let saved: Vec<PathBuf> = std::fs::read_dir(tmp.path().join("profiles/default/migrations"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("hermes-")
        })
        .collect();
    assert_eq!(saved.len(), 1, "{saved:?}");
    for text in [
        std::fs::read_to_string(&saved[0]).unwrap(),
        common::stdout(&out),
    ] {
        assert!(text.contains("OPENAI_API_KEY"));
        for value in ["sk-abc", "sk-or-1", "ghp-secret-1", "xyz"] {
            assert!(!text.contains(value), "{value} leaked");
        }
    }
}

#[test]
fn model_settings_map_and_defaults_are_not_carried() {
    let tmp = common::temp_home();
    let h = rich_home(tmp.path());
    let report = report(&migrate(
        tmp.path(),
        &h.root,
        &["--yes", "--only", "config"],
    ));
    let parsed: toml::Table = toml::from_str(&config(tmp.path(), "default")).unwrap();
    assert_eq!(parsed["model"]["default"].as_str(), Some("claude-sonnet-4"));
    assert_eq!(parsed["model"]["provider"].as_str(), Some("anthropic"));
    assert_eq!(parsed["agent"]["max_iterations"].as_integer(), Some(60));
    let max_bytes = entry(&report, "default", "tool_output.max_bytes");
    assert_eq!(max_bytes["disposition"], "default_omitted");
    assert!(!config(tmp.path(), "default").contains("tool_output"));
}

#[test]
fn a_config_older_than_version_12_fails_alone() {
    let tmp = common::temp_home();
    let h = Hermes::new(&tmp.path().join("hermes"));
    h.write(
        "default",
        "config.yaml",
        "_config_version: 9\nmax_turns: 5\n",
    );
    h.write("default", "SOUL.md", "still imported\n");
    let out = migrate(tmp.path(), &h.root, &["--yes"]);
    assert_eq!(out.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let components = report["profiles"][0]["components"].as_array().unwrap();
    let config = components
        .iter()
        .find(|c| c["component"] == "config")
        .unwrap();
    assert_eq!(config["status"], "failed");
    assert_eq!(
        config["error"],
        "Hermes config predates version 12; run hermes update first"
    );
    let identity = components
        .iter()
        .find(|c| c["component"] == "identity")
        .unwrap();
    assert_eq!(identity["status"], "committed");
    assert!(tmp.path().join("profiles/default/SOUL.md").is_file());
}

#[test]
fn an_unquoted_off_on_cron_approvals_resolves_to_deny() {
    let tmp = common::temp_home();
    let h = rich_home(tmp.path());
    let report = report(&migrate(
        tmp.path(),
        &h.root,
        &["--yes", "--only", "config"],
    ));
    let parsed: toml::Table = toml::from_str(&config(tmp.path(), "default")).unwrap();
    assert_eq!(parsed["approvals"]["cron_mode"].as_str(), Some("deny"));
    let e = entry(&report, "default", "approvals.cron_mode");
    assert_eq!(e["disposition"], "ambiguous");
    assert!(
        e["reason"]
            .as_str()
            .unwrap()
            .contains("set approvals.cron_mode explicitly"),
        "{e}"
    );
}

#[test]
fn a_secret_reference_in_an_mcp_header_stays_a_secret() {
    let tmp = common::temp_home();
    let h = rich_home(tmp.path());
    report(&migrate(
        tmp.path(),
        &h.root,
        &["--yes", "--only", "secrets,mcp"],
    ));
    let parsed: toml::Table = toml::from_str(&config(tmp.path(), "default")).unwrap();
    let header = &parsed["mcp"]["servers"]["github"]["headers"]["Authorization"];
    assert_eq!(header.as_str(), Some("Bearer ${GITHUB_TOKEN}"));
    assert_eq!(
        secret(tmp.path(), "default", "GITHUB_TOKEN").as_deref(),
        Some("ghp-secret-1")
    );
}

#[test]
fn env_settings_move_into_config_and_unknown_names_are_secrets() {
    let tmp = common::temp_home();
    let h = rich_home(tmp.path());
    let report = report(&migrate(
        tmp.path(),
        &h.root,
        &["--yes", "--only", "secrets"],
    ));
    let parsed: toml::Table = toml::from_str(&config(tmp.path(), "default")).unwrap();
    assert_eq!(parsed["cron"]["max_parallel_jobs"].as_integer(), Some(4));
    assert_eq!(
        secret(tmp.path(), "default", "ANTHROPIC_API_KEY").as_deref(),
        Some("sk-ant-1")
    );
    assert_eq!(
        secret(tmp.path(), "default", "HERMES_CRON_MAX_PARALLEL"),
        None
    );
    assert_eq!(
        secret(tmp.path(), "default", "ACME_WIDGET").as_deref(),
        Some("xyz")
    );
    let acme = entry(&report, "default", ".env:ACME_WIDGET");
    assert_eq!(acme["disposition"], "secret");
    assert_eq!(acme["reason"], "unclassified");
    assert_eq!(
        entry(&report, "default", ".env:HERMES_SEARCH_SLOW_MS")["disposition"],
        "unsupported"
    );
    assert_eq!(
        entry(&report, "default", ".env:PYTHONPATH")["disposition"],
        "skipped"
    );
}

#[test]
fn a_credential_pool_keeps_both_keys() {
    let tmp = common::temp_home();
    let h = rich_home(tmp.path());
    report(&migrate(
        tmp.path(),
        &h.root,
        &["--yes", "--only", "secrets"],
    ));
    assert_eq!(
        secret(tmp.path(), "default", "OPENROUTER_API_KEY").as_deref(),
        Some("sk-or-1")
    );
    assert_eq!(
        secret(tmp.path(), "default", "OPENROUTER_API_KEY_2").as_deref(),
        Some("sk-or-2")
    );
    let parsed: toml::Table = toml::from_str(&config(tmp.path(), "default")).unwrap();
    assert_eq!(
        parsed["providers"]["openrouter"]["pool_strategy"].as_str(),
        Some("round_robin")
    );
    let out = common::run(tmp.path(), &["--json", "auth", "list"]);
    let text = common::stdout(&out);
    assert!(text.contains("OPENROUTER_API_KEY_2"), "{text}");
}

#[test]
fn an_unknown_skin_is_reported_and_the_default_theme_applies() {
    let tmp = common::temp_home();
    let h = rich_home(tmp.path());
    let report = report(&migrate(
        tmp.path(),
        &h.root,
        &["--yes", "--only", "config"],
    ));
    let skin = entry(&report, "default", "display.skin");
    assert_eq!(skin["disposition"], "unsupported");
    assert_eq!(skin["value"], "charizard");
    assert!(!config(tmp.path(), "default").contains("theme"));
}

#[test]
fn oauth_credential_files_become_sign_ins_with_their_expiry() {
    let tmp = common::temp_home();
    let h = Hermes::new(&tmp.path().join("hermes"));
    h.write(
        "default",
        ".anthropic_oauth.json",
        r#"{"claudeAiOauth": {"accessToken": "sk-ant-oat-1", "refreshToken": "r1", "expiresAt": 1893456000000}}"#,
    );
    h.write("default", "google_token.json", r#"{"token": "g"}"#);
    let report = report(&migrate(
        tmp.path(),
        &h.root,
        &["--yes", "--only", "secrets"],
    ));
    assert_eq!(
        entry(&report, "default", ".anthropic_oauth.json")["disposition"],
        "secret"
    );
    let google = entry(&report, "default", "google_token.json");
    assert_eq!(google["disposition"], "unsupported");
    let listed = common::stdout(&common::run(tmp.path(), &["auth", "list"]));
    assert!(
        listed.contains("anthropic")
            && listed.contains("import:hermes")
            && listed.contains("expires 2030-01-01"),
        "{listed}"
    );
    // Credential files are never copied into the profile.
    assert!(
        !tmp.path()
            .join("profiles/default/.anthropic_oauth.json")
            .exists()
    );
    assert!(
        !tmp.path()
            .join("profiles/default/google_token.json")
            .exists()
    );
}

#[test]
fn a_dry_run_predicts_conflicts_with_an_existing_profile() {
    let tmp = common::temp_home();
    let h = rich_home(tmp.path());
    common::ok(&common::run(
        tmp.path(),
        &["config", "set", "agent.max_iterations", "80"],
    ));
    let before = config(tmp.path(), "default");
    let planned = report(&migrate(
        tmp.path(),
        &h.root,
        &["--dry-run", "--only", "config"],
    ));
    let e = entry(&planned, "default", "max_turns");
    assert_eq!(e["disposition"], "conflict", "{e}");
    assert_eq!(config(tmp.path(), "default"), before);
    let overwrite = report(&migrate(
        tmp.path(),
        &h.root,
        &["--dry-run", "--only", "config", "--overwrite"],
    ));
    assert_eq!(
        entry(&overwrite, "default", "max_turns")["disposition"],
        "transformed"
    );
}
