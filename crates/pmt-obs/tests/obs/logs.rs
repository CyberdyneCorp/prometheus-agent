//! Structured logs (spec: observability, Structured logs, scenario "Turn
//! context on every record"; cli, Log viewer: rotation at 10 MiB keeping 5
//! files).

use std::path::{Path, PathBuf};

use pmt_config::schema::LogLevel;
use pmt_core::secret::{Redactor, SecretString};
use pmt_obs::logs::{self, LogSetup, Record};
use serde_json::Value;
use tracing_subscriber::prelude::*;

fn home() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("profiles/default")).unwrap();
    home
}

fn setup(home: &Path, env_filter: Option<&str>, redactor: Redactor) -> LogSetup {
    LogSetup {
        home: Some(home.to_owned()),
        env_filter: env_filter.map(str::to_owned),
        redactor,
        stderr: false,
    }
}

fn profile_log(home: &Path) -> PathBuf {
    home.join("profiles/default/logs/prometheus.log")
}

fn records(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

#[test]
fn a_tool_record_inside_a_turn_carries_the_turn_context() {
    let home = home();
    let (layer, _handle) = logs::layer(setup(home.path(), None, Redactor::default()));
    tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
        let session = tracing::info_span!(
            "session",
            profile = "default",
            session_id = "ses_01JB7Z3Q5X8V2K4M6N8P0R2T4W"
        );
        let _session = session.enter();
        let turn = tracing::info_span!("turn", turn_id = "turn_7", trace_id = "4bf92f3577b34da6");
        let _turn = turn.enter();
        let tool = tracing::debug_span!("tool", component = "sandbox", tool = "terminal");
        let _tool = tool.enter();
        tracing::info!(exit_code = 0, command = "ls", "command finished");
    });
    let stored = records(&profile_log(home.path()));
    assert_eq!(stored.len(), 1);
    let record = &stored[0];
    let mut keys: Vec<&str> = record
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "component",
            "fields",
            "level",
            "msg",
            "profile",
            "session_id",
            "trace_id",
            "ts",
            "turn_id"
        ]
    );
    assert_eq!(record["session_id"], "ses_01JB7Z3Q5X8V2K4M6N8P0R2T4W");
    assert_eq!(record["turn_id"], "turn_7");
    assert_eq!(record["trace_id"], "4bf92f3577b34da6");
    assert_eq!(record["component"], "sandbox");
    assert_eq!(record["level"], "info");
    assert_eq!(record["msg"], "command finished");
    assert_eq!(
        record["fields"],
        serde_json::json!({"exit_code": 0, "command": "ls"})
    );
    let parsed: Record = serde_json::from_value(record.clone()).unwrap();
    assert_eq!(parsed.profile.as_deref(), Some("default"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(profile_log(home.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[test]
fn records_without_a_profile_and_home_components_go_to_the_home_logs() {
    let home = home();
    let (layer, _handle) = logs::layer(setup(home.path(), None, Redactor::default()));
    tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
        tracing::warn!("ignoring HERMES_HOME");
        tracing::info!(
            component = "update",
            profile = "default",
            "downloaded 1.2.0"
        );
    });
    let startup = records(&home.path().join("logs/prometheus.log"));
    assert_eq!(startup[0]["msg"], "ignoring HERMES_HOME");
    assert_eq!(startup[0]["profile"], Value::Null);
    let update = records(&home.path().join("logs/update.log"));
    assert_eq!(update[0]["msg"], "downloaded 1.2.0");
    assert!(!profile_log(home.path()).exists());
}

#[test]
fn the_level_comes_from_the_environment_else_from_logging_level() {
    let home = home();
    let (layer, handle) = logs::layer(setup(home.path(), None, Redactor::default()));
    tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
        tracing::debug!("hidden at the default info level");
        tracing::info!("shown");
        handle.set_level(LogLevel::Error);
        tracing::warn!("hidden at error");
        tracing::error!("shown too");
    });
    let msgs: Vec<Value> = records(&home.path().join("logs/prometheus.log"))
        .into_iter()
        .map(|r| r["msg"].clone())
        .collect();
    assert_eq!(msgs, ["shown", "shown too"]);

    let home = self::home();
    let (layer, handle) = logs::layer(setup(home.path(), Some("warn"), Redactor::default()));
    tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
        handle.set_level(LogLevel::Trace);
        tracing::info!("PMT_LOG=warn wins over logging.level");
        tracing::warn!("kept");
    });
    let msgs: Vec<Value> = records(&home.path().join("logs/prometheus.log"))
        .into_iter()
        .map(|r| r["msg"].clone())
        .collect();
    assert_eq!(msgs, ["kept"]);
}

#[test]
fn records_are_redacted_like_trajectories() {
    let home = home();
    let launch = SecretString::new("sk-launch-value-0123456789");
    let stored = SecretString::new("xoxb-stored-slack-token-123456");
    let redactor = Redactor::named([("OPENAI_API_KEY", &launch)]);
    let (layer, handle) = logs::layer(setup(home.path(), None, redactor));
    handle.register_profile("default", Redactor::named([("SLACK_BOT_TOKEN", &stored)]));
    tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
        let span = tracing::info_span!("session", profile = "default");
        let _span = span.enter();
        tracing::info!(
            header = format!("Bearer {}", launch.expose()),
            "posted with {}",
            stored.expose()
        );
    });
    let text = std::fs::read_to_string(profile_log(home.path())).unwrap();
    assert!(
        !text.contains(launch.expose()) && !text.contains(stored.expose()),
        "{text}"
    );
    let record = &records(&profile_log(home.path()))[0];
    assert_eq!(record["msg"], "posted with [REDACTED:SLACK_BOT_TOKEN]");
    assert_eq!(
        record["fields"]["header"],
        "Bearer [REDACTED:OPENAI_API_KEY]"
    );
}

#[test]
fn logs_rotate_at_the_size_limit_keeping_five_files() {
    assert_eq!(logs::MAX_BYTES, 10 * 1024 * 1024);
    assert_eq!(logs::KEEP, 5);
    let home = home();
    let (layer, _handle) = logs::layer(setup(home.path(), None, Redactor::default()));
    let layer = layer.with_rotation(2_000, logs::KEEP);
    tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
        for i in 0..200 {
            tracing::info!(i, "record {i:04} {}", "x".repeat(40));
        }
    });
    let current = home.path().join("logs/prometheus.log");
    let mut all = vec![current.clone()];
    all.extend((1..logs::KEEP).map(|n| logs::rotated(&current, n)));
    for path in &all {
        let size = std::fs::metadata(path).unwrap().len();
        assert!(size <= 2_000, "{} is {size} bytes", path.display());
    }
    assert!(
        !logs::rotated(&current, logs::KEEP).exists(),
        "5 files in all"
    );
    let last = records(&current).last().unwrap().clone();
    assert!(last["msg"].as_str().unwrap().starts_with("record 0199"));
    let newest_rotated = records(&logs::rotated(&current, 1));
    let first_current = &records(&current)[0];
    let previous: u64 = newest_rotated.last().unwrap()["fields"]["i"]
        .as_u64()
        .unwrap();
    assert_eq!(
        first_current["fields"]["i"].as_u64().unwrap(),
        previous + 1,
        "no record lost at rotation"
    );
}
