//! `vision_analyze` in real sessions on the mock provider (spec:
//! web-and-media-tools, Image embed budget, per-image cap and replay): the
//! session's `vision.max_calls_per_image` and `vision.embed_target_bytes`
//! apply to the main agent and to delegated children.

mod common;

use std::io::Cursor;
use std::path::Path;
use std::process::Output;

use common::{append_config, command, mock_home, stderr, stdout};
use serde_json::Value;

/// The mock route carries images inside tool results, so `vision_analyze`
/// embeds the image itself (the only path the cap counts).
const NATIVE_ROUTE: &str = "\n[providers.mock]\nwire_format = \"anthropic_messages\"\n";

fn oneshot(home: &Path, prompt: &str, script: &str) -> Output {
    let path = home.join("script.toml");
    std::fs::write(&path, script).unwrap();
    command(home)
        .current_dir(home)
        .args(["--json", "-z", prompt, "--mock-script"])
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn answer(out: &Output) -> String {
    let body: Value = serde_json::from_str(stdout(out).trim())
        .unwrap_or_else(|e| panic!("{e}: stdout {} stderr {}", stdout(out), stderr(out)));
    body["answer"].as_str().unwrap_or_default().to_owned()
}

/// A noisy PNG that compresses badly (far above 64 KiB).
fn noisy_png(path: &Path, width: u32, height: u32) {
    let mut seed = 7u32;
    let pixels = image::RgbImage::from_fn(width, height, |_, _| {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        let [a, b, c, _] = seed.to_le_bytes();
        image::Rgb([a, b, c])
    });
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(pixels)
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    std::fs::write(path, out.into_inner()).unwrap();
}

/// Scenario: Cap inside a subagent (end to end): a delegated child with
/// `vision.max_calls_per_image` unset is refused on its fourth load of
/// the same file.
#[test]
fn a_delegated_child_is_refused_on_its_fourth_load_of_one_image() {
    let home = mock_home();
    append_config(home.path(), NATIVE_ROUTE);
    std::fs::write(home.path().join("shot.png"), common::PNG).unwrap();
    let script = r#"
[[rule]]
user = "please delegate"
[[rule.tool_calls]]
name = "delegate_task"
arguments = { tasks = [{ goal = "Inspect shot.png", toolsets = ["vision", "file"] }] }

[[rule]]
user = "Inspect shot.png"
tool_result_regex = "refused: this image has already been loaded into context 3 time\\(s\\)"
text = "child stopped at the cap"

[[rule]]
user = "Inspect shot.png"
tool_result = "\"mime\":\"image/png\""
repeat = true
[[rule.tool_calls]]
name = "vision_analyze"
arguments = { image_url = "shot.png", question = "Again?" }

[[rule]]
user = "Inspect shot.png"
[[rule.tool_calls]]
name = "vision_analyze"
arguments = { image_url = "shot.png", question = "What is it?" }

[[rule]]
tool_result = "child stopped at the cap"
text = "parent saw the cap"

[[rule]]
tool_result_regex = "(?s).*"
repeat = true
text = "parent saw something else"
"#;
    let out = oneshot(home.path(), "please delegate", script);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(answer(&out), "parent saw the cap");
}

/// The main agent's configured cap and embed target: an oversized image
/// comes back as a JPEG within the target, and a second load is refused.
#[test]
fn the_main_agent_follows_the_configured_cap_and_embed_target() {
    let home = mock_home();
    append_config(
        home.path(),
        &format!("{NATIVE_ROUTE}\n[vision]\nmax_calls_per_image = 1\nembed_target_bytes = 65536\n"),
    );
    noisy_png(&home.path().join("big.png"), 2_000, 600);
    let script = r#"
[[rule]]
user = "look twice"
tool_result_regex = "refused: this image has already been loaded into context 1 time\\(s\\)"
text = "refused after one"

[[rule]]
user = "look twice"
tool_result = "\"mime\":\"image/jpeg\""
[[rule.tool_calls]]
name = "vision_analyze"
arguments = { image_url = "big.png", question = "Again?" }

[[rule]]
user = "look twice"
[[rule.tool_calls]]
name = "vision_analyze"
arguments = { image_url = "big.png", question = "What is it?" }

[[rule]]
tool_result_regex = "(?s).*"
repeat = true
text = "unexpected tool result"
"#;
    let out = oneshot(home.path(), "look twice", script);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(answer(&out), "refused after one");
    let media = home.path().join("profiles/default/cache/media");
    let kept: Vec<_> = std::fs::read_dir(&media)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "jpeg"))
        .collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
    let bytes = std::fs::read(&kept[0]).unwrap();
    assert!(bytes.len() <= 65_536, "{} bytes", bytes.len());
    let fitted = image::load_from_memory(&bytes).unwrap();
    assert!(fitted.width() <= 1_568, "{}", fitted.width());
}

/// An image attached to the current user message is already in context:
/// `vision_analyze` without a region does not load it again.
#[test]
fn an_attached_image_is_not_loaded_again() {
    let home = mock_home();
    append_config(home.path(), NATIVE_ROUTE);
    let shot = home.path().join("shot.png");
    std::fs::write(&shot, common::PNG).unwrap();
    let script = r#"
[[rule]]
user = "what is this"
tool_result = "already in context"
text = "it was already there"

[[rule]]
user = "what is this"
[[rule.tool_calls]]
name = "vision_analyze"
arguments = { image_url = "shot.png", question = "What is it?" }

[[rule]]
tool_result_regex = "(?s).*"
repeat = true
text = "loaded it again"
"#;
    let path = home.path().join("script.toml");
    std::fs::write(&path, script).unwrap();
    let out = command(home.path())
        .current_dir(home.path())
        .args(["--json", "-z", "what is this", "--image"])
        .arg(&shot)
        .arg("--mock-script")
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(answer(&out), "it was already there");
}
