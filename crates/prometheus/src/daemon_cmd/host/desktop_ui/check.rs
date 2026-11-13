//! What the daemon refuses before a window tool reaches the window, and
//! how long it waits for the window (spec: desktop-app, Desktop UI
//! toolset contract; Annotating the preview page; Pane focus, tours and
//! tips; Appearance tool).

use std::time::Duration;

use serde_json::Value;

/// The panes `focus_pane` reveals.
pub(super) const PANES: &[&str] = &["chat", "files", "terminal", "review", "sessions"];
/// The named accents of `set_theme` (besides `#rrggbb`).
pub(super) const ACCENTS: &[&str] = &[
    "mono", "green", "cyan", "violet", "pink", "red", "orange", "theme",
];
/// The modes of `set_theme`.
pub(super) const MODES: &[&str] = &["light", "dark", "system"];

/// How long an ordinary window request waits.
const WAIT: Duration = Duration::from_secs(30);
/// How long a picker, tour or onboarding card waits for the user.
const WAIT_FOR_USER: Duration = Duration::from_mins(5);

fn text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn has_target(args: &Value) -> bool {
    text(args, "ref").is_some() || text(args, "selector").is_some()
}

/// Why `tool` refuses `args` before asking the window, if it does.
///
/// # Errors
///
/// The message the model gets.
pub(super) fn refusal(tool: &str, args: &Value) -> Result<(), String> {
    match tool {
        "focus_pane" => focus_pane(args),
        "gui_tour" => gui_tour(args),
        "set_theme" => set_theme(args),
        "annotate_preview" => annotate_preview(args),
        "drive_preview" => drive_preview(args),
        "desktop_preview" => desktop_preview(args),
        _ => Ok(()),
    }
}

fn focus_pane(args: &Value) -> Result<(), String> {
    let pane = text(args, "pane").unwrap_or_default();
    if PANES.contains(&pane) {
        return Ok(());
    }
    Err(format!(
        "unknown pane '{pane}'; use one of {}",
        PANES.join(", ")
    ))
}

fn gui_tour(args: &Value) -> Result<(), String> {
    let action = text(args, "action").unwrap_or_default();
    if action == "start" && args.get("steps").is_some() && args.get("preset").is_some() {
        return Err("pass steps or preset, not both".to_owned());
    }
    if action == "show" && text(args, "selector").is_none() {
        return Err("gui_tour show needs a selector".to_owned());
    }
    Ok(())
}

fn set_theme(args: &Value) -> Result<(), String> {
    let picker = text(args, "picker");
    let others = ["accent", "mode", "layout"]
        .iter()
        .filter(|k| args.get(**k).is_some())
        .count();
    if picker.is_some() && others > 0 {
        return Err(
            "picker shows a card on its own; pass it without accent, mode or layout".to_owned(),
        );
    }
    if picker.is_none() && others == 0 {
        return Err("pass accent, mode, layout or picker".to_owned());
    }
    if let Some(accent) = text(args, "accent")
        && !ACCENTS.contains(&accent)
        && !is_hex_colour(accent)
    {
        return Err(format!(
            "unknown accent '{accent}'; use one of {} or #rrggbb",
            ACCENTS.join(", ")
        ));
    }
    if let Some(mode) = text(args, "mode")
        && !MODES.contains(&mode)
    {
        return Err(format!(
            "unknown mode '{mode}'; use one of {}",
            MODES.join(", ")
        ));
    }
    Ok(())
}

fn is_hex_colour(text: &str) -> bool {
    text.len() == 7 && text.starts_with('#') && text[1..].chars().all(|c| c.is_ascii_hexdigit())
}

fn annotate_preview(args: &Value) -> Result<(), String> {
    let action = text(args, "action").unwrap_or_default();
    if matches!(action, "add" | "remove") && !has_target(args) {
        return Err(format!(
            "annotate_preview {action} needs a target: pass ref or selector"
        ));
    }
    Ok(())
}

fn drive_preview(args: &Value) -> Result<(), String> {
    let action = text(args, "action").unwrap_or_default();
    if matches!(action, "click" | "hover" | "type") && !has_target(args) {
        return Err(format!(
            "drive_preview {action} needs a target: pass ref or selector"
        ));
    }
    if action == "type" && args.get("text").and_then(Value::as_str).is_none() {
        return Err("drive_preview type needs text".to_owned());
    }
    if action == "press" && text(args, "key").is_none() {
        return Err("drive_preview press needs a key".to_owned());
    }
    Ok(())
}

fn desktop_preview(args: &Value) -> Result<(), String> {
    if text(args, "action") == Some("open") && text(args, "url").is_none() {
        return Err("desktop_preview open needs a url".to_owned());
    }
    Ok(())
}

/// How long the window has to answer `tool`.
#[must_use]
pub(super) fn deadline(tool: &str, args: &Value) -> Duration {
    let waits_for_user = match tool {
        "gui_tour" | "setup_beat" => true,
        "set_theme" => args.get("picker").is_some(),
        _ => false,
    };
    if waits_for_user { user_wait() } else { WAIT }
}

/// 300 s; debug builds take `PMT_TEST_UI_USER_WAIT_SECS` so tests need
/// not wait five minutes.
fn user_wait() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(secs) = std::env::var("PMT_TEST_UI_USER_WAIT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        return Duration::from_secs(secs);
    }
    WAIT_FOR_USER
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Scenario: Unknown pane.
    #[test]
    fn an_unknown_pane_lists_the_five_allowed_ones() {
        let refused = refusal("focus_pane", &json!({"pane": "inbox"})).unwrap_err();
        assert_eq!(
            refused,
            "unknown pane 'inbox'; use one of chat, files, terminal, review, sessions"
        );
        assert!(refusal("focus_pane", &json!({"pane": "review"})).is_ok());
    }

    /// Scenario: Preset and steps together.
    #[test]
    fn a_tour_takes_steps_or_a_preset_not_both() {
        let both = json!({"action": "start", "preset": "quick", "steps": [{"selector": "a"}]});
        assert_eq!(
            refusal("gui_tour", &both).unwrap_err(),
            "pass steps or preset, not both"
        );
        assert!(refusal("gui_tour", &json!({"action": "start", "preset": "quick"})).is_ok());
    }

    #[test]
    fn marks_and_actions_name_their_missing_target() {
        let add = refusal("annotate_preview", &json!({"action": "add"})).unwrap_err();
        assert!(add.contains("needs a target"), "{add}");
        assert!(refusal("annotate_preview", &json!({"action": "clear"})).is_ok());
        let click = refusal("drive_preview", &json!({"action": "click"})).unwrap_err();
        assert!(click.contains("ref or selector"), "{click}");
    }

    #[test]
    fn appearance_values_are_checked_and_pickers_stand_alone() {
        assert!(refusal("set_theme", &json!({"accent": "#12ab9F"})).is_ok());
        assert!(refusal("set_theme", &json!({"accent": "teal"})).is_err());
        assert!(refusal("set_theme", &json!({"mode": "dim"})).is_err());
        assert!(refusal("set_theme", &json!({"picker": "accent", "mode": "dark"})).is_err());
        assert!(refusal("set_theme", &json!({})).is_err());
    }

    #[test]
    fn pickers_tours_and_cards_wait_for_the_user() {
        assert_eq!(deadline("read_terminal", &json!({})), WAIT);
        assert_eq!(deadline("set_theme", &json!({"accent": "red"})), WAIT);
        assert!(deadline("set_theme", &json!({"picker": "layout"})) > WAIT);
        assert!(deadline("gui_tour", &json!({"action": "start"})) > WAIT);
    }
}
