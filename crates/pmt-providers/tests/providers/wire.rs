//! Recorded stream fixtures per wire format (spec: Wire formats,
//! selection and normalised event stream).

use pmt_providers::StreamEvent;
use pmt_providers::classify::ErrorClass;
use pmt_providers::event::StopReason;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer};

use crate::support::{Env, caller, request, sse};

fn config(uri: &str) -> String {
    format!(
        "[providers.ant]\nbase_url = \"{uri}/ant\"\nwire_format = \"anthropic_messages\"\nkey_secret = \"ANT_KEY\"\n\
         [providers.chat]\nbase_url = \"{uri}/chat/v1\"\nkey_secret = \"CHAT_KEY\"\n\
         [providers.resp]\nbase_url = \"{uri}/resp/v1\"\nwire_format = \"responses\"\nkey_secret = \"RESP_KEY\"\n\
         [providers.relay]\nbase_url = \"{uri}/relay/anthropic\"\nkey_secret = \"RELAY_KEY\"\n"
    )
}

const KEYS: &[(&str, &str)] = &[
    ("ANT_KEY", "ant-key-0123456789"),
    ("CHAT_KEY", "chat-key-0123456789"),
    ("RESP_KEY", "resp-key-0123456789"),
    ("RELAY_KEY", "relay-key-0123456789"),
];

fn tool_events(events: &[StreamEvent]) -> Vec<StreamEvent> {
    events
        .iter()
        .filter(|e| {
            matches!(
                e,
                StreamEvent::ToolCallStart { .. }
                    | StreamEvent::ToolCallEnd { .. }
                    | StreamEvent::Stop { .. }
            )
        })
        .cloned()
        .collect()
}

#[tokio::test]
async fn same_tool_call_across_wire_formats() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/ant/v1/messages"))
        .and(header("x-api-key", "ant-key-0123456789"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(sse("anthropic_tool_call.sse"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/v1/chat/completions"))
        .and(header("authorization", "Bearer chat-key-0123456789"))
        .respond_with(sse("chat_tool_call.sse"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/resp/v1/responses"))
        .and(header("authorization", "Bearer resp-key-0123456789"))
        .respond_with(sse("responses_tool_call.sse"))
        .mount(&server)
        .await;
    let env = Env::new(&config(&server.uri()), KEYS);
    let mut normalised = Vec::new();
    for provider in ["ant", "chat", "resp"] {
        let route = env.route(provider, "m").await;
        let mut stream = caller()
            .send(&route, &request("read main"), None)
            .await
            .unwrap();
        let events = stream.collect().await;
        // Exactly one stop, last, after exactly one usage.
        assert!(
            matches!(events.last(), Some(StreamEvent::Stop { .. })),
            "{provider}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, StreamEvent::Stop { .. }))
                .count(),
            1
        );
        assert!(matches!(
            events[events.len() - 2],
            StreamEvent::Usage { .. }
        ));
        assert_eq!(
            events[0],
            StreamEvent::TextDelta {
                text: "Reading the file.".into()
            }
        );
        normalised.push(tool_events(&events));
    }
    let expected = vec![
        StreamEvent::ToolCallStart {
            index: 0,
            id: "toolu_01".into(),
            name: "read_file".into(),
        },
        StreamEvent::ToolCallEnd {
            index: 0,
            id: "toolu_01".into(),
            name: "read_file".into(),
            arguments: "{\"limit\":50,\"path\":\"src/main.rs\"}".into(),
        },
        StreamEvent::Stop {
            stop_reason: StopReason::ToolUse,
        },
    ];
    for events in normalised {
        assert_eq!(events, expected);
    }
}

#[tokio::test]
async fn usage_is_normalised_per_format_with_cache_tokens() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/ant/v1/messages"))
        .respond_with(sse("anthropic_tool_call.sse"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/chat/v1/chat/completions"))
        .respond_with(sse("chat_tool_call.sse"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/resp/v1/responses"))
        .respond_with(sse("responses_tool_call.sse"))
        .mount(&server)
        .await;
    let env = Env::new(&config(&server.uri()), KEYS);
    let mut usages = Vec::new();
    for provider in ["ant", "chat", "resp"] {
        let route = env.route(provider, "m").await;
        let mut stream = caller().send(&route, &request("x"), None).await.unwrap();
        stream.collect().await;
        usages.push(stream.usage().unwrap());
    }
    let summary: Vec<(u64, u64, u64, u64, u64)> = usages
        .iter()
        .map(|u| {
            (
                u.input_tokens,
                u.output_tokens,
                u.cache_read_tokens,
                u.cache_write_tokens,
                u.reasoning_tokens,
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (120, 42, 4096, 2048, 0),
            (120, 42, 4096, 0, 0),
            (120, 42, 4096, 0, 10)
        ]
    );
}

#[tokio::test]
async fn text_and_reasoning_stream_without_usage_reports_zeros() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse("chat_text.sse"))
        .mount(&server)
        .await;
    let env = Env::new(&config(&server.uri()), KEYS);
    let route = env.route("chat", "m").await;
    let events = caller()
        .send(&route, &request("hi"), None)
        .await
        .unwrap()
        .collect()
        .await;
    assert_eq!(
        events,
        vec![
            StreamEvent::ReasoningDelta {
                text: "Short answer.".into()
            },
            StreamEvent::TextDelta {
                text: "Hello".into()
            },
            StreamEvent::TextDelta {
                text: " there.".into()
            },
            StreamEvent::Usage {
                usage: pmt_providers::Usage::default()
            },
            StreamEvent::Stop {
                stop_reason: StopReason::EndTurn
            },
        ]
    );
}

#[tokio::test]
async fn host_rule_selects_anthropic_messages_for_an_anthropic_path() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/relay/anthropic/v1/messages"))
        .and(header("x-api-key", "relay-key-0123456789"))
        .respond_with(sse("anthropic_tool_call.sse"))
        .mount(&server)
        .await;
    let env = Env::new(&config(&server.uri()), KEYS);
    let route = env.route("relay", "m").await;
    assert_eq!(route.wire_format.as_str(), "anthropic_messages");
    let events = caller()
        .send(&route, &request("x"), None)
        .await
        .unwrap()
        .collect()
        .await;
    assert!(events.contains(&StreamEvent::Stop {
        stop_reason: StopReason::ToolUse
    }));
}

#[tokio::test]
async fn an_in_stream_error_before_any_event_is_a_classified_failure() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(sse("anthropic_overloaded.sse"))
        .mount(&server)
        .await;
    let env = Env::new(&config(&server.uri()), KEYS);
    let mut route = env.route("ant", "m").await;
    route.retry.max_retries = 0;
    let err = caller()
        .send(&route, &request("x"), None)
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Overloaded);
}

#[test]
fn unknown_wire_format_value_lists_the_accepted_values() {
    let err = pmt_config::check(
        std::path::Path::new("config.toml"),
        "[providers.x]\nbase_url = \"https://x.test/v1\"\nwire_format = \"chat\"\n",
    )
    .unwrap_err();
    assert_eq!(err.exit_code(), 78);
    for value in ["chat_completions", "responses", "anthropic_messages"] {
        assert!(err.message().contains(value), "{}", err.message());
    }
}
