//! Images attached to a user message travel as pixels on every request
//! that replays the message (spec: cli, Image attachments; agent-turn-loop,
//! Image input routing).

use pmt_agent::turn::TurnInput;
use pmt_agent::turn::transcript::ImageRef;
use pmt_providers::request::Part;
use pmt_store::Source;

use crate::support::{FakeTools, Profile, agent, mock_model, options};

/// A 1x1 PNG.
const PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 218, 99, 100, 96, 248, 95, 15, 0, 2,
    135, 1, 128, 235, 71, 186, 146, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
];

fn images(parts: &[Part]) -> usize {
    parts
        .iter()
        .filter(|p| matches!(p, Part::Image { media_type, .. } if media_type == "image/png"))
        .count()
}

#[tokio::test]
async fn an_attached_image_is_sent_as_pixels_and_replayed() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, "[[rule]]\ntext = \"ok\"\nrepeat = true\n", "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    let blob = profile.store.put_blob(PNG).unwrap();
    let input = TurnInput {
        images: vec![ImageRef {
            blob: blob.to_string(),
            media_type: "image/png".into(),
            name: "shot.png".into(),
            description: None,
        }],
        ..TurnInput::text("describe")
    };
    run.turn_with(&profile, input, options()).await;
    run.turn(&profile, "again").await;

    let requests = model.requests();
    assert_eq!(
        images(&requests[0].messages[0].parts),
        1,
        "{:?}",
        requests[0].messages[0].parts
    );
    assert_eq!(requests[1].messages[0], requests[0].messages[0]);
    assert_eq!(images(&requests[1].messages[2].parts), 0);
}

fn attach(profile: &Profile, name: &str) -> ImageRef {
    ImageRef {
        blob: profile.store.put_blob(PNG).unwrap().to_string(),
        media_type: "image/png".into(),
        name: name.into(),
        description: None,
    }
}

/// Scenario: Resume replays recent images (agent-turn-loop) / Resume
/// replays images (web-and-media-tools).
#[tokio::test]
async fn a_resumed_session_replays_its_photo_byte_identically() {
    let profile = Profile::new("");
    let s = profile.session(Source::Cli);
    let answer = "[[rule]]\ntext = \"ok\"\nrepeat = true\n";
    let first = mock_model(&profile, answer, "mock").await;
    let run = agent(&profile, s, first.clone(), FakeTools::new(), |_| {});
    let input = TurnInput {
        images: vec![attach(&profile, "a.png"), attach(&profile, "b.png")],
        ..TurnInput::text("two screenshots")
    };
    run.turn_with(&profile, input, options()).await;
    drop(run);

    // A new process: a new agent over the stored session.
    let resumed = mock_model(&profile, answer, "mock").await;
    let run = agent(&profile, s, resumed.clone(), FakeTools::new(), |_| {});
    run.turn(&profile, "what was in the photos?").await;
    let sent = &first.requests()[0].messages[0];
    let replayed = &resumed.requests()[0].messages[0];
    assert_eq!(images(&replayed.parts), 2);
    assert_eq!(replayed, sent, "byte-identical to the first send");
}

#[tokio::test]
async fn older_images_retire_in_batches_and_the_current_message_keeps_its_own() {
    let profile = Profile::new("[vision]\nreplay_recent_images = 1\n");
    let s = profile.session(Source::Cli);
    let model = mock_model(&profile, "[[rule]]\ntext = \"ok\"\nrepeat = true\n", "mock").await;
    let run = agent(&profile, s, model.clone(), FakeTools::new(), |_| {});
    for name in ["1.png", "2.png", "3.png"] {
        let input = TurnInput {
            images: vec![attach(&profile, name)],
            ..TurnInput::text(name)
        };
        run.turn_with(&profile, input, options()).await;
    }
    let requests = model.requests();
    let users = |i: usize| -> Vec<usize> {
        requests[i]
            .messages
            .iter()
            .filter(|m| m.role == pmt_providers::request::Role::User)
            .map(|m| images(&m.parts))
            .collect()
    };
    assert_eq!(users(0), [1]);
    assert_eq!(users(1), [1, 1], "one older image stays");
    assert_eq!(users(2), [0, 1, 1], "the oldest retired");
    let retired = &requests[2].messages[0].parts;
    assert!(retired.contains(&Part::text("[image]")), "{retired:?}");
}
