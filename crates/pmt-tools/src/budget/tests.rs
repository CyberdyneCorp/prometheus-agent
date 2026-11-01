use std::fs;
use std::path::Path;

use pmt_config::schema::ResultBudget as BudgetConfig;
use pmt_core::secret::{Redactor, SecretString};

use super::{ResultBudget, SPILL_KEY, TurnResult};
use crate::descriptor::Owner;
use crate::result::ToolResult;
use crate::spillover::session_dir;
use crate::testing::{builtin, tool};

fn budget(dir: &Path, window: Option<u64>) -> ResultBudget {
    ResultBudget::new(
        BudgetConfig::default(),
        window,
        dir.to_owned(),
        Redactor::default(),
    )
}

#[test]
fn oversized_result_is_replaced_by_a_spill_reference() {
    let profile = tempfile::tempdir().unwrap();
    let dir = session_dir(profile.path(), "ses_1");
    let budget = budget(&dir, None);
    let big = "x".repeat(100_001);
    let mut result = ToolResult::ok(big.clone());
    budget.apply("call_1", Some(&builtin("web_extract", "web")), &mut result);

    let path = dir.join("call_1.txt");
    let text = result.text_content();
    assert!(text.starts_with("<persisted-output>\nOutput too large (100001 characters)."));
    assert!(text.contains(&format!("Full output saved to: {}", path.display())));
    assert!(text.contains(&format!(
        "Preview (first 1500 characters):\n{}\n...",
        "x".repeat(1500)
    )));
    assert!(text.ends_with("</persisted-output>"));
    assert!(text.chars().count() < 2_000);
    assert_eq!(fs::read_to_string(&path).unwrap(), big);
    assert_eq!(result.metadata[SPILL_KEY], path.display().to_string());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn results_within_the_threshold_are_untouched() {
    let profile = tempfile::tempdir().unwrap();
    let budget = budget(&session_dir(profile.path(), "s"), None);
    let mut result = ToolResult::ok("x".repeat(100_000));
    budget.apply("c", Some(&builtin("web_extract", "web")), &mut result);
    assert_eq!(result.text_chars(), 100_000);
    assert!(!session_dir(profile.path(), "s").exists());
}

#[test]
fn small_context_model_spills_at_fifteen_percent() {
    let profile = tempfile::tempdir().unwrap();
    let budget = budget(&session_dir(profile.path(), "s"), Some(32_000));
    assert_eq!(
        budget.threshold(Some(&builtin("terminal", "terminal"))),
        Some(19_200)
    );
    assert_eq!(budget.per_turn(), 38_400);
    let mut result = ToolResult::ok("y".repeat(40_000));
    budget.apply("c", Some(&builtin("terminal", "terminal")), &mut result);
    assert!(result.text_content().starts_with("<persisted-output>"));
    // Tiny windows are floored.
    let tiny = super::ResultBudget::new(
        BudgetConfig::default(),
        Some(1_000),
        "d".into(),
        Redactor::default(),
    );
    assert_eq!(tiny.threshold(None), Some(8_000));
    assert_eq!(tiny.per_turn(), 16_000);
}

#[test]
fn threshold_resolution_order() {
    let mut config = BudgetConfig::default();
    config.overrides.insert("search_files".into(), 5);
    config.overrides.insert("read_file".into(), 5);
    let budget = ResultBudget::new(config, None, "d".into(), Redactor::default());
    assert_eq!(budget.threshold(Some(&builtin("read_file", "file"))), None);
    assert_eq!(
        budget.threshold(Some(&builtin("search_files", "file"))),
        Some(5)
    );
    let mcp = tool("mcp__gh__x", "web", Owner::Mcp("gh".into()));
    assert_eq!(budget.threshold(Some(&mcp)), Some(50_000));
    let mut capped = builtin("web_extract", "web");
    capped.descriptor.result_cap = Some(70_000);
    assert_eq!(budget.threshold(Some(&capped)), Some(70_000));
    assert_eq!(budget.threshold(None), Some(100_000));
    // A plugin's read_file is not the self-paging built-in.
    let plugin = tool("read_file", "file", Owner::Plugin("p".into()));
    assert_eq!(budget.threshold(Some(&plugin)), Some(5));
}

#[test]
fn read_file_is_never_spilled() {
    let profile = tempfile::tempdir().unwrap();
    let budget = budget(&session_dir(profile.path(), "s"), Some(8_000));
    let mut result = ToolResult::ok("z".repeat(300_000));
    budget.apply("c", Some(&builtin("read_file", "file")), &mut result);
    assert_eq!(result.text_chars(), 300_000);
}

#[test]
fn spill_write_failure_truncates_inline() {
    let profile = tempfile::tempdir().unwrap();
    // A file where the spill directory should be: the directory cannot be made.
    let blocked = profile.path().join("blocked");
    fs::write(&blocked, "").unwrap();
    let budget = budget(&blocked.join("ses_1"), Some(32_000));
    let mut result = ToolResult::ok("w".repeat(40_000));
    budget.apply("c", Some(&builtin("terminal", "terminal")), &mut result);
    let text = result.text_content();
    assert!(text.starts_with(&"w".repeat(19_200)));
    assert!(
        text.ends_with(
            "\n[Truncated: tool response was 40000 chars; full output could not be saved]"
        )
    );
    assert_eq!(text.chars().count(), 19_200 + 1 + 74);
}

#[test]
fn largest_results_are_spilled_first_to_fit_the_turn() {
    let profile = tempfile::tempdir().unwrap();
    let budget = budget(&session_dir(profile.path(), "s"), None);
    let web = builtin("web_extract", "web");
    let read = builtin("read_file", "file");
    let mut results = [
        ToolResult::ok("a".repeat(90_000)),
        ToolResult::ok("b".repeat(95_000)),
        ToolResult::ok("c".repeat(30_000)),
        ToolResult::ok("d".repeat(99_000)),
    ];
    let [a, b, c, d] = &mut results;
    let mut turn = [
        TurnResult {
            call_id: "a",
            tool: Some(&web),
            result: a,
        },
        TurnResult {
            call_id: "b",
            tool: Some(&web),
            result: b,
        },
        TurnResult {
            call_id: "c",
            tool: Some(&web),
            result: c,
        },
        TurnResult {
            call_id: "d",
            tool: Some(&read),
            result: d,
        },
    ];
    budget.apply_turn(&mut turn);
    let spilled: Vec<bool> = results
        .iter()
        .map(|r| r.text_content().starts_with("<persisted-output>"))
        .collect();
    // 314,000 chars: read_file cannot spill, so b (95k) then a (90k) go.
    assert_eq!(spilled, [true, true, false, false]);
}

#[test]
fn spilled_text_is_redacted() {
    let profile = tempfile::tempdir().unwrap();
    let dir = session_dir(profile.path(), "s");
    let secret = SecretString::new("sk-live-0123456789abcdef");
    let budget = ResultBudget::new(
        BudgetConfig::default(),
        None,
        dir.clone(),
        Redactor::new([&secret]),
    );
    let mut result = ToolResult::ok(format!("{}{}", secret.expose(), "q".repeat(100_000)));
    budget.apply("call/../x", None, &mut result);
    let written = fs::read_to_string(dir.join("call____x.txt")).unwrap();
    assert!(!written.contains("sk-live-0123456789abcdef"));
    assert!(written.starts_with("[REDACTED]"));
}

#[test]
fn starting_a_session_prunes_expired_spillover() {
    let profile = tempfile::tempdir().unwrap();
    let old = session_dir(profile.path(), "ses_old");
    fs::create_dir_all(&old).unwrap();
    let file = old.join("call_1.txt");
    fs::write(&file, "x").unwrap();
    let mut config = pmt_config::schema::Config::default();
    config.sessions.retention_secs = 1;
    let past = std::time::SystemTime::now() - std::time::Duration::from_secs(5);
    fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(past)
        .unwrap();
    let budget = ResultBudget::for_session(
        profile.path(),
        "ses_new",
        &config,
        None,
        Redactor::default(),
    );
    assert!(!file.exists() && !old.exists());
    assert_eq!(budget.threshold(None), Some(100_000));
}
