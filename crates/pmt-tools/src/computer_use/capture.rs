//! Captures (spec: computer-use, Capture modes and payload budget;
//! Screenshot retention and delivery).
//!
//! `som` is a screenshot with numbered element overlays plus the
//! elements, `vision` pixels only, `ax` the elements only. The longest
//! image edge is capped at `computer_use.max_image_dimension`; at most
//! [`MAX_ELEMENTS`] elements reach the model and the full list goes to a
//! session cache file named in the result. Every image is also kept as a
//! bounded copy under `<profile>/cache/computer-use/` (the newest
//! [`KEEP_SCREENSHOTS`]); that copy is what the result attaches and what a
//! `MEDIA:` line can deliver to a chat when the user asks for it.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use pmt_config::schema::CaptureMode;
use serde_json::{Value, json};

use super::driver::DriverReply;
use super::session::{SessionSetup, Target, call};
use crate::result::{Content, Status, ToolResult};

/// Elements surfaced to the model per capture.
pub const MAX_ELEMENTS: usize = 100;
/// Screenshots kept per profile.
pub const KEEP_SCREENSHOTS: usize = 20;
/// Element spill files kept per session.
const KEEP_SPILLS: usize = 20;
/// The longest element label shown to the model.
const MAX_LABEL_CHARS: usize = 120;

/// The screenshot directory of a profile.
#[must_use]
pub fn screenshot_dir(profile_dir: &Path) -> PathBuf {
    profile_dir.join("cache").join("computer-use")
}

/// The most recent screenshot of a profile, if any.
#[must_use]
pub fn latest_screenshot(profile_dir: &Path) -> Option<PathBuf> {
    newest_first(&screenshot_dir(profile_dir), "capture-")
        .into_iter()
        .next()
}

/// One accessibility element.
#[derive(Debug, Clone, PartialEq)]
pub struct Element {
    /// Its index (what `element` names).
    pub index: i64,
    /// Its role.
    pub role: String,
    /// Its label.
    pub label: String,
    /// `[x, y, w, h]`, when the driver reported real bounds.
    pub bounds: Option<[i64; 4]>,
    /// The driver's snapshot token, sent back with actions on it.
    pub token: Option<String>,
}

/// The frontmost on-screen window `app` / `window` select.
#[must_use]
pub fn select_window(
    reply: &DriverReply,
    app: Option<&str>,
    window: Option<&str>,
) -> Option<Target> {
    let mut windows: Vec<(i64, Target, String)> = reply
        .field("windows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|w| w["is_on_screen"].as_bool() != Some(false))
        .filter_map(|w| {
            let target = Target {
                pid: w["pid"].as_i64().filter(|p| *p > 0)?,
                window_id: w["window_id"].as_i64().filter(|id| *id > 0)?,
                app: w["app_name"].as_str().unwrap_or_default().to_owned(),
            };
            let title = w["title"].as_str().unwrap_or_default().to_owned();
            Some((w["z_index"].as_i64().unwrap_or(0), target, title))
        })
        .collect();
    windows.sort_by_key(|(z, ..)| std::cmp::Reverse(*z));
    if let Some(window) = window {
        let wanted = window.to_lowercase();
        windows.retain(|(_, t, title)| {
            t.window_id.to_string() == wanted || title.to_lowercase().contains(&wanted)
        });
    }
    let Some(app) = app else {
        return windows.into_iter().next().map(|(_, t, _)| t);
    };
    let wanted = app.to_lowercase();
    let exact = windows
        .iter()
        .find(|(_, t, _)| t.app.to_lowercase() == wanted);
    exact
        .or_else(|| {
            windows
                .iter()
                .find(|(_, t, _)| t.app.to_lowercase().contains(&wanted))
        })
        .map(|(_, t, _)| t.clone())
}

/// The elements of a `get_window_state` reply.
#[must_use]
pub fn elements(reply: &DriverReply) -> Vec<Element> {
    reply
        .field("elements")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|raw| {
            let frame = &raw["frame"];
            let bounds = ["x", "y", "w", "h"].map(|k| frame[k].as_i64().unwrap_or(0));
            Some(Element {
                index: raw["element_index"].as_i64()?,
                role: raw["role"].as_str().unwrap_or_default().to_owned(),
                label: raw["label"].as_str().unwrap_or_default().to_owned(),
                // A zero rect means "geometry unknown", never a position.
                bounds: (bounds != [0; 4]).then_some(bounds),
                token: raw["element_token"].as_str().map(str::to_owned),
            })
        })
        .collect()
}

/// The image of a reply: an image part, else base64 in the structured
/// content.
#[must_use]
pub fn image(reply: &DriverReply) -> Option<(String, Vec<u8>)> {
    if let Some(first) = reply.images.first() {
        return Some(first.clone());
    }
    let encoded = reply
        .field("screenshot_png_b64")
        .or_else(|| reply.field("png_b64"))?
        .as_str()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let mime = reply
        .field("screenshot_mime_type")
        .and_then(Value::as_str)
        .unwrap_or("image/png");
    Some((mime.to_owned(), bytes))
}

/// Takes one capture of `target` in `mode`.
pub(super) async fn take(
    driver: &std::sync::Arc<dyn super::driver::Driver>,
    setup: &SessionSetup,
    label: &str,
    target: &Target,
    mode: CaptureMode,
) -> Result<Shot, String> {
    let mut arguments = json!({"session": label, "pid": target.pid, "window_id": target.window_id});
    if setup.config.ax_max_elements > 0 && mode != CaptureMode::Vision {
        arguments["max_elements"] = json!(setup.config.ax_max_elements);
    }
    let pixels_only =
        mode == CaptureMode::Vision && driver.tools().iter().any(|t| t == "screenshot");
    let tool = if pixels_only {
        "screenshot"
    } else {
        "get_window_state"
    };
    let reply = call(driver, tool, arguments).await?;
    if reply.is_error {
        return Err(format!("{tool} failed: {}", reply.message()));
    }
    let found = if mode == CaptureMode::Vision {
        Vec::new()
    } else {
        elements(&reply)
    };
    let picture = if mode == CaptureMode::Ax {
        None
    } else {
        image(&reply)
    };
    Ok(Shot {
        mode,
        app: target.app.clone(),
        title: reply
            .field("window_title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        elements: found,
        image: picture,
    })
}

/// One capture's content.
#[derive(Debug)]
pub struct Shot {
    /// Its mode.
    pub mode: CaptureMode,
    /// The captured application.
    pub app: String,
    /// The window title.
    pub title: String,
    /// Its elements.
    pub elements: Vec<Element>,
    /// Its image: MIME type and bytes.
    pub image: Option<(String, Vec<u8>)>,
}

/// The tool result of a capture: a JSON summary plus, for an image
/// capture, the retained screenshot (attached when the model sees images).
pub(super) fn result(setup: &SessionSetup, shot: Shot) -> ToolResult {
    let total = shot.elements.len();
    let shown: Vec<Value> = shot
        .elements
        .iter()
        .take(MAX_ELEMENTS)
        .map(element_json)
        .collect();
    let mut body = json!({
        "mode": shot.mode.as_str(),
        "app": shot.app,
        "window_title": shot.title,
        "total_elements": total,
        "elements": shown,
    });
    if total > MAX_ELEMENTS {
        body["truncated_elements"] = json!(total - MAX_ELEMENTS);
        if let Some(path) = spill(setup, &shot.elements) {
            body["elements_file"] = json!(path.display().to_string());
        }
    }
    let mut media = None;
    if let Some((mime, bytes)) = shot.image {
        match keep(setup, &mime, &bytes) {
            Ok((path, mime, (width, height))) => {
                body["width"] = json!(width);
                body["height"] = json!(height);
                body["screenshot_path"] = json!(path.display().to_string());
                media = setup.image_input.then(|| Content::Media {
                    mime,
                    path: path.display().to_string(),
                });
            }
            Err(why) => body["screenshot_error"] = json!(why),
        }
    }
    let mut result = ToolResult::text(Status::Ok, body.to_string());
    result.content.extend(media);
    result
}

fn element_json(element: &Element) -> Value {
    let label: String = element.label.chars().take(MAX_LABEL_CHARS).collect();
    json!({
        "index": element.index,
        "role": element.role,
        "label": label,
        "bounds": element.bounds,
    })
}

/// Writes every element (full labels) to a session cache file.
fn spill(setup: &SessionSetup, elements: &[Element]) -> Option<PathBuf> {
    let dir = crate::spillover::session_dir(&setup.profile_dir, &setup.session_id);
    std::fs::create_dir_all(&dir).ok()?;
    prune(&dir, "computer-use-elements-", KEEP_SPILLS - 1);
    let path = dir.join(format!(
        "computer-use-elements-{}.json",
        ulid::Ulid::generate()
    ));
    let all: Vec<Value> = elements
        .iter()
        .map(|e| json!({"index": e.index, "role": e.role, "label": e.label, "bounds": e.bounds}))
        .collect();
    std::fs::write(&path, Value::Array(all).to_string()).ok()?;
    Some(path)
}

/// The capped image written as the profile's newest screenshot: its
/// path, MIME type and size.
fn keep(
    setup: &SessionSetup,
    mime: &str,
    bytes: &[u8],
) -> Result<(PathBuf, String, (u32, u32)), String> {
    let (mime, bytes, size) = capped(mime, bytes, setup.config.max_image_dimension)?;
    let dir = screenshot_dir(&setup.profile_dir);
    private_dir(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    prune(&dir, "capture-", KEEP_SCREENSHOTS - 1);
    let extension = if mime == "image/jpeg" { "jpg" } else { "png" };
    let path = dir.join(format!("capture-{}.{extension}", ulid::Ulid::generate()));
    private_write(&path, &bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    Ok((path, mime, size))
}

/// An encoded image: MIME type, bytes and size.
type Capped = (String, Vec<u8>, (u32, u32));

/// The image with its longest edge at most `max` (`0`: as it is).
fn capped(mime: &str, bytes: &[u8], max: u32) -> Result<Capped, String> {
    let decoded = image::load_from_memory(bytes).map_err(|e| format!("unreadable capture: {e}"))?;
    let (width, height) = (decoded.width(), decoded.height());
    if max == 0 || width.max(height) <= max {
        return Ok((mime.to_owned(), bytes.to_vec(), (width, height)));
    }
    let resized = decoded.resize(max, max, image::imageops::FilterType::Triangle);
    let mut out = Cursor::new(Vec::new());
    let (format, mime) = if mime == "image/jpeg" {
        (image::ImageFormat::Jpeg, "image/jpeg")
    } else {
        (image::ImageFormat::Png, "image/png")
    };
    resized
        .write_to(&mut out, format)
        .map_err(|e| format!("cannot encode the capture: {e}"))?;
    Ok((
        mime.to_owned(),
        out.into_inner(),
        (resized.width(), resized.height()),
    ))
}

/// Files of `dir` named `<prefix>...`, newest first (names carry a ULID,
/// so name order is creation order).
fn newest_first(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(prefix))
        })
        .collect();
    files.sort();
    files.reverse();
    files
}

/// Keeps the newest `keep` files of `dir` named `<prefix>...`.
fn prune(dir: &Path, prefix: &str, keep: usize) {
    for stale in newest_first(dir, prefix).into_iter().skip(keep) {
        let _ = std::fs::remove_file(stale);
    }
}

/// A directory only the owner can list (a capture shows whatever was on
/// the screen).
fn private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn private_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    std::io::Write::write_all(&mut options.open(path)?, bytes)
}
