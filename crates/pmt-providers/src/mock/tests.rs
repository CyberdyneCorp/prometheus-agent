//! Script validation, rule matching and fault classification.

use std::time::Duration;

use pmt_core::error::codes;

use super::script::{Answer, Fault, StreamFault, Uses};
use super::*;
use crate::classify::ErrorClass;

fn parse(text: &str) -> Script {
    Script::parse(text, "s.toml").unwrap()
}

fn invalid(text: &str) -> String {
    let err = Script::parse(text, "s.toml").unwrap_err();
    assert_eq!(err.code().code, codes::MOCK_SCRIPT_INVALID.code);
    err.message().to_owned()
}

#[test]
fn rules_parse_with_their_defaults() {
    let script = parse(
        r#"
latency_ms = 5
[[rule]]
user_regex = "^sum"
[[rule.tool_calls]]
name = "read_file"
arguments = { path = "README.md" }
[[rule]]
text = "done"
repeat = true
latency_ms = 0
"#,
    );
    let [first, second] = script.rules.as_slice() else {
        panic!("two rules")
    };
    assert_eq!(first.uses, Uses::Times(1));
    assert_eq!(first.latency, Duration::from_millis(5));
    let Answer::Respond(response) = &first.answer else {
        panic!("a response")
    };
    assert_eq!(response.tool_calls[0].arguments, r#"{"path":"README.md"}"#);
    assert_eq!(second.uses, Uses::Always);
    assert_eq!(second.latency, Duration::ZERO);
}

#[test]
fn invalid_rules_are_refused_with_their_position() {
    let cases = [
        ("[[rule]]\n", "rule 1: answers nothing"),
        (
            "[[rule]]\ntext = \"a\"\n[[rule]]\nbogus = 1\n",
            "unknown field `bogus`",
        ),
        (
            "[[rule]]\ntext = \"a\"\ntimes = 0\n",
            "rule 1: times must be at least 1",
        ),
        (
            "[[rule]]\ntext = \"a\"\ntimes = 2\nrepeat = true\n",
            "times and repeat are exclusive",
        ),
        (
            "[[rule]]\ntext = \"a\"\nuser = \"x\"\nuser_regex = \"x\"\n",
            "user and user_regex are exclusive",
        ),
        (
            "[[rule]]\ntext = \"a\"\ntool_result_regex = \"(\"\n",
            "rule 1: tool_result_regex:",
        ),
        (
            "[[rule]]\nfault = \"rate_limit\"\ntext = \"a\"\n",
            "takes no text",
        ),
        (
            "[[rule]]\nfault = \"timeout\"\nretry_after_secs = 1\n",
            "retry_after_secs applies only",
        ),
        (
            "[[rule]]\nfault = \"rate_limit\"\nstatus = 500\n",
            "status applies only",
        ),
        (
            "[[rule]]\nfault = \"server_error\"\nstatus = 404\n",
            "404 is not a server error",
        ),
        ("[[rule]]\nfault = \"melted\"\n", "unknown variant `melted`"),
        (
            "[[rule]]\n[[rule.tool_calls]]\nname = \" \"\n",
            "name must not be empty",
        ),
    ];
    for (text, expected) in cases {
        let message = invalid(text);
        assert!(message.starts_with("s.toml"), "{message}");
        assert!(message.contains(expected), "{text}: {message}");
    }
}

#[test]
fn stream_faults_follow_content_and_request_faults_stand_alone() {
    let script = parse(
        "[[rule]]\ntext = \"partial\"\nfault = \"truncated_stream\"\n[[rule]]\nfault = \"malformed_stream\"\n[[rule]]\nfault = \"server_error\"\n",
    );
    let answers: Vec<_> = script.rules.iter().map(|r| &r.answer).collect();
    assert!(
        matches!(answers[0], Answer::Respond(r) if r.broken == Some(StreamFault::Truncated) && r.text.as_deref() == Some("partial"))
    );
    assert!(matches!(answers[1], Answer::Respond(r) if r.broken == Some(StreamFault::Malformed)));
    assert!(matches!(
        answers[2],
        Answer::Fail(Fault::ServerError { status: 500 })
    ));
}

#[test]
fn a_missing_script_file_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let err = Script::load(&dir.path().join("nope.toml")).unwrap_err();
    assert_eq!(err.code().code, codes::MOCK_SCRIPT_MISSING.code);
    assert_eq!(err.exit_code(), 66);
}

/// Every request fault maps to the class the same provider failure gets.
#[test]
fn each_request_fault_maps_to_its_classified_error() {
    let cases = [
        (
            Fault::RateLimit {
                retry_after_secs: Some(7),
            },
            ErrorClass::RateLimit,
            Some(429),
        ),
        (Fault::Timeout, ErrorClass::Timeout, None),
        (
            Fault::ServerError { status: 500 },
            ErrorClass::ServerError,
            Some(500),
        ),
        (
            Fault::ServerError { status: 503 },
            ErrorClass::Overloaded,
            Some(503),
        ),
        (Fault::AuthError, ErrorClass::Auth, Some(401)),
        (
            Fault::ContextOverflow,
            ErrorClass::ContextOverflow,
            Some(400),
        ),
    ];
    for (fault, class, status) in cases {
        let error = fault::error(fault);
        assert_eq!(error.class, class, "{fault:?}");
        assert_eq!(error.status, status, "{fault:?}");
    }
    let limited = fault::error(Fault::RateLimit {
        retry_after_secs: Some(7),
    });
    assert_eq!(limited.retry_after, Some(Duration::from_secs(7)));
    assert!(limited.reset_at.is_some());
    assert_eq!(
        fault::error(Fault::RateLimit {
            retry_after_secs: None
        })
        .retry_after,
        None
    );
}

#[test]
fn mock_models_are_free_and_take_tools_images_and_reasoning() {
    let meta = metadata();
    assert!(meta.supports_tools && meta.supports_vision && meta.supports_reasoning);
    let tokens = pmt_store::Tokens {
        input: 1_000_000,
        output: 1_000_000,
        ..pmt_store::Tokens::default()
    };
    assert!(meta.price.unwrap().cost(&tokens).abs() < f64::EPSILON);
}

/// `tools` / `tools_regex` match the offered tool array as JSON.
#[test]
fn a_rule_can_match_the_offered_tools() {
    let script = parse(
        "[[rule]]\ntools = \"\\\"v4a\\\"\"\ntext = \"a\"\n[[rule]]\ntools_regex = \"^\\\\[\\\\]$\"\ntext = \"b\"\n",
    );
    let call = |tools: &str| Call {
        step: 0,
        model: "m",
        user: None,
        tool_results: Vec::new(),
        tools: tools.to_owned(),
    };
    let [v4a, none] = script.rules.as_slice() else {
        panic!("two rules")
    };
    assert!(call(r#"[{"name":"patch","enum":["replace","v4a"]}]"#).matches(v4a));
    assert!(!call(r#"[{"name":"patch"}]"#).matches(v4a));
    assert!(call("[]").matches(none) && !call("[{}]").matches(none));
    assert!(
        invalid("[[rule]]\ntools = \"a\"\ntools_regex = \"b\"\ntext = \"c\"\n")
            .contains("exclusive")
    );
}

/// `tool_result` sees every result of the latest tool-call batch: a list
/// requires each text somewhere in the batch, so a rule waiting on a
/// parallel batch misses when one of its calls fails or is lost; earlier
/// batches no longer count.
#[test]
fn a_rule_can_require_every_result_of_the_latest_batch() {
    let script =
        parse("[[rule]]\ntool_result = [\"Total: 85.15\", \"Owner: finance\"]\ntext = \"both\"\n");
    let [both] = script.rules.as_slice() else {
        panic!("one rule")
    };
    let result = |content: &str| Message {
        role: Role::Tool,
        parts: vec![Part::ToolResult {
            call_id: "c".into(),
            content: content.into(),
            is_error: false,
        }],
        cache_breakpoint: false,
    };
    let assistant = Message::text(Role::Assistant, "");
    let holds = |messages: &[Message]| {
        Call {
            step: 1,
            model: "m",
            user: None,
            tool_results: latest_batch(messages),
            tools: String::new(),
        }
        .matches(both)
    };
    let batch = [
        assistant.clone(),
        result("Total: 85.15"),
        result("Owner: finance"),
    ];
    assert!(holds(&batch));
    let lost_first = [
        assistant.clone(),
        result("error: not found"),
        result("Owner: finance"),
    ];
    assert!(!holds(&lost_first));
    let lost_last = [
        assistant.clone(),
        result("Total: 85.15"),
        result("error: not found"),
    ];
    assert!(!holds(&lost_last));
    let earlier = [
        assistant.clone(),
        result("Total: 85.15"),
        assistant,
        result("Owner: finance"),
    ];
    assert!(!holds(&earlier));
    assert!(
        invalid("[[rule]]\ntool_result = [\"a\"]\ntool_result_regex = \"b\"\ntext = \"c\"\n")
            .contains("exclusive")
    );
}
