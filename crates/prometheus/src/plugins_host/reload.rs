//! `prometheus plugins reload [<id>]` and `/plugins reload [<id>] [--now]`
//! (spec: plugin-system, Hot reload respects the prompt-cache invariant):
//! the profile's live set is replaced, then the command says which parts
//! of the plugins are active now and which wait for the next session.

use std::fmt::Write as _;

use pmt_core::error::{Error, codes};
use pmt_plugins::load::{Loaded, State};

/// The parts a reload activates in running sessions at once.
const NOW: &[&str] = &["hooks", "slash commands", "event subscribers"];

/// The parts that are read when a session starts (and by `--now`).
const DEFERRED: &[&str] = &["tools", "system prompt sections", "skills"];

/// Checks that `id`, when given, is a plugin of the profile.
///
/// # Errors
///
/// `plugins.not_found` (66): `plugin '<id>' is not installed`.
pub fn check_target(loaded: &Loaded, id: Option<&str>) -> Result<(), Error> {
    match id {
        Some(id) if !loaded.statuses.iter().any(|s| s.id == id) => Err(Error::new(
            &codes::PLUGINS_NOT_FOUND,
            format!("plugin '{id}' is not installed"),
        )),
        _ => Ok(()),
    }
}

/// What a reload did: the plugins' states after it, then one line per
/// part; with `now` the deferred parts were applied to the current
/// session too.
#[must_use]
pub fn report(loaded: &Loaded, id: Option<&str>, now: bool) -> String {
    let mut text = String::new();
    for status in loaded
        .statuses
        .iter()
        .filter(|s| id.is_none_or(|id| s.id == id))
    {
        let _ = write!(text, "{} {}", status.id, status.state.as_str());
        if let (State::Error, Some(problem)) = (status.state, &status.problem) {
            let _ = write!(text, ": {problem}");
        }
        text.push('\n');
    }
    if text.is_empty() {
        text.push_str("no plugins\n");
    }
    for part in NOW {
        let _ = writeln!(text, "{part}: now");
    }
    let deferred = if now {
        "now (this session)"
    } else {
        "next session"
    };
    for part in DEFERRED {
        let _ = writeln!(text, "{part}: {deferred}");
    }
    text.trim_end().to_owned()
}
