//! The fixed texts of the prompt: identity, tool-use and model-family
//! guidance, the session line and the runtime environment block.

use std::path::Path;

use jiff::Timestamp;
use pmt_core::id::SessionId;

use super::scan;

/// The identity used when the profile has no (non-empty) `SOUL.md`.
pub const DEFAULT_IDENTITY: &str = "You are Prometheus, a personal AI agent.";

/// The identity file in a profile directory.
pub const IDENTITY_FILE: &str = "SOUL.md";

/// The runtime block's first line.
pub const RUNTIME_START: &str = "# Prometheus runtime environment";
/// The runtime block's last line.
pub const RUNTIME_END: &str = "<!-- End Prometheus runtime environment -->";

/// The identity: the profile's `SOUL.md` when `load` and it is non-empty
/// (scanned, findings logged at WARN, loaded unchanged), else the default.
#[must_use]
pub fn identity(profile_dir: &Path, load: bool) -> String {
    if !load {
        return DEFAULT_IDENTITY.to_owned();
    }
    let path = profile_dir.join(IDENTITY_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return DEFAULT_IDENTITY.to_owned();
    };
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(&text).trim();
    if text.is_empty() {
        return DEFAULT_IDENTITY.to_owned();
    }
    let found = scan::findings(text);
    if !found.is_empty() {
        tracing::warn!(file = %path.display(), findings = ?found, "SOUL.md matches threat patterns; loaded unchanged");
    }
    text.to_owned()
}

/// Guidance on using the session's tools, naming only tools it has.
#[must_use]
pub fn tool_guidance(tools: &[String]) -> Option<String> {
    if tools.is_empty() {
        return None;
    }
    let has = |name: &str| tools.iter().any(|t| t == name);
    let mut lines = vec![
        "# Tool use".to_owned(),
        "Use tools to act and to check facts instead of guessing. Call independent tools in one response; wait for results before depending on them.".to_owned(),
    ];
    if has("todo") {
        lines.push("Track multi-step work with `todo` and keep it current.".to_owned());
    }
    if has("clarify") {
        lines.push("Ask with `clarify` only when the request is ambiguous and a wrong guess would be costly.".to_owned());
    }
    if has("session_search") {
        lines.push("Use `session_search` to recall earlier conversations.".to_owned());
    }
    Some(lines.join("\n"))
}

/// Guidance for a model family, when one applies.
#[must_use]
pub fn model_guidance(model: &str) -> Option<&'static str> {
    let model = model.to_ascii_lowercase();
    if model.contains("gpt")
        || model.contains("codex")
        || model.starts_with("o3")
        || model.starts_with("o4")
    {
        return Some(
            "When you decide to use a tool, call it in the same response instead of announcing that you will.",
        );
    }
    if model.contains("gemini") || model.contains("gemma") {
        return Some(
            "Keep tool arguments exactly as the schemas define them; do not invent fields.",
        );
    }
    None
}

/// The session line: start time (UTC, seconds) and, when asked, the id.
#[must_use]
pub fn session_info(started_at: Timestamp, session_id: Option<SessionId>) -> String {
    let started = started_at.strftime("%Y-%m-%dT%H:%M:%SZ");
    match session_id {
        Some(id) => format!("Session started: {started}\nSession ID: {id}"),
        None => format!("Session started: {started}"),
    }
}

/// The runtime environment block (always the last text of the prompt).
#[must_use]
pub fn runtime(platform: &str, cwd: Option<&Path>) -> String {
    let mut lines = vec![
        RUNTIME_START.to_owned(),
        format!(
            "- Operating system: {} ({})",
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
        format!("- Surface: {platform}"),
    ];
    if let Some(cwd) = cwd {
        lines.push(format!("- Working directory: {}", cwd.display()));
    }
    lines.push(RUNTIME_END.to_owned());
    lines.join("\n")
}
