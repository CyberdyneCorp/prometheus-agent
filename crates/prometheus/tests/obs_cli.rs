//! `prometheus logs` and `prometheus trajectory show` (spec: cli, Log
//! viewer; observability, Log viewer, Structured logs, Canonical text
//! rendering, Grammar versioning and forward compatibility).

mod common;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::id::SessionId;
use pmt_store::{LineageKind, NewSession, Source, Store};
use serde_json::{Value, json};

use common::{ok, run, stderr, stdout, temp_home};

fn init(home: &Path) {
    ok(&run(home, &["profile", "list"]));
}

fn profile_log(home: &Path) -> PathBuf {
    home.join("profiles/default/logs/prometheus.log")
}

fn record(ts: &str, level: &str, component: &str, session: Option<&str>, msg: &str) -> String {
    let value = json!({
        "ts": ts, "level": level, "component": component, "profile": "default",
        "session_id": session, "turn_id": null, "trace_id": null, "msg": msg,
        "fields": {"n": 1},
    });
    format!("{value}\n")
}

fn write_log(path: &Path, records: &[String]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, records.concat()).unwrap();
}

fn body_lines(out: &str) -> Vec<&str> {
    out.lines().filter(|l| !l.starts_with("==> ")).collect()
}

#[test]
fn the_default_shows_the_last_50_agent_records_under_a_header_naming_the_file() {
    let home = temp_home();
    init(home.path());
    let mut records: Vec<String> = (0..60)
        .map(|i| {
            record(
                "2026-05-01T10:00:00.000000Z",
                "info",
                "agent",
                None,
                &format!("m{i:02}"),
            )
        })
        .collect();
    records.push(record(
        "2026-05-01T10:00:00.000000Z",
        "info",
        "cron",
        None,
        "cron run",
    ));
    write_log(&profile_log(home.path()), &records);

    let out = ok(&run(home.path(), &["logs"]));
    let header = format!("==> {} <==", profile_log(home.path()).display());
    assert_eq!(out.lines().next().unwrap(), header);
    let body = body_lines(&out);
    assert_eq!(body.len(), 50);
    assert_eq!(body[0], "2026-05-01T10:00:00.000000Z INFO  agent: m10 n=1");
    assert!(body[49].ends_with("m59 n=1"));

    let out = ok(&run(home.path(), &["logs", "-n", "3"]));
    let body = body_lines(&out);
    assert_eq!(body.len(), 3);
    assert!(body[2].contains("m59"));

    let out = ok(&run(home.path(), &["logs", "cron", "--lines", "5"]));
    assert_eq!(
        body_lines(&out),
        ["2026-05-01T10:00:00.000000Z INFO  cron: cron run n=1"]
    );
}

#[test]
fn the_header_names_the_home_level_file_of_update() {
    let home = temp_home();
    init(home.path());
    ok(&run(home.path(), &["profile", "create", "work"]));
    let out = ok(&run(home.path(), &["-p", "work", "logs", "update"]));
    let expected = home.path().join("logs/update.log");
    assert_eq!(out, format!("==> {} <==\n", expected.display()));
}

#[test]
fn level_since_and_errors_filter_records() {
    let home = temp_home();
    init(home.path());
    let now = jiff::Timestamp::now();
    let old = (now - std::time::Duration::from_hours(3)).to_string();
    let recent = (now - std::time::Duration::from_mins(10)).to_string();
    write_log(
        &profile_log(home.path()),
        &[
            record(&old, "error", "agent", None, "old error"),
            record(&recent, "info", "agent", None, "recent info"),
            record(&recent, "warn", "agent", None, "recent warn"),
            record(&recent, "error", "sandbox", None, "sandbox error"),
        ],
    );
    let msgs = |args: &[&str]| -> Vec<String> {
        let out = ok(&run(home.path(), args));
        body_lines(&out)
            .iter()
            .map(|l| {
                l.split(": ")
                    .nth(1)
                    .unwrap()
                    .trim_end_matches(" n=1")
                    .to_owned()
            })
            .collect()
    };
    assert_eq!(
        msgs(&["logs", "--level", "warn"]),
        ["old error", "recent warn"]
    );
    assert_eq!(
        msgs(&["logs", "--since", "1h"]),
        ["recent info", "recent warn"]
    );
    let since = (now - std::time::Duration::from_hours(4)).to_string();
    assert_eq!(
        msgs(&["logs", "--since", &since, "--level", "error"]),
        ["old error"]
    );
    assert_eq!(
        msgs(&["logs", "errors"]),
        ["old error", "recent warn", "sandbox error"]
    );
}

#[test]
fn invalid_since_unknown_component_and_unknown_level_exit_2() {
    let home = temp_home();
    init(home.path());
    let out = run(home.path(), &["logs", "--since", "yesterday"]);
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        stderr(&out),
        "error: invalid --since value 'yesterday': use 30m, 2h, 1d or an RFC 3339 time\n"
    );
    assert!(stdout(&out).is_empty());

    let out = run(home.path(), &["logs", "chat"]);
    assert_eq!(out.status.code(), Some(2));
    let text = stderr(&out);
    assert!(text.contains("'chat'"), "{text}");
    for allowed in pmt_obs::logs::COMPONENTS {
        assert!(text.contains(allowed), "{allowed} missing from {text}");
    }

    let out = run(home.path(), &["logs", "--level", "loud"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        stderr(&out).contains("error, warn, info, debug, trace"),
        "{}",
        stderr(&out)
    );
}

fn scope(home: &Path) -> ProfileScope {
    let home = Home::open(home).unwrap();
    let profile = home.profile(&ProfileName::default_profile()).unwrap();
    ProfileScope::open(profile, &Overrides::default(), Launch::Other).unwrap()
}

#[test]
fn session_and_root_select_one_session_or_a_whole_lineage() {
    let home = temp_home();
    init(home.path());
    let (root, continuation, child, other) = {
        let store = Store::open(&scope(home.path())).unwrap();
        let root = store
            .create_session(&NewSession::new(Source::Cli))
            .unwrap()
            .id;
        let continuation = store
            .create_session(&NewSession::child(
                Source::Cli,
                root,
                LineageKind::Compression,
            ))
            .unwrap()
            .id;
        let child = store
            .create_session(&NewSession::child(
                Source::Subagent,
                continuation,
                LineageKind::Delegate,
            ))
            .unwrap()
            .id;
        let other = store
            .create_session(&NewSession::new(Source::Cli))
            .unwrap()
            .id;
        (root, continuation, child, other)
    };
    let ts = "2026-05-01T10:00:00.000000Z";
    let ids = [root, continuation, child, other].map(|id| id.to_string());
    let records: Vec<String> = ids
        .iter()
        .map(|id| record(ts, "info", "agent", Some(id), &format!("from {id}")))
        .collect();
    write_log(&profile_log(home.path()), &records);

    let out = ok(&run(home.path(), &["logs", "--session", &ids[1]]));
    assert_eq!(body_lines(&out).len(), 1);
    assert!(out.contains(&format!("from {}", ids[1])));

    let out = ok(&run(home.path(), &["logs", "--root", &ids[1]]));
    let body = body_lines(&out);
    assert_eq!(body.len(), 3, "{out}");
    assert!(!out.contains(&format!("from {}", ids[3])));

    let out = run(
        home.path(),
        &["logs", "--session", &ids[0], "--root", &ids[0]],
    );
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn json_output_is_one_document_and_rotated_files_are_named_when_read() {
    let home = temp_home();
    init(home.path());
    let log = profile_log(home.path());
    let ts = "2026-05-01T10:00:00.000000Z";
    write_log(
        &pmt_obs::logs::rotated(&log, 1),
        &[record(ts, "info", "agent", None, "older")],
    );
    write_log(&log, &[record(ts, "info", "agent", None, "newer")]);

    let out = ok(&run(home.path(), &["logs", "--json"]));
    let doc: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(doc["schema"], "prometheus.logs.v1");
    assert_eq!(
        doc["files"],
        json!([
            log.display().to_string(),
            pmt_obs::logs::rotated(&log, 1).display().to_string()
        ])
    );
    let msgs: Vec<&str> = doc["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["msg"].as_str().unwrap())
        .collect();
    assert_eq!(msgs, ["older", "newer"]);

    let out = ok(&run(home.path(), &["logs", "-n", "1"]));
    assert_eq!(
        out.lines().filter(|l| l.starts_with("==> ")).count(),
        1,
        "{out}"
    );

    let list: Value =
        serde_json::from_str(&ok(&run(home.path(), &["logs", "list", "--json"]))).unwrap();
    assert_eq!(list["schema"], "prometheus.log_components.v1");
    assert_eq!(
        list["components"].as_array().unwrap().len(),
        pmt_obs::logs::COMPONENTS.len()
    );
    let text = ok(&run(home.path(), &["logs", "list"]));
    assert!(text.contains(&format!(
        "update     {}",
        home.path().join("logs/update.log").display()
    )));
}

#[cfg(unix)]
#[test]
fn a_store_warning_is_logged_to_the_profile_with_its_context() {
    use std::os::unix::fs::PermissionsExt as _;
    let home = temp_home();
    init(home.path());
    ok(&run(home.path(), &["sessions", "list"]));
    let db = home.path().join("profiles/default/state.db");
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644)).unwrap();
    ok(&run(home.path(), &["sessions", "list"]));

    let out = ok(&run(home.path(), &["logs", "errors", "--json"]));
    let doc: Value = serde_json::from_str(&out).unwrap();
    let records = doc["records"].as_array().unwrap();
    let tightened = records
        .iter()
        .find(|r| r["msg"].as_str().unwrap().contains("tightened to 600"))
        .unwrap_or_else(|| panic!("{out}"));
    assert_eq!(tightened["level"], "warn");
    assert_eq!(tightened["profile"], "default");
    assert_eq!(tightened["component"], "agent");
    let mode = std::fs::metadata(profile_log(home.path()))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[cfg(unix)]
#[test]
fn following_a_session_prints_old_and_new_records_until_interrupted_then_exits_130() {
    use std::io::{BufRead as _, BufReader};
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let home = temp_home();
    init(home.path());
    let session = "ses_01JB7Z3Q5X8V2K4M6N8P0R2T4W";
    let ts = "2026-05-01T10:00:00.000000Z";
    let log = profile_log(home.path());
    write_log(
        &log,
        &[
            record(ts, "info", "agent", Some(session), "existing"),
            record(
                ts,
                "info",
                "agent",
                Some("ses_01JB7Z3Q5X8V2K4M6N8P0R2T4X"),
                "other",
            ),
        ],
    );
    let mut child = common::command(home.path())
        .args(["logs", "-f", "--session", session])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    assert!(lines.next().unwrap().unwrap().starts_with("==> "));
    assert!(lines.next().unwrap().unwrap().ends_with("existing n=1"));

    let mut appended = std::fs::read_to_string(&log).unwrap();
    appended.push_str(&record(
        ts,
        "info",
        "agent",
        Some("ses_01JB7Z3Q5X8V2K4M6N8P0R2T4X"),
        "other new",
    ));
    appended.push_str(&record(ts, "warn", "agent", Some(session), "new"));
    std::fs::write(&log, appended).unwrap();
    let started = Instant::now();
    let next = lines.next().unwrap().unwrap();
    assert!(next.ends_with(": new n=1"), "{next}");
    assert!(started.elapsed() < Duration::from_secs(5));

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(i32::try_from(child.id()).unwrap()),
        nix::sys::signal::Signal::SIGINT,
    )
    .unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(130));
    assert!(lines.next().is_none(), "nothing else was printed");
}

fn golden(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../pmt-obs/tests/obs/golden")
        .join(name)
}

const GOLDEN_SESSION: &str = "ses_01JB7Z3Q5X8V2K4M6N8P0R2T4W";

fn install_trajectory(home: &Path, text: &str) -> PathBuf {
    let dir = home.join("profiles/default/trajectories");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{GOLDEN_SESSION}.jsonl"));
    std::fs::write(&path, text).unwrap();
    path
}

#[test]
fn trajectory_show_renders_the_canonical_text_and_the_stored_lines() {
    let home = temp_home();
    init(home.path());
    let jsonl = std::fs::read_to_string(golden("recorded.jsonl")).unwrap();
    let path = install_trajectory(home.path(), &jsonl);
    pmt_obs::trajectory::files::compress(&path).unwrap();

    let text = ok(&run(home.path(), &["trajectory", "show", GOLDEN_SESSION]));
    assert_eq!(
        text,
        std::fs::read_to_string(golden("recorded.txt")).unwrap()
    );
    let raw = ok(&run(
        home.path(),
        &["trajectory", "show", GOLDEN_SESSION, "--format", "jsonl"],
    ));
    assert_eq!(raw, jsonl);
    let clipped = ok(&run(
        home.path(),
        &["trajectory", "show", GOLDEN_SESSION, "--max-chars", "20"],
    ));
    assert!(clipped.starts_with("[USER]\nSummarise not…(+"), "{clipped}");
    let doc: Value = serde_json::from_str(&ok(&run(
        home.path(),
        &["trajectory", "show", GOLDEN_SESSION, "--json"],
    )))
    .unwrap();
    assert_eq!(doc["schema"], "prometheus.trajectory.v1");
    assert_eq!(
        doc["events"].as_array().unwrap().len(),
        jsonl.lines().count()
    );
}

#[test]
fn a_newer_major_grammar_exits_76_and_a_missing_trajectory_66() {
    let home = temp_home();
    init(home.path());
    let missing = run(home.path(), &["trajectory", "show", GOLDEN_SESSION]);
    assert_eq!(missing.status.code(), Some(66), "{}", stderr(&missing));

    let mut newer = String::new();
    for line in std::fs::read_to_string(golden("recorded.jsonl"))
        .unwrap()
        .lines()
    {
        let _ = writeln!(newer, "{}", line.replace("\"v\":\"1.0\"", "\"v\":\"2.1\""));
    }
    install_trajectory(home.path(), &newer);
    let out = run(home.path(), &["trajectory", "show", GOLDEN_SESSION]);
    assert_eq!(out.status.code(), Some(76));
    assert_eq!(
        stderr(&out),
        "error: trajectory grammar 2.1 is newer than supported 1.0; run 'prometheus update'\n"
    );
    let _ = SessionId::parse(GOLDEN_SESSION).unwrap();
}
