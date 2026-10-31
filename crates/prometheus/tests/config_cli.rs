//! `prometheus config` against the built binary and a temporary home
//! (spec: configuration-and-profiles: Home resolution, Config CLI,
//! Configuration layering, Config explain, Format-preserving atomic config
//! writes, Strict validation, Secrets are not configuration, Config schema
//! versioning; workspace-architecture: Error taxonomy).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_prometheus");

fn temp_home() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

fn command(env_home: Option<&Path>) -> Command {
    let mut cmd = Command::new(BIN);
    for var in ["PROMETHEUS_HOME", "VISUAL", "EDITOR", "PMT_LOG", "RUST_LOG"] {
        cmd.env_remove(var);
    }
    if let Some(home) = env_home {
        cmd.env("PROMETHEUS_HOME", home);
    }
    cmd
}

fn run(home: &Path, args: &[&str]) -> Output {
    command(None)
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .unwrap()
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

fn config_path(home: &Path) -> PathBuf {
    home.join("profiles").join("default").join("config.toml")
}

#[test]
fn environment_home_holds_every_profile() {
    let home = temp_home();
    let out = command(Some(home.path()))
        .args(["config", "path"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&out).trim(),
        config_path(home.path()).display().to_string()
    );
    assert!(!home.path().join("config.toml").exists());
}

#[test]
fn home_flag_wins_over_environment() {
    let env_home = temp_home();
    let flag_home = temp_home();
    let out = command(Some(env_home.path()))
        .arg("--home")
        .arg(flag_home.path())
        .args(["config", "path"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(Path::new(stdout(&out).trim()).starts_with(flag_home.path()));
    assert!(!env_home.path().join("profiles").exists());
}

#[test]
fn set_keeps_comments_and_untouched_keys() {
    let home = temp_home();
    run(home.path(), &["config", "path"]);
    let text = "schema_version = 1\n\n# Pinned for the team.\n[model]\ndefault = \"claude-x\"\n";
    std::fs::write(config_path(home.path()), text).unwrap();
    let out = run(home.path(), &["config", "set", "display.theme", "mono"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let written = std::fs::read_to_string(config_path(home.path())).unwrap();
    assert_eq!(written, format!("{text}\n[display]\ntheme = \"mono\"\n"));
}

#[test]
fn out_of_range_value_exits_2_and_writes_nothing() {
    let home = temp_home();
    run(home.path(), &["config", "path"]);
    let before = std::fs::read_to_string(config_path(home.path())).unwrap();
    let out = run(
        home.path(),
        &["config", "set", "agent.max_iterations", "-3"],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("agent.max_iterations: must be between 0 and 100000"));
    assert_eq!(
        std::fs::read_to_string(config_path(home.path())).unwrap(),
        before
    );
}

#[test]
fn pasted_api_key_is_refused() {
    let home = temp_home();
    let out = run(
        home.path(),
        &["config", "set", "providers.acme.base_url", "sk-live-123456"],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out)
            .contains("this looks like a credential; store it with: prometheus secrets set <NAME>")
    );
    let written = std::fs::read_to_string(config_path(home.path())).unwrap();
    assert!(!written.contains("sk-live"));
}

#[test]
fn per_invocation_override_is_typed_and_not_persisted() {
    let home = temp_home();
    let out = run(
        home.path(),
        &[
            "--set",
            "agent.max_iterations=10",
            "config",
            "get",
            "agent.max_iterations",
        ],
    );
    assert_eq!(stdout(&out), "10\n", "{}", stderr(&out));
    let out = run(home.path(), &["config", "get", "agent.max_iterations"]);
    assert_eq!(stdout(&out), "500\n");
    let out = run(
        home.path(),
        &[
            "--set",
            "agent.max_iterations=ten",
            "config",
            "get",
            "agent.max_iterations",
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).contains("agent.max_iterations: expected integer, found string"));
}

#[test]
fn unset_restores_the_default() {
    let home = temp_home();
    run(
        home.path(),
        &["config", "set", "agent.max_iterations", "40"],
    );
    assert_eq!(
        stdout(&run(
            home.path(),
            &["config", "get", "agent.max_iterations"]
        )),
        "40\n"
    );
    let out = run(home.path(), &["config", "unset", "agent.max_iterations"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        stdout(&run(
            home.path(),
            &["config", "get", "agent.max_iterations"]
        )),
        "500\n"
    );
}

#[test]
fn credential_in_the_file_is_invalid_for_validate_and_every_load() {
    let home = temp_home();
    run(home.path(), &["config", "path"]);
    std::fs::write(
        config_path(home.path()),
        "[providers.acme]\nbase_url = \"sk-live-123456\"\n",
    )
    .unwrap();
    for args in [
        &["config", "validate"][..],
        &["config", "get", "agent.max_iterations"],
    ] {
        let out = run(home.path(), args);
        assert_eq!(out.status.code(), Some(78), "{args:?}");
        let err = stderr(&out);
        assert!(
            err.contains("config.toml:2:12: providers.acme.base_url: this looks like a credential"),
            "{args:?}: {err}"
        );
        assert!(!err.contains("sk-live"), "{args:?}: {err}");
    }
    std::fs::write(config_path(home.path()), "").unwrap();
    let out = run(
        home.path(),
        &[
            "--set",
            "display.theme=ghp_abcdefghijklmnop",
            "config",
            "get",
            "display.theme",
        ],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("this looks like a credential"));
}

#[test]
fn validate_reports_every_error_with_line_and_allowed_values() {
    let home = temp_home();
    run(home.path(), &["config", "path"]);
    std::fs::write(
        config_path(home.path()),
        "[approvals]\nmode = \"of\"\n\n[agent]\nmax_iteration = 40\n",
    )
    .unwrap();
    let out = run(home.path(), &["config", "validate"]);
    assert_eq!(out.status.code(), Some(78));
    let err = stderr(&out);
    assert!(err.contains("config.toml:2:8: approvals.mode: unknown value \"of\" (allowed: \"manual\", \"smart\", \"off\")"), "{err}");
    assert!(err.contains("config.toml:5:1: unknown key 'agent.max_iteration' (did you mean 'agent.max_iterations'?)"), "{err}");
    assert!(stdout(&out).is_empty());
}

#[test]
fn unknown_toolset_fails_validation_with_the_closest_name() {
    let home = temp_home();
    run(home.path(), &["config", "path"]);
    std::fs::write(
        config_path(home.path()),
        "[tools]\nenabled_toolsets = [\"core\", \"brwser\"]\n",
    )
    .unwrap();
    let out = run(home.path(), &["config", "validate"]);
    assert_eq!(out.status.code(), Some(78));
    let err = stderr(&out);
    assert!(
        err.contains("tools.enabled_toolsets: unknown toolset 'brwser' (did you mean 'browser'?)"),
        "{err}"
    );
    std::fs::write(
        config_path(home.path()),
        "[tools]\nenabled_toolsets = [\"core\", \"x_search\"]\ndisabled_toolsets = [\"browser\"]\n",
    )
    .unwrap();
    assert_eq!(
        run(home.path(), &["config", "validate"]).status.code(),
        Some(0)
    );
}

#[test]
fn unknown_context_engine_fails_validation_instead_of_falling_back() {
    let home = temp_home();
    run(home.path(), &["config", "path"]);
    std::fs::write(config_path(home.path()), "[context]\nengine = \"lcm\"\n").unwrap();
    let out = run(home.path(), &["config", "validate"]);
    assert_eq!(out.status.code(), Some(78));
    assert!(
        stderr(&out).contains("unknown context engine 'lcm'"),
        "{}",
        stderr(&out)
    );
    std::fs::write(
        config_path(home.path()),
        "[context]\nengine = \"builtin\"\n",
    )
    .unwrap();
    assert_eq!(
        run(home.path(), &["config", "validate"]).status.code(),
        Some(0)
    );
}

#[test]
fn compression_values_out_of_range_fail_validation_instead_of_clamping() {
    let home = temp_home();
    run(home.path(), &["config", "path"]);
    for (text, message) in [
        (
            "[compression]\nmax_attempts = 50\n",
            "compression.max_attempts: must be between 1 and 10",
        ),
        (
            "[compression]\ntail_mode = \"fat\"\n",
            "compression.tail_mode: unknown value \"fat\" (allowed: \"lean\", \"ratio\")",
        ),
        (
            "[compression]\ntarget_ratio = 0.05\n",
            "compression.target_ratio: must be between 0.1 and 0.8",
        ),
        (
            "[compression]\nthreshold = 0.0\n",
            "compression.threshold: must be greater than 0 and less than 1",
        ),
        (
            "[prompt_cache]\nttl = \"2h\"\n",
            "prompt_cache.ttl: unknown value \"2h\"",
        ),
    ] {
        std::fs::write(config_path(home.path()), text).unwrap();
        let out = run(home.path(), &["config", "validate"]);
        assert_eq!(out.status.code(), Some(78), "{text}");
        assert!(stderr(&out).contains(message), "{text}: {}", stderr(&out));
    }
}

#[test]
fn unknown_terminal_backend_fails_validation_with_the_available_list() {
    let home = temp_home();
    run(home.path(), &["config", "path"]);
    std::fs::write(
        config_path(home.path()),
        "[terminal]\nbackend = \"acmebox\"\n",
    )
    .unwrap();
    let out = run(home.path(), &["config", "validate"]);
    assert_eq!(out.status.code(), Some(78));
    let err = stderr(&out);
    assert!(
        err.contains("unknown terminal backend 'acmebox'; available: local, container, ssh"),
        "{err}"
    );

    // Reserved names are valid configuration; selecting one fails at start.
    std::fs::write(
        config_path(home.path()),
        "[terminal]\nbackend = \"container\"\n",
    )
    .unwrap();
    assert_eq!(
        run(home.path(), &["config", "validate"]).status.code(),
        Some(0)
    );
}

#[test]
fn unknown_key_json_error_has_kind_and_code() {
    let home = temp_home();
    run(home.path(), &["config", "path"]);
    std::fs::write(config_path(home.path()), "[agent]\nmax_iterationz = 40\n").unwrap();
    let out = run(home.path(), &["--json", "config", "validate"]);
    assert_eq!(out.status.code(), Some(78));
    let doc: serde_json::Value = serde_json::from_str(stderr(&out).trim()).unwrap();
    assert_eq!(doc["error"]["kind"], "config");
    assert_eq!(doc["error"]["code"], "config.unknown_key");
    assert!(stdout(&out).is_empty());
}

#[test]
fn explain_names_default_effective_source_line_and_apply_class() {
    let home = temp_home();
    run(home.path(), &["config", "path"]);
    // `max_iterations` lands on line 12.
    let text = "schema_version = 1\n# 2\n# 3\n# 4\n# 5\n# 6\n# 7\n# 8\n# 9\n# 10\n[agent]\nmax_iterations = 40\n";
    std::fs::write(config_path(home.path()), text).unwrap();
    let out = run(home.path(), &["config", "explain", "agent.max_iterations"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let shown = stdout(&out);
    assert!(shown.contains("default:     500"), "{shown}");
    assert!(shown.contains("effective:   40"), "{shown}");
    assert!(shown.contains("source:      config.toml:12"), "{shown}");
    assert!(shown.contains("apply:       next_session"), "{shown}");

    let out = run(
        home.path(),
        &["--json", "config", "explain", "agent.max_iterations"],
    );
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["schema"], "prometheus.config_explain.v1");
    assert_eq!(doc["value"], 40);
    assert_eq!(doc["line"], 12);

    let out = run(
        home.path(),
        &[
            "--set",
            "display.theme=mono",
            "config",
            "explain",
            "--changed",
        ],
    );
    assert_eq!(
        stdout(&out),
        "agent.max_iterations = 40  (config.toml:12)\ndisplay.theme = mono  (--set)\n"
    );
}

#[test]
fn schema_lists_every_key_with_default_and_description() {
    let home = temp_home();
    let out = run(home.path(), &["config", "schema"]);
    assert!(out.status.success());
    let schema: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    let agent = &schema["$defs"]["AgentConfig"]["properties"]["max_iterations"];
    assert_eq!(agent["default"], 500);
    assert!(agent["description"].as_str().is_some_and(|d| !d.is_empty()));
    let checked_in = include_str!("../../../schema/config.schema.json");
    assert_eq!(
        stdout(&out),
        checked_in,
        "schema/config.schema.json is stale: run cargo xtask gen"
    );
}

#[test]
fn migration_on_load_keeps_a_backup_and_newer_files_are_refused() {
    let home = temp_home();
    run(home.path(), &["config", "path"]);
    std::fs::write(
        config_path(home.path()),
        "schema_version = 0\n[agent]\nmax_turns = 7\n",
    )
    .unwrap();
    let out = run(home.path(), &["config", "get", "agent.max_iterations"]);
    assert_eq!(stdout(&out), "7\n", "{}", stderr(&out));
    let backups = home.path().join("profiles/default/backups/config");
    let names: Vec<String> = std::fs::read_dir(backups)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|n| n.starts_with("config-v0-")),
        "{names:?}"
    );

    std::fs::write(config_path(home.path()), "schema_version = 999\n").unwrap();
    let out = run(home.path(), &["config", "get", "agent.max_iterations"]);
    assert_eq!(out.status.code(), Some(78));
    assert!(
        stderr(&out)
            .contains("config.toml schema_version 999 is newer than this prometheus supports")
    );
}

#[test]
fn unknown_launch_profile_is_a_usage_error() {
    let home = temp_home();
    let out = run(home.path(), &["-p", "work", "config", "path"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains(
            "profile 'work' does not exist; create it with: prometheus profile create work"
        ),
        "{}",
        stderr(&out)
    );
    let out = run(home.path(), &["--json", "-p", "work", "config", "path"]);
    let doc: serde_json::Value = serde_json::from_str(stderr(&out).trim()).unwrap();
    assert_eq!(doc["error"]["kind"], "usage");
    assert_eq!(doc["error"]["code"], "profile.not_found");
    assert!(!home.path().join("profiles").join("work").exists());
}

const SECRET_KEY_CREDENTIALS: &[&str] = &[
    "sk-live-1234567abc",
    "ghp_abcdefghijklmnop",
    "xoxb-123456789012-abcdef",
    "AKIAIOSFODNN7EXAMPLE",
];

/// The refusal names the key; the value appears nowhere in the output.
fn assert_refused_without_value(out: &Output, value: &str, exit: i32, json: bool) {
    let err = stderr(out);
    assert_eq!(out.status.code(), Some(exit), "{value}: {err}");
    assert!(
        err.contains("providers.y.key_secret: this looks like a credential"),
        "{value}: {err}"
    );
    assert!(
        !err.contains(value) && !stdout(out).contains(value),
        "{err}"
    );
    if json {
        let doc: serde_json::Value = serde_json::from_str(err.trim()).unwrap();
        assert!(
            doc["error"]["code"]
                .as_str()
                .unwrap()
                .starts_with("config.credential")
        );
    }
}

#[test]
fn credential_in_a_secret_reference_key_is_refused_without_echoing_it() {
    let home = temp_home();
    run(home.path(), &["config", "path"]);
    let key = "providers.y.key_secret";
    for &value in SECRET_KEY_CREDENTIALS {
        for json in [false, true] {
            let flag = if json { "--json" } else { "-v" };
            let out = run(home.path(), &[flag, "config", "set", key, value]);
            assert_refused_without_value(&out, value, 2, json);
            let set = format!("{key}={value}");
            let out = run(home.path(), &[flag, "--set", &set, "config", "get", key]);
            assert_refused_without_value(&out, value, 2, json);
        }
        let written = std::fs::read_to_string(config_path(home.path())).unwrap();
        assert!(!written.contains(value), "{written}");
    }
    for &value in SECRET_KEY_CREDENTIALS {
        let text = format!("[providers.y]\nkey_secret = \"{value}\"\n");
        std::fs::write(config_path(home.path()), text).unwrap();
        for json in [false, true] {
            let flag = if json { "--json" } else { "-v" };
            let out = run(home.path(), &[flag, "config", "validate"]);
            assert_refused_without_value(&out, value, 78, json);
        }
    }
}

#[test]
fn bad_secret_name_error_does_not_repeat_the_value() {
    let home = temp_home();
    let out = run(
        home.path(),
        &["config", "set", "providers.y.key_secret", "my-token-value"],
    );
    assert_eq!(out.status.code(), Some(2));
    let err = stderr(&out);
    assert!(
        err.contains("providers.y.key_secret: not a secret name"),
        "{err}"
    );
    assert!(!err.contains("my-token-value"), "{err}");
}

#[cfg(unix)]
mod edit {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::Stdio;

    use super::{command, config_path, temp_home};

    /// An editor that writes `first` on its first run and `second` after.
    fn editor(dir: &Path, first: &str, second: &str) -> std::path::PathBuf {
        let script = dir.join("fake-editor.sh");
        let count = dir.join("runs");
        let body = format!(
            "#!/bin/sh\nif [ -f '{c}' ]; then printf '%s' '{second}' > \"$1\"; else touch '{c}'; printf '%s' '{first}' > \"$1\"; fi\n",
            c = count.display()
        );
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    fn edit(home: &Path, editor: &Path, answer: &str) -> std::process::Output {
        use std::io::Write as _;
        let mut child = command(None)
            .env("EDITOR", editor)
            .arg("--home")
            .arg(home)
            .args(["config", "edit"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(answer.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    #[test]
    fn invalid_edit_is_rejected_and_abort_keeps_the_file() {
        let home = temp_home();
        let tools = temp_home();
        super::run(home.path(), &["config", "path"]);
        let before = std::fs::read_to_string(config_path(home.path())).unwrap();
        let script = editor(tools.path(), "[approvals]\nmode = \"of\"\n", "unused");
        let out = edit(home.path(), &script, "n\n");
        assert_eq!(out.status.code(), Some(78));
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("approvals.mode"), "{err}");
        assert!(err.contains("Re-open the editor"), "{err}");
        assert_eq!(
            std::fs::read_to_string(config_path(home.path())).unwrap(),
            before
        );
        assert!(
            !home
                .path()
                .join("profiles/default/config.edit.toml")
                .exists()
        );
    }

    #[test]
    fn credential_in_the_edited_file_is_a_validation_error() {
        let home = temp_home();
        let tools = temp_home();
        super::run(home.path(), &["config", "path"]);
        let before = std::fs::read_to_string(config_path(home.path())).unwrap();
        let script = editor(
            tools.path(),
            "[display]\ntheme = \"sk-live-123456\"\n",
            "unused",
        );
        let out = edit(home.path(), &script, "n\n");
        assert_eq!(out.status.code(), Some(78));
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains("display.theme: this looks like a credential"),
            "{err}"
        );
        assert_eq!(
            std::fs::read_to_string(config_path(home.path())).unwrap(),
            before
        );
    }

    #[test]
    fn reopened_editor_can_fix_the_file() {
        let home = temp_home();
        let tools = temp_home();
        let script = editor(
            tools.path(),
            "[approvals]\nmode = \"of\"\n",
            "[approvals]\nmode = \"smart\"\n",
        );
        let out = edit(home.path(), &script, "y\n");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(config_path(home.path())).unwrap(),
            "[approvals]\nmode = \"smart\"\n"
        );
    }
}
