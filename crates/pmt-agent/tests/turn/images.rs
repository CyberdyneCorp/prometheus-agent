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
