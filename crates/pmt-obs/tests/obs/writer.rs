//! The trajectory writer (spec: observability, Trajectories are lossless
//! and redacted; Trajectory retention and size limits; security-boundaries,
//! Known-secret redaction everywhere).

use std::path::Path;

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope, SecretsHandle};
use pmt_core::secret::{Redactor, SecretString};
use pmt_obs::trajectory::payload::{MessageText, ToolResult};
use pmt_obs::trajectory::writer::{Emit, TrajectoryWriter, WriterOptions, open_for_session};
use pmt_obs::trajectory::{Event, Line, files, read};

const SESSION: &str = "ses_01JB7Z3Q5X8V2K4M6N8P0R2T4W";

fn options(dir: &Path, max_bytes: u64, redactor: Option<Redactor>) -> WriterOptions {
    WriterOptions {
        dir: dir.to_owned(),
        session_id: SESSION.to_owned(),
        root_session_id: SESSION.to_owned(),
        max_bytes,
        redactor,
    }
}

fn tool_result(output: &str) -> Event {
    Event::ToolResult(ToolResult {
        call_id: "tc_1".to_owned(),
        tool: "terminal".to_owned(),
        ok: true,
        duration_ms: Some(5),
        output: Some(output.to_owned()),
        error: None,
    })
}

fn lines(path: &Path) -> Vec<Line> {
    read::read_file(path).unwrap()
}

#[test]
fn a_secret_echoed_by_a_tool_is_redacted_before_it_reaches_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let token = SecretString::new("gho_16C7e42F292c6912E7710c838347Ae178B4a");
    let redactor = Redactor::named([("GITHUB_TOKEN", &token)]);
    let writer = TrajectoryWriter::open(options(dir.path(), u64::MAX, Some(redactor))).unwrap();
    let echoed = format!("$ echo $GITHUB_TOKEN\n{}\n", token.expose());
    writer
        .blocking_record(Emit::now(tool_result(&echoed)))
        .unwrap();
    writer
        .blocking_record(Emit::now(tool_result("key sk-proj-abcdefghijklmnopqrstuv")))
        .unwrap();
    let path = writer.close().unwrap();

    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains(token.expose()), "{raw}");
    assert!(!raw.contains("sk-proj-abcdefghijklmnopqrstuv"), "{raw}");
    let stored = lines(&path);
    assert_eq!(
        stored[0].payload["output"],
        "$ echo $GITHUB_TOKEN\n[REDACTED:GITHUB_TOKEN]\n"
    );
    assert_eq!(stored[1].payload["output"], "key [REDACTED]");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[test]
fn past_the_size_limit_one_error_event_is_written_and_headers_continue() {
    let dir = tempfile::tempdir().unwrap();
    let writer = TrajectoryWriter::open(options(dir.path(), 2_000, None)).unwrap();
    for _ in 0..6 {
        writer
            .blocking_record(Emit::now(tool_result(&"o".repeat(600))))
            .unwrap();
    }
    let path = writer.close().unwrap();
    let stored = lines(&path);
    let seqs: Vec<u64> = stored.iter().map(|l| l.seq).collect();
    assert_eq!(seqs, (1..=7).collect::<Vec<_>>(), "no event is dropped");
    let errors: Vec<&Line> = stored.iter().filter(|l| l.kind == "error").collect();
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].payload["code"], "trajectory.size_limit");
    let error_at = stored.iter().position(|l| l.kind == "error").unwrap();
    for line in &stored[..error_at] {
        assert_eq!(line.payload["output"].as_str().unwrap().len(), 600);
    }
    for line in &stored[error_at + 1..] {
        assert_eq!(line.kind, "tool.result");
        assert!(line.payload["omitted_bytes"].as_u64().unwrap() > 600);
        assert_eq!(line.payload.as_object().unwrap().len(), 1);
    }
    // Every line before the error fits in the limit.
    let before: usize = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .take(error_at)
        .map(|l| l.len() + 1)
        .sum();
    assert!(before <= 2_000, "{before}");
}

#[test]
fn a_full_queue_slows_producers_down_and_loses_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let writer = std::sync::Arc::new(
        TrajectoryWriter::with_capacity(options(dir.path(), u64::MAX, None), 4).unwrap(),
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut tasks = Vec::new();
        for producer in 0..4 {
            let writer = std::sync::Arc::clone(&writer);
            tasks.push(tokio::spawn(async move {
                for i in 0..500 {
                    let text = format!("{producer}-{i}");
                    let event = Event::UserMessage(MessageText {
                        message_id: None,
                        text,
                    });
                    writer.record(Emit::now(event)).await.unwrap();
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
    });
    let writer = std::sync::Arc::into_inner(writer).unwrap();
    let path = writer.close().unwrap();
    let stored = lines(&path);
    assert_eq!(stored.len(), 2_000);
    let seqs: Vec<u64> = stored.iter().map(|l| l.seq).collect();
    assert_eq!(seqs, (1..=2_000).collect::<Vec<_>>());
    for producer in 0..4 {
        let mine: Vec<String> = stored
            .iter()
            .filter_map(|l| l.payload["text"].as_str())
            .filter(|t| t.starts_with(&format!("{producer}-")))
            .map(str::to_owned)
            .collect();
        let expected: Vec<String> = (0..500).map(|i| format!("{producer}-{i}")).collect();
        assert_eq!(mine, expected, "each producer's events keep their order");
    }
}

#[test]
fn session_end_compresses_and_a_reopened_session_continues_the_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let writer = TrajectoryWriter::open(options(dir.path(), u64::MAX, None)).unwrap();
    writer
        .blocking_record(Emit::now(tool_result("one")))
        .unwrap();
    let compressed = writer.finish().unwrap();
    assert_eq!(compressed, files::compressed(dir.path(), SESSION));
    assert!(!files::plain(dir.path(), SESSION).exists());

    let writer = TrajectoryWriter::open(options(dir.path(), u64::MAX, None)).unwrap();
    writer
        .blocking_record(Emit::now(tool_result("two")))
        .unwrap();
    let compressed = writer.finish().unwrap();
    let stored = lines(&compressed);
    let seqs: Vec<u64> = stored.iter().map(|l| l.seq).collect();
    assert_eq!(seqs, [1, 2]);
    assert_eq!(stored[1].payload["output"], "two");
}

fn scope(home: &Path, set: &[&str]) -> ProfileScope {
    let home = Home::open(home).unwrap();
    let profile = home.profile(&ProfileName::default_profile()).unwrap();
    ProfileScope::open(profile, &Overrides::parse(set).unwrap(), Launch::Other).unwrap()
}

#[test]
fn disabling_trajectories_only_affects_sessions_started_afterwards() {
    let home = tempfile::tempdir().unwrap();
    let enabled = scope(home.path(), &[]);
    let running = open_for_session(&enabled, SESSION, SESSION)
        .unwrap()
        .unwrap();

    let disabled = scope(home.path(), &["observability.trajectories.enabled=false"]);
    let later = "ses_01JB7Z3Q5X8V2K4M6N8P0R2T4X";
    assert!(open_for_session(&disabled, later, later).unwrap().is_none());

    running
        .blocking_record(Emit::now(tool_result("still recorded")))
        .unwrap();
    let path = running.close().unwrap();
    assert_eq!(
        path,
        enabled
            .path("trajectories")
            .join(format!("{SESSION}.jsonl"))
    );
    assert_eq!(lines(&path).len(), 1);
    assert!(files::find(&enabled.path("trajectories"), later).is_none());
}

#[test]
fn the_profile_secret_store_names_the_redacted_value() {
    let home = tempfile::tempdir().unwrap();
    let scope = scope(home.path(), &[]);
    let secrets: &SecretsHandle = scope.secrets();
    let value = SecretString::new("ghp_storedTokenValue1234567890");
    secrets.store().set("GITHUB_TOKEN", &value).unwrap();
    let writer = open_for_session(&scope, SESSION, SESSION).unwrap().unwrap();
    writer
        .blocking_record(Emit::now(tool_result(value.expose())))
        .unwrap();
    let path = writer.close().unwrap();
    assert_eq!(lines(&path)[0].payload["output"], "[REDACTED:GITHUB_TOKEN]");
}
