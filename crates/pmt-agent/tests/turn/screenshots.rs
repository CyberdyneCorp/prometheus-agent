//! Screenshots a tool returns travel as pixels after their tool results,
//! within the screenshot context budget (spec: computer-use, Screenshot
//! context budget).

use std::fmt::Write as _;

use pmt_agent::turn::screenshots::RETIRED;
use pmt_providers::request::{Part, Request};
use pmt_store::Source;
use pmt_tools::{Content, Status, ToolResult};

use crate::support::{FakeTools, Profile, agent, mock_model, options};

/// A mock script calling `lookup` once per step for `steps` steps.
fn script(steps: usize) -> String {
    let mut text = String::new();
    for step in 0..steps {
        let _ = write!(
            text,
            "[[rule]]\nstep = {step}\n[[rule.tool_calls]]\nname = \"lookup\"\narguments = {{ q = \"{step}\" }}\n\n"
        );
    }
    let _ = write!(text, "[[rule]]\nstep = {steps}\ntext = \"done\"\n");
    text
}

/// A blank `side` x `side * 5 / 8` PNG in the profile's cache.
fn screenshot(profile: &Profile, side: u32) -> String {
    let path = profile.scope.dir().join(format!("shot-{side}.png"));
    image::RgbImage::new(side, side * 5 / 8)
        .save(&path)
        .unwrap();
    path.display().to_string()
}

fn capturing(path: String) -> std::sync::Arc<FakeTools> {
    FakeTools::answering(move |_| ToolResult {
        status: Status::Ok,
        content: vec![
            Content::Text {
                text: "captured".into(),
            },
            Content::Media {
                mime: "image/png".into(),
                path: path.clone(),
            },
        ],
        metadata: serde_json::Map::new(),
    })
}

fn images(request: &Request) -> usize {
    request
        .messages
        .iter()
        .flat_map(|m| &m.parts)
        .filter(|p| matches!(p, Part::Image { .. }))
        .count()
}

fn retired(request: &Request) -> usize {
    request
        .messages
        .iter()
        .flat_map(|m| &m.parts)
        .filter(|p| matches!(p, Part::Text { text } if text == RETIRED))
        .count()
}

/// Scenario: Below the limit.
#[tokio::test]
async fn thirty_screenshots_stay_and_the_prefix_never_changes() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, &script(30), "mock").await;
    let tools = capturing(screenshot(&profile, 1456));
    let run = agent(&profile, s, model.clone(), tools, |_| {});
    run.turn_with(
        &profile,
        pmt_agent::turn::TurnInput::text("look"),
        options(),
    )
    .await;
    let requests = model.requests();
    assert_eq!(requests.len(), 31);
    assert_eq!(images(&requests[30]), 30, "every screenshot is still sent");
    assert_eq!(retired(&requests[30]), 0);
    for pair in requests.windows(2) {
        let (before, after) = (&pair[0].messages, &pair[1].messages);
        assert_eq!(
            after[..before.len()],
            before[..],
            "an earlier message was rewritten"
        );
    }
}

#[tokio::test]
async fn past_the_limit_the_oldest_screenshots_retire_once_in_a_batch() {
    // A window large enough that compression never steps in.
    let profile = Profile::new("[models.\"mock/mock\"]\ncontext_window = 2000000\n");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, &script(104), "mock").await;
    let tools = capturing(screenshot(&profile, 320));
    let run = agent(&profile, s, model.clone(), tools, |_| {});
    run.turn_with(
        &profile,
        pmt_agent::turn::TurnInput::text("look"),
        options(),
    )
    .await;
    let requests = model.requests();
    assert_eq!(requests.len(), 105);
    for (n, request) in requests.iter().enumerate() {
        assert!(
            images(request) <= 100,
            "request {n} carries {} images",
            images(request)
        );
    }
    assert_eq!(retired(&requests[100]), 0, "100 small images fit");
    // The 101st screenshot retired the oldest 50 at once; they stay retired.
    assert_eq!(retired(&requests[101]), 50);
    assert_eq!(retired(&requests[104]), 50);
    assert_eq!(images(&requests[104]), 54);
    for pair in requests[101..].windows(2) {
        let (before, after) = (&pair[0].messages, &pair[1].messages);
        assert_eq!(
            after[..before.len()],
            before[..],
            "no rewrite between batches"
        );
    }
}
