//! `prometheus replay` and `prometheus cache` against mock sessions
//! (spec: replay-and-evaluation, Session recordings; Replay modes;
//! Divergence detection; Replay has no side effects; Cache administration
//! and limits): sessions recorded by the built binary, exported, replayed
//! offline and compared.

mod common;

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use common::{command, newest_session, ok, stderr, stdout, temp_home};
use pmt_eval::compare::loop_events;
use pmt_eval::recording::{Recording, member};
use serde_json::Value;

/// Runs a one-shot chat of `prompt` in `<home>/work` with the mock
/// `script`; returns the session id.
fn record(home: &Path, script: &str, prompt: &str) -> String {
    let work = home.join("work");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("README.md"), "# Ledger\nTotal: 85.15\n").unwrap();
    let path = home.join("s.toml");
    std::fs::write(&path, script).unwrap();
    let out = command(home)
        .current_dir(&work)
        .args(["chat", "-z", prompt, "--provider", "mock"])
        .arg("--mock-script")
        .arg(&path)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    ok(&out);
    newest_session(home)
}

fn replay(home: &Path, args: &[&str]) -> Output {
    command(home)
        .arg("replay")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn export(home: &Path, session: &str, extra: &[&str]) -> PathBuf {
    let file = home.join(format!("{session}.pmtrec"));
    let mut args = vec!["export", session, "-o", file.to_str().unwrap()];
    args.extend(extra);
    ok(&replay(home, &args));
    file
}

const READ_THEN_DONE: &str = "[[rule]]\nstep = 0\n[[rule.tool_calls]]\nname = \"read_file\"\n\
     arguments = { path = \"README.md\" }\n[[rule]]\nstep = 1\ntext = \"done\"\n";

fn trajectory(recording: &Recording) -> String {
    let root = recording.root().unwrap();
    String::from_utf8(recording.get(&member::trajectory(root)).unwrap().to_vec()).unwrap()
}

/// Replaying a recorded session offline reproduces the trajectory: every
/// call is served, and the agent-loop events equal the original's once ids
/// and timestamps are removed.
#[test]
fn a_strict_replay_of_an_unchanged_build_reproduces_the_trajectory() {
    let home = temp_home();
    let session = record(home.path(), READ_THEN_DONE, "summarise");
    let file = export(home.path(), &session, &[]);
    let out_dir = home.path().join("replayed");
    let out = replay(
        home.path(),
        &[
            "run",
            file.to_str().unwrap(),
            "--out",
            out_dir.to_str().unwrap(),
        ],
    );
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{text}\n{}", stderr(&out));
    assert!(text.starts_with("replay matched: 2 calls\n"), "{text}");
    let recording = Recording::open(&file).unwrap();
    let replayed = std::fs::read_to_string(out_dir.join("trajectory.jsonl")).unwrap();
    let original = loop_events(&trajectory(&recording));
    assert!(original.iter().any(|e| e["type"] == "tool.result"));
    assert_eq!(loop_events(&replayed), original);
}

/// Scenario: Recording without workspace contents.
#[test]
fn a_recording_without_workspace_lists_files_with_hashes_and_no_contents() {
    let home = temp_home();
    let session = record(home.path(), READ_THEN_DONE, "summarise");
    let file = export(home.path(), &session, &[]);
    let recording = Recording::open(&file).unwrap();
    let manifest: Value = recording.json(member::WORKSPACE).unwrap().unwrap();
    let files = manifest["files"].as_array().unwrap();
    let readme = files.iter().find(|f| f["path"] == "README.md").unwrap();
    assert_eq!(readme["size"], 22);
    assert!(readme["sha256"].as_str().unwrap().starts_with("sha256:"));
    assert!(
        recording
            .members()
            .all(|(name, _)| !name.starts_with("workspace/files/")),
        "no file contents without --include-workspace"
    );
    // The manifest lists every member's hash; the state names the route.
    assert!(recording.manifest.sha256.contains_key(member::STATE));
    let state: Value = recording.json(member::STATE).unwrap().unwrap();
    assert_eq!(state["provider"], "mock");
    assert!(state["mock_script"].as_str().unwrap().contains("read_file"));

    let with = export(home.path(), &session, &["--include-workspace"]);
    let with = Recording::open(&with).unwrap();
    assert_eq!(
        with.get(&member::workspace_file("README.md")),
        Some("# Ledger\nTotal: 85.15\n".as_bytes())
    );
}

/// Rewrites one member of a recording's tar without updating the
/// manifest.
fn tamper(file: &Path, target: &str) {
    let bytes = zstd::decode_all(std::fs::File::open(file).unwrap()).unwrap();
    let mut archive = tar::Archive::new(bytes.as_slice());
    let mut builder = tar::Builder::new(Vec::new());
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let name = entry.path().unwrap().to_string_lossy().into_owned();
        let mut data = Vec::new();
        entry.read_to_end(&mut data).unwrap();
        if name == target {
            data.extend_from_slice(b"{\"type\":\"user.message\"}\n");
        }
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o600);
        header.set_entry_type(tar::EntryType::Regular);
        builder
            .append_data(&mut header, &name, data.as_slice())
            .unwrap();
    }
    let tarred = builder.into_inner().unwrap();
    std::fs::write(file, zstd::encode_all(tarred.as_slice(), 3).unwrap()).unwrap();
}

/// Scenario: Tampered recording.
#[test]
fn a_tampered_recording_is_refused_with_exit_65() {
    let home = temp_home();
    let session = record(home.path(), READ_THEN_DONE, "summarise");
    let file = export(home.path(), &session, &[]);
    let member = member::trajectory(&session);
    tamper(&file, &member);
    let out = replay(home.path(), &["run", file.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(65), "{}", stdout(&out));
    assert!(
        stderr(&out).contains(&format!("recording integrity check failed: {member}")),
        "{}",
        stderr(&out)
    );
}

/// The recording as if made by a build whose first system block read
/// `rewrite(text)`: its recorded requests carry that text and the hashes
/// of those requests.
fn as_if_recorded_with(file: &Path, rewrite: impl Fn(&str) -> String) {
    let recording = Recording::open(file).unwrap();
    let root = recording.root().unwrap().to_owned();
    let calls: Vec<Value> = recording.json(&member::calls(&root)).unwrap().unwrap();
    let mut rebuilt = Recording::new(
        &recording.manifest.prometheus_version,
        &recording.manifest.trajectory_grammar,
        recording.manifest.session_ids.clone(),
    );
    for (name, bytes) in recording.members() {
        if !name.starts_with("requests/") && !name.starts_with("responses/") {
            rebuilt.add(name, bytes.to_vec());
        }
    }
    let mut renamed = Vec::new();
    for call in calls {
        let hash = call["request_hash"].as_str().unwrap().to_owned();
        let mut request: Value = recording.json(&member::request(&hash)).unwrap().unwrap();
        let system = request["system"][0]["text"].as_str().unwrap().to_owned();
        request["system"][0]["text"] = Value::String(rewrite(&system));
        let new_hash = pmt_providers::canonical::hash_value(&request);
        let responses: Value = recording.json(&member::response(&hash)).unwrap().unwrap();
        rebuilt.add_json(&member::request(&new_hash), &request);
        rebuilt.add_json(&member::response(&new_hash), &responses);
        renamed
            .push(serde_json::json!({"request_hash": new_hash, "ts": call["ts"], "served": false}));
    }
    rebuilt.add_json(&member::calls(&root), &renamed);
    rebuilt.write(file).unwrap();
}

/// The recording as if made by a build whose tool-guidance layer read
/// differently; returns the recorded line.
fn with_old_guidance(file: &Path) -> String {
    let recording = Recording::open(file).unwrap();
    let root = recording.root().unwrap().to_owned();
    let calls: Vec<Value> = recording.json(&member::calls(&root)).unwrap().unwrap();
    let first = calls[0]["request_hash"].as_str().unwrap().to_owned();
    let request: Value = recording.json(&member::request(&first)).unwrap().unwrap();
    let guidance = request["system"][0]["text"]
        .as_str()
        .unwrap()
        .lines()
        .find(|l| l.contains("tool"))
        .expect("a tool-guidance line")
        .to_owned();
    let old_line = format!("{guidance} (an older wording)");
    let replaced = old_line.clone();
    as_if_recorded_with(file, move |text| text.replacen(&guidance, &replaced, 1));
    old_line
}

/// Scenario: Prompt change detected.
#[test]
fn a_changed_tool_guidance_layer_diverges_at_the_first_model_call_with_a_diff() {
    let home = temp_home();
    let session = record(home.path(), READ_THEN_DONE, "summarise");
    let file = export(home.path(), &session, &[]);
    let old_line = with_old_guidance(&file);
    let out = replay(home.path(), &["run", file.to_str().unwrap()]);
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{text}\n{}", stderr(&out));
    assert!(
        text.starts_with("divergence at step 1 (model.call)\n"),
        "{text}"
    );
    assert!(text.contains("--- recorded\n+++ replayed\n"), "{text}");
    assert!(
        text.lines()
            .any(|l| l.starts_with('-') && l.contains(&old_line)),
        "the diff shows the recorded line:\n{text}"
    );
    assert!(
        text.lines()
            .any(|l| l.starts_with('+')
                && l.contains(old_line.trim_end_matches(" (an older wording)"))),
        "{text}"
    );
}

/// Scenario: Recorded message send is not re-sent: the recorded results
/// of the session's side-effecting calls come back, and nothing runs.
#[test]
fn a_strict_replay_serves_recorded_side_effects_without_running_them() {
    let home = temp_home();
    let script = "[[rule]]\nstep = 0\n[[rule.tool_calls]]\nname = \"send_message\"\n\
         arguments = { target = \"telegram:-1001\", message = \"deploy done\" }\n\
         [[rule.tool_calls]]\nname = \"write_file\"\narguments = { path = \"sent.txt\", content = \"sent\" }\n\
         [[rule]]\nstep = 1\ntext = \"sent\"\n";
    let session = record(home.path(), script, "tell the team");
    assert!(
        home.path().join("work/sent.txt").exists(),
        "the original wrote"
    );
    let file = export(home.path(), &session, &[]);
    let out_dir = home.path().join("replayed");
    let out = replay(
        home.path(),
        &[
            "run",
            file.to_str().unwrap(),
            "--out",
            out_dir.to_str().unwrap(),
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}\n{}",
        stdout(&out),
        stderr(&out)
    );
    assert!(
        !out_dir.join("workspace/sent.txt").exists(),
        "the replay ran a recorded side effect"
    );
    let recording = Recording::open(&file).unwrap();
    let results = |text: &str| -> Vec<Value> {
        loop_events(text)
            .into_iter()
            .filter(|e| e["type"] == "tool.result")
            .collect()
    };
    let replayed = std::fs::read_to_string(out_dir.join("trajectory.jsonl")).unwrap();
    let recorded = results(&trajectory(&recording));
    assert_eq!(recorded.len(), 2);
    assert_eq!(
        results(&replayed),
        recorded,
        "the recorded results came back"
    );
    // Nothing reached a gateway outbox: the replay's home has none.
    assert!(!out_dir.join("home/profiles/default/outbox").exists());
}

/// `tools-live` runs the recorded tool calls for real in a scratch copy
/// of the recorded workspace.
#[test]
fn a_tools_live_replay_runs_the_tools_in_a_copy_of_the_workspace() {
    let home = temp_home();
    let script = "[[rule]]\nstep = 0\n[[rule.tool_calls]]\nname = \"write_file\"\n\
         arguments = { path = \"out.txt\", content = \"written\" }\n[[rule]]\nstep = 1\ntext = \"ok\"\n";
    let session = record(home.path(), script, "write it");
    let without = export(home.path(), &session, &[]);
    let refused = replay(
        home.path(),
        &["run", without.to_str().unwrap(), "--mode", "tools-live"],
    );
    assert_eq!(refused.status.code(), Some(65), "{}", stderr(&refused));
    let file = export(home.path(), &session, &["--include-workspace"]);
    let out_dir = home.path().join("live");
    let out = replay(
        home.path(),
        &[
            "run",
            file.to_str().unwrap(),
            "--mode",
            "tools-live",
            "--out",
            out_dir.to_str().unwrap(),
        ],
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}\n{}",
        stdout(&out),
        stderr(&out)
    );
    assert_eq!(
        std::fs::read_to_string(out_dir.join("workspace/out.txt")).unwrap(),
        "written"
    );
    assert_eq!(
        std::fs::read_to_string(out_dir.join("workspace/README.md")).unwrap(),
        "# Ledger\nTotal: 85.15\n"
    );
}

/// `model-live` sends the replayed requests to the configured provider
/// (here the recorded mock script) and serves the tool results.
#[test]
fn a_model_live_replay_asks_the_provider_and_serves_the_tools() {
    let home = temp_home();
    let session = record(home.path(), READ_THEN_DONE, "summarise");
    let file = export(home.path(), &session, &[]);
    let out = replay(
        home.path(),
        &["run", file.to_str().unwrap(), "--mode", "model-live"],
    );
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(0), "{text}\n{}", stderr(&out));
    assert!(text.starts_with("replay matched: 0 calls\n"), "{text}");
}

/// `--on-divergence continue` falls through to the live provider: the
/// replay completes with the divergence reported and exits 1.
#[test]
fn continuing_past_a_divergence_finishes_live() {
    let home = temp_home();
    let session = record(home.path(), READ_THEN_DONE, "summarise");
    let file = export(home.path(), &session, &[]);
    with_old_guidance(&file);
    let out_dir = home.path().join("continued");
    let out = replay(
        home.path(),
        &[
            "run",
            file.to_str().unwrap(),
            "--on-divergence",
            "continue",
            "--out",
            out_dir.to_str().unwrap(),
        ],
    );
    let text = stdout(&out);
    assert_eq!(out.status.code(), Some(1), "{text}\n{}", stderr(&out));
    assert!(
        text.starts_with("divergence at step 1 (model.call)"),
        "{text}"
    );
    let replayed = std::fs::read_to_string(out_dir.join("trajectory.jsonl")).unwrap();
    assert!(
        replayed.contains("\"text\":\"done\""),
        "the live mock finished the turn"
    );
}

/// `prometheus cache stats|prune|clear` over the demo's cache (the demo
/// stores its calls with `read_write`).
#[test]
fn the_cache_commands_report_and_remove_entries() {
    let base = temp_home();
    let out = command(base.path())
        .args(["demo", "--output"])
        .arg(base.path().join("demo"))
        .arg("--keep")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    ok(&out);
    let home = base.path().join("demo/home");
    let cache = |args: &[&str]| {
        command(&home)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    let stats: Value = serde_json::from_str(&ok(&cache(&["--json", "cache", "stats"]))).unwrap();
    let entries = stats["total"]["entries"].as_u64().unwrap();
    assert!(entries >= 5, "{stats}");
    assert_eq!(stats["namespaces"]["default"]["entries"], entries);
    let pruned = ok(&cache(&["cache", "prune", "--older-than", "30d"]));
    assert!(pruned.starts_with("removed 0 entries"), "{pruned}");
    let cleared = ok(&cache(&["cache", "clear", "--namespace", "default"]));
    assert!(
        cleared.starts_with(&format!("removed {entries} entries")),
        "{cleared}"
    );
    let text = ok(&cache(&["cache", "stats"]));
    assert!(text.starts_with("model-call cache: 0 entries"), "{text}");
}

/// `replay bless` re-records a recording whose build changed: it prints
/// the accepted diff, and the blessed recording then replays cleanly.
#[test]
fn bless_accepts_an_intended_change_and_the_recording_replays_again() {
    let home = temp_home();
    let session = record(home.path(), READ_THEN_DONE, "summarise");
    let file = export(home.path(), &session, &["--include-workspace"]);
    with_old_guidance(&file);
    let blessed = ok(&replay(home.path(), &["bless", file.to_str().unwrap()]));
    assert!(
        blessed.starts_with("divergence at step 1 (model.call)\n"),
        "{blessed}"
    );
    assert!(
        blessed.contains("(an older wording)"),
        "the diff it accepts:\n{blessed}"
    );
    assert!(
        blessed.ends_with(&format!("blessed {}\n", file.display())),
        "{blessed}"
    );
    let text = ok(&replay(home.path(), &["run", file.to_str().unwrap()]));
    assert!(text.starts_with("replay matched: 2 calls\n"), "{text}");
    let again = ok(&replay(home.path(), &["bless", file.to_str().unwrap()]));
    assert!(again.ends_with("unchanged (replay matched)\n"), "{again}");
}

/// Scenario: Unintended prompt drift: of two golden recordings, the one
/// recorded with two system-prompt layers in another order fails the
/// gate by name; the other passes.
#[test]
fn the_golden_gate_names_each_recording_a_layer_reorder_breaks() {
    let home = temp_home();
    let session = record(home.path(), READ_THEN_DONE, "summarise");
    let goldens = home.path().join("golden");
    std::fs::create_dir_all(&goldens).unwrap();
    let kept = goldens.join("kept.pmtrec");
    let drifted = goldens.join("drifted.pmtrec");
    let file = export(home.path(), &session, &[]);
    std::fs::copy(&file, &kept).unwrap();
    std::fs::copy(&file, &drifted).unwrap();
    // Identity and tool guidance, the first two layers of the first block,
    // in the other order.
    as_if_recorded_with(&drifted, |text| {
        let (identity, rest) = text.split_once("\n\n").unwrap();
        let (guidance, tail) = rest.split_once("\n\n").unwrap_or((rest, ""));
        let swapped = format!("{guidance}\n\n{identity}");
        if tail.is_empty() {
            swapped
        } else {
            format!("{swapped}\n\n{tail}")
        }
    });
    let launch = pmt_core::env::LaunchEnv::from_vars(
        [("HOME".into(), common::user_home(home.path()).into())],
        &pmt_core::env::EnvPolicy::default(),
    );
    let (failures, ran) =
        prometheus::replay::gate::check(&goldens, &home.path().join("gate"), &launch).unwrap();
    assert_eq!(ran, 2);
    let names: Vec<&str> = failures.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["drifted.pmtrec"]);
    assert!(
        failures[0]
            .report
            .starts_with("divergence at step 1 (model.call)"),
        "{}",
        failures[0].report
    );
}
