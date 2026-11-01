//! Image attachments of one-shot runs (spec: cli, Image attachments).

mod common;

use common::{PNG, append_config, mock_home, newest_session, ok, run, session_messages, stderr};

fn image(home: &std::path::Path) -> String {
    let path = home.join("shot.png");
    std::fs::write(&path, PNG).unwrap();
    path.to_string_lossy().into_owned()
}

fn first_user(home: &std::path::Path) -> serde_json::Value {
    session_messages(home, &newest_session(home))
        .into_iter()
        .find(|m| m["role"] == "user")
        .expect("a user message")
}

/// Scenario: Non-vision model.
#[test]
fn a_model_without_vision_and_no_vision_route_refuses_before_any_call() {
    let home = mock_home();
    append_config(
        home.path(),
        "\n[models.\"mock/mock\"]\nsupports_vision = false\n",
    );
    let shot = image(home.path());
    let out = run(home.path(), &["-z", "describe", "--image", &shot]);
    assert_eq!(out.status.code(), Some(78), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("model mock does not accept images"),
        "{}",
        stderr(&out)
    );
    let usage = ok(&run(home.path(), &["--json", "usage", "--by", "session"]));
    assert!(!usage.contains("\"calls\":1"), "{usage}");
    assert!(session_messages(home.path(), &newest_session(home.path())).is_empty());
}

#[test]
fn a_vision_model_gets_the_pixels_from_the_blob_store() {
    let home = mock_home();
    let shot = image(home.path());
    let out = ok(&run(home.path(), &["-z", "describe", "--image", &shot]));
    assert_eq!(out.trim(), "mock: describe");
    let user = first_user(home.path());
    let images = &user["metadata"]["images"];
    assert_eq!(images[0]["name"], "shot.png");
    assert_eq!(images[0]["media_type"], "image/png");
    assert!(images[0].get("description").is_none(), "{images}");
    let blob = images[0]["blob"].as_str().unwrap();
    let sha = blob.strip_prefix("blob:sha256:").unwrap();
    let stored = common::files_under(&home.path().join("profiles/default"))
        .into_iter()
        .any(|p| p.to_string_lossy().contains(sha));
    assert!(stored, "blob {sha} not in the profile's blob store");
}

/// The non-vision model gets the `vision` route's description instead.
#[test]
fn a_model_without_vision_gets_the_vision_route_description() {
    let home = mock_home();
    append_config(
        home.path(),
        "\n[models.\"mock/mock\"]\nsupports_vision = false\n\n[auxiliary.vision]\nprovider = \"mock\"\n",
    );
    let shot = image(home.path());
    ok(&run(home.path(), &["-z", "what is it", "--image", &shot]));
    let user = first_user(home.path());
    let description = user["metadata"]["images"][0]["description"]
        .as_str()
        .unwrap();
    assert!(
        description.starts_with("mock: Describe this image"),
        "{description}"
    );
}

#[test]
fn files_that_are_not_accepted_images_are_refused() {
    let home = mock_home();
    let text = home.path().join("notes.txt");
    std::fs::write(&text, "hello").unwrap();
    for bad in [
        text.to_string_lossy().into_owned(),
        "missing.png".to_owned(),
    ] {
        let out = run(home.path(), &["-z", "x", "--image", &bad]);
        assert_eq!(out.status.code(), Some(65), "{}", stderr(&out));
        assert!(stderr(&out).contains("cannot attach"), "{}", stderr(&out));
    }
}
