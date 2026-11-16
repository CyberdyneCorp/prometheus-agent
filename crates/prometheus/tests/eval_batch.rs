//! `prometheus eval batch`, statistics, toolset distributions and the
//! trajectory compressor end to end (spec: replay-and-evaluation, Batch
//! prompt datasets; Batch statistics and quality filtering; Toolset
//! distributions; Trajectory compressor).

#![cfg(feature = "eval")]

mod common;
mod eval_common;

use std::path::Path;

use common::{append_config, mock_home, ok, run, stdout};
use eval_common::{json, read_json};
use serde_json::Value;

fn dataset(home: &Path, lines: &[&str]) -> std::path::PathBuf {
    let path = home.join("data.jsonl");
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

fn script(home: &Path, text: &str) {
    let path = home.join("batch.toml");
    std::fs::write(&path, text).unwrap();
    append_config(
        home,
        &format!(
            "\n[providers.mock]\nscript = {}\n",
            eval_common::toml_path(&path)
        ),
    );
}

const ANSWER: &str = "[[rule]]\nrepeat = true\ntext = \"ok\"\n";

fn runs(home: &Path) -> std::path::PathBuf {
    home.join("profiles/default/evals/runs")
}

/// Scenario: Resume after interruption (the first invocation stops after
/// part of the dataset, as an interrupted run leaves it).
#[test]
fn a_resumed_batch_runs_only_the_remaining_prompts() {
    let home = mock_home();
    script(home.path(), ANSWER);
    let lines: Vec<String> = (1..=5)
        .map(|i| format!("{{\"id\": \"p{i}\", \"prompt\": \"prompt {i}\"}}"))
        .collect();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let data = dataset(home.path(), &refs);
    let first = ok(&run(
        home.path(),
        &[
            "eval",
            "batch",
            data.to_str().unwrap(),
            "--max-samples",
            "3",
            "--run-id",
            "bt1",
        ],
    ));
    assert!(
        first.contains("3 executed, 0 failed, 0 skipped, 0 resumed"),
        "{first}"
    );
    let second = ok(&run(
        home.path(),
        &["eval", "batch", data.to_str().unwrap(), "--run-id", "bt1"],
    ));
    assert!(
        second.contains("2 executed, 0 failed, 0 skipped, 3 resumed"),
        "{second}"
    );
    let stats = read_json(&runs(home.path()).join("bt1/statistics.json"));
    assert_eq!(stats["tasks"]["completed"], 5, "{stats}");
    assert_eq!(stats["turns_per_task"]["1"], 5, "{stats}");
}

/// Scenario: Ephemeral prompt not recorded (and it did reach the model,
/// as did the prefill messages).
#[test]
fn the_ephemeral_system_prompt_reaches_the_model_and_no_file() {
    let server = eval_common::FakeProvider::start("Short answer.");
    let home = common::temp_home();
    ok(&run(home.path(), &["config", "path"]));
    std::fs::write(
        home.path().join("profiles/default/config.toml"),
        format!(
            "[model]\nprovider = \"acme\"\ndefault = \"r1\"\n\n[providers.acme]\nbase_url = \"http://127.0.0.1:{}/v1\"\nwire_format = \"chat_completions\"\nauth = \"none\"\nmax_retries = 0\n\n[budgets]\nallow_unpriced = true\n",
            server.port
        ),
    )
    .unwrap();
    let prefill = home.path().join("prefill.json");
    std::fs::write(&prefill, r#"[{"role": "user", "content": "FEW-SHOT-QUESTION"}, {"role": "assistant", "content": "FEW-SHOT-ANSWER"}]"#).unwrap();
    let data = dataset(home.path(), &[r#"{"id": "a", "prompt": "what is 2+2?"}"#]);
    let batch = ok(&run(
        home.path(),
        &[
            "eval",
            "batch",
            data.to_str().unwrap(),
            "--run-id",
            "eph",
            "--ephemeral-system-prompt",
            "Answer tersely.",
            "--prefill",
            prefill.to_str().unwrap(),
        ],
    ));
    let bodies = server.bodies();
    assert!(batch.contains("1 executed"), "{batch}");
    assert!(
        bodies.iter().any(|b| b.contains("Answer tersely.")),
        "sent with the call"
    );
    assert!(
        bodies.iter().any(|b| b.contains("FEW-SHOT-QUESTION")),
        "prefill sent"
    );
    let out = home.path().join("export.jsonl");
    ok(&run(
        home.path(),
        &[
            "trajectory",
            "export",
            "--format",
            "sharegpt",
            "--eval-run",
            "eph",
            "-o",
            out.to_str().unwrap(),
        ],
    ));
    let exported = std::fs::read_to_string(&out).unwrap();
    assert!(exported.contains("what is 2+2?"), "{exported}");
    assert!(!exported.contains("Answer tersely."), "{exported}");
    for file in common::files_under(&home.path().join("profiles/default")) {
        let bytes = std::fs::read(&file).unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains("Answer tersely."),
            "{} keeps the ephemeral prompt",
            file.display()
        );
    }
}

/// Scenario: Zero-filled tool stats; tasks with only failed tool calls are
/// filtered out of exports unless `--include-filtered`.
#[test]
fn statistics_zero_fill_tools_and_filtered_tasks_leave_exports() {
    let home = mock_home();
    script(
        home.path(),
        "[[rule]]\nuser = \"use the terminal\"\nstep = 0\n[[rule.tool_calls]]\nname = \"terminal\"\narguments = { command = \"exit 3\" }\n\n[[rule]]\nuser = \"read a missing file\"\nstep = 0\n[[rule.tool_calls]]\nname = \"read_file\"\narguments = { path = \"missing.txt\" }\n\n[[rule]]\nrepeat = true\ntext = \"done\"\n",
    );
    let data = dataset(
        home.path(),
        &[
            r#"{"id": "good", "prompt": "use the terminal"}"#,
            r#"{"id": "bad", "prompt": "read a missing file", "toolsets": ["file"]}"#,
        ],
    );
    ok(&run(
        home.path(),
        &[
            "eval",
            "batch",
            data.to_str().unwrap(),
            "--toolsets",
            "terminal,web",
            "--run-id",
            "st1",
        ],
    ));
    let stats = read_json(&runs(home.path()).join("st1/statistics.json"));
    let zero = serde_json::json!({"count": 0, "success": 0, "failure": 0});
    assert_eq!(stats["tools"]["web_search"], zero, "{stats}");
    assert_eq!(stats["tools"]["terminal"]["count"], 1, "{stats}");
    assert_eq!(stats["tasks"]["filtered"], 1, "{stats}");
    assert_eq!(
        stats["filtered"][0],
        serde_json::json!({"task_id": "bad", "reason": "only failed tool calls"})
    );
    assert!(stats["reasoning_fraction"].is_number() && stats["unpriced_calls"].is_number());
    let info = read_json(&runs(home.path()).join("st1/run.json"));
    assert_eq!(
        info["toolsets"]["good"],
        serde_json::json!(["terminal", "web"])
    );
    let out = home.path().join("x.jsonl");
    ok(&run(
        home.path(),
        &[
            "trajectory",
            "export",
            "--format",
            "sharegpt",
            "--eval-run",
            "st1",
            "--include-failed",
            "-o",
            out.to_str().unwrap(),
        ],
    ));
    assert_eq!(
        std::fs::read_to_string(&out).unwrap().lines().count(),
        1,
        "the filtered task is left out"
    );
    ok(&run(
        home.path(),
        &[
            "trajectory",
            "export",
            "--format",
            "sharegpt",
            "--eval-run",
            "st1",
            "--include-failed",
            "--include-filtered",
            "-o",
            out.to_str().unwrap(),
        ],
    ));
    assert_eq!(std::fs::read_to_string(&out).unwrap().lines().count(), 2);
}

/// An invalid line fails before any task runs; a task whose container
/// image is unavailable fails alone.
#[test]
fn invalid_lines_stop_the_batch_and_missing_images_fail_one_task() {
    let home = mock_home();
    script(home.path(), ANSWER);
    let data = dataset(
        home.path(),
        &[r#"{"prompt": "fine"}"#, r#"{"id": "x", "prompt": 3}"#],
    );
    let out = run(
        home.path(),
        &["eval", "batch", data.to_str().unwrap(), "--run-id", "inv"],
    );
    assert_eq!(out.status.code(), Some(65));
    assert!(
        common::stderr(&out).contains("line 2: 'prompt' must be a string"),
        "{}",
        common::stderr(&out)
    );
    assert!(!runs(home.path()).join("inv").exists(), "nothing ran");

    let data = dataset(
        home.path(),
        &[
            r#"{"id": "plain", "prompt": "fine"}"#,
            r#"{"id": "boxed", "prompt": "fine", "image": "pmt-test.invalid/none:0"}"#,
        ],
    );
    let out = run(
        home.path(),
        &["eval", "batch", data.to_str().unwrap(), "--run-id", "img"],
    );
    assert_eq!(out.status.code(), Some(1), "{}", stdout(&out));
    let boxed = read_json(&runs(home.path()).join("img/tasks/boxed/1/result.json"));
    assert_eq!(
        boxed["error"],
        "image not available: pmt-test.invalid/none:0"
    );
    assert_eq!(
        read_json(&runs(home.path()).join("img/tasks/plain/1/result.json"))["ok"],
        true
    );
}

/// Scenario: Reproducible sampling, end to end: the same seed gives every
/// task the same toolsets in two runs, recorded in run.json and each
/// result.json.
#[test]
fn a_seeded_distribution_samples_the_same_toolsets_in_every_run() {
    let home = mock_home();
    script(home.path(), ANSWER);
    let lines: Vec<String> = (1..=6)
        .map(|i| format!("{{\"id\": \"d{i}\", \"prompt\": \"p\"}}"))
        .collect();
    let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
    let data = dataset(home.path(), &refs);
    for id in ["s1", "s2"] {
        ok(&run(
            home.path(),
            &[
                "eval",
                "batch",
                data.to_str().unwrap(),
                "--toolset-distribution",
                "research",
                "--seed",
                "7",
                "--run-id",
                id,
            ],
        ));
    }
    let a = read_json(&runs(home.path()).join("s1/run.json"));
    let b = read_json(&runs(home.path()).join("s2/run.json"));
    assert_eq!(a["toolsets"], b["toolsets"]);
    assert_eq!(a["toolset_distribution"], "research");
    let result = read_json(&runs(home.path()).join("s2/tasks/d1/1/result.json"));
    assert_eq!(result["toolsets"], a["toolsets"]["d1"]);
    let listed = json(home.path(), &["eval", "distributions"]);
    assert_eq!(listed["distributions"].as_object().unwrap().len(), 17);
    assert_eq!(
        listed["distributions"]["browser_tasks"]["browser+web"],
        0.97
    );
    let unknown = home.path().join("d.toml");
    std::fs::write(&unknown, "[toolsets]\nteleport = 0.5\n").unwrap();
    let refused = run(
        home.path(),
        &[
            "eval",
            "batch",
            data.to_str().unwrap(),
            "--toolset-distribution",
            unknown.to_str().unwrap(),
        ],
    );
    assert_eq!(refused.status.code(), Some(65));
}

fn sharegpt(turns: usize, chars: usize) -> Value {
    let roles = ["human", "gpt", "tool", "gpt"];
    let mut conversations = vec![serde_json::json!({"from": "system", "value": "s".repeat(chars)})];
    for i in 1..turns {
        conversations.push(serde_json::json!({"from": roles[(i - 1) % 4], "value": format!("{i:03}{}", "x".repeat(chars - 3))}));
    }
    serde_json::json!({"conversations": conversations})
}

/// Scenario: Head and tail preserved; Scenario: Short trajectory untouched
/// (end to end: only the long record costs a summary call).
#[test]
fn trajectory_compress_summarises_only_long_records() {
    let home = mock_home();
    script(
        home.path(),
        "[[rule]]\nuser = \"Summarise in at most\"\nrepeat = true\ntext = \"the ledgers were summed\"\n",
    );
    let input = home.path().join("in.jsonl");
    let long = sharegpt(40, 3000);
    let short = sharegpt(8, 4000);
    std::fs::write(&input, format!("{long}\n{short}\n")).unwrap();
    let out = home.path().join("out.jsonl");
    ok(&run(
        home.path(),
        &[
            "trajectory",
            "compress",
            input.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
        ],
    ));
    let lines: Vec<Value> = std::fs::read_to_string(&out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let turns = lines[0]["conversations"].as_array().unwrap();
    assert_eq!(turns[..4], long["conversations"].as_array().unwrap()[..4]);
    assert_eq!(
        turns[turns.len() - 4..],
        long["conversations"].as_array().unwrap()[36..]
    );
    assert!(
        turns[4]["value"]
            .as_str()
            .unwrap()
            .contains("the ledgers were summed")
    );
    assert_eq!(lines[1], short, "copied unchanged");
    let stats = read_json(&home.path().join("compress-stats.json"));
    assert_eq!(
        (
            &stats["compressed"],
            &stats["unchanged"],
            &stats["summary_calls"]
        ),
        (&1.into(), &1.into(), &1.into()),
        "{stats}"
    );
    assert!(
        stats["tokens_after"].as_u64().unwrap() <= 15_250 + 8000,
        "{stats}"
    );
}

/// Regression: the hidden session's title came from the input path, so a
/// path over the store's 100-character title cap exited 65 and a second run
/// over the same file hit `store.title_in_use`.
#[test]
fn trajectory_compress_accepts_long_input_paths_and_reruns() {
    let home = mock_home();
    script(
        home.path(),
        "[[rule]]\nuser = \"Summarise in at most\"\nrepeat = true\ntext = \"the ledgers were summed\"\n",
    );
    let dir = home
        .path()
        .join("a-directory-name-long-enough-to-matter")
        .join("another-directory-name-long-enough-to-matter")
        .join("and-a-third-one-for-good-measure");
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("trajectories-with-a-long-file-name.jsonl");
    assert!(input.to_str().unwrap().chars().count() > 100);
    std::fs::write(&input, format!("{}\n", sharegpt(40, 3000))).unwrap();
    for _ in 0..2 {
        ok(&run(
            home.path(),
            &["trajectory", "compress", input.to_str().unwrap()],
        ));
    }
    let out = dir.join("trajectories-with-a-long-file-name.compressed.jsonl");
    let text = std::fs::read_to_string(&out).unwrap();
    assert!(text.contains("the ledgers were summed"), "{text}");
}
