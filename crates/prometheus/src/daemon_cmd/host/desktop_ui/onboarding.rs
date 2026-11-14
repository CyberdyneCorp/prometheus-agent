//! The first-run onboarding chat (spec: desktop-app, First-run onboarding
//! chat): `onboarding.ensure` opens the profile's onboarding session (the
//! only one whose tools include `setup_beat`), and `setup_beat` shows one
//! setup card after another, waits up to 300 s for the user and keeps
//! each answer in the session's onboarding record,
//! `<profile>/onboarding/<session>.json`.
//!
//! Only the user's Skip setup button ends onboarding early: the window
//! then answers `ended`, and later beats are refused.

use std::path::PathBuf;
use std::sync::Arc;

use pmt_core::error::{Error, codes};
use pmt_store::{NewSession, Source};
use pmt_tools::ToolResult;
use serde_json::{Map, Value, json};

use super::super::{DaemonHost, Profile, setup};
use super::{UiPort, answer, check, fail, window};
use crate::chat::session::tools::ONBOARDING_TAG;

/// The beats, in the order their cards are shown.
pub(super) const BEATS: &[&str] = &[
    "name",
    "theme",
    "layout",
    "connectors",
    "plugins",
    "tour",
    "build",
];
/// Most plugins the `plugins` card suggests.
const MAX_SUGGESTED: usize = 5;

/// `profile`'s onboarding chat, created when it has none.
///
/// # Errors
///
/// Store, home and model-route errors.
pub(in super::super) async fn ensure_chat(
    host: &DaemonHost,
    profile: &Arc<Profile>,
) -> Result<(String, bool), Error> {
    let cwd = DaemonHost::cwd_of(profile, None);
    std::fs::create_dir_all(&cwd).map_err(|e| Error::io(&codes::HOME_IO, &cwd, &e))?;
    let defaults = pmt_rpc::contracts::SessionSetup::default();
    let factory = host.factory(profile, cwd.clone(), &defaults)?;
    let target = factory.models.primary(&setup::flags(&defaults)).await?;
    let mut new = NewSession::new(Source::Desktop);
    new.provider = Some(target.provider.clone());
    new.model.clone_from(&target.model);
    new.cwd = Some(cwd.to_string_lossy().into_owned());
    new.source_tag = Some(ONBOARDING_TAG.to_owned());
    new.title = Some("Welcome to Prometheus".to_owned());
    let (session, created) = profile.store.ensure_tagged_session(&new)?;
    Ok((session.id.to_string(), created))
}

fn record_path(port: &UiPort) -> PathBuf {
    port.scope
        .dir()
        .join("onboarding")
        .join(format!("{}.json", port.session))
}

fn read_record(port: &UiPort) -> Map<String, Value> {
    std::fs::read(record_path(port))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Map<String, Value>>(&bytes).ok())
        .unwrap_or_default()
}

fn write_record(port: &UiPort, record: &Map<String, Value>) -> Result<(), String> {
    let path = record_path(port);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let text = serde_json::to_vec_pretty(record).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| e.to_string())
}

/// The first beat the record has no answer for.
fn next_beat(beats: &Map<String, Value>) -> Option<&'static str> {
    BEATS.iter().copied().find(|b| !beats.contains_key(*b))
}

/// Why `beat` may not be shown now, if it may not.
fn refusal(record: &Map<String, Value>, args: &Value) -> Result<&'static str, String> {
    if record.get("ended").and_then(Value::as_bool) == Some(true) {
        return Err("onboarding has ended: the user skipped setup".to_owned());
    }
    let name = args.get("beat").and_then(Value::as_str).unwrap_or_default();
    let beat = BEATS
        .iter()
        .copied()
        .find(|b| *b == name)
        .ok_or_else(|| format!("unknown beat '{name}'; use one of {}", BEATS.join(", ")))?;
    let beats = record
        .get("beats")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(next) = next_beat(&beats)
        && !beats.contains_key(beat)
        && beat != next
    {
        return Err(format!(
            "show the {next} card before {beat}: beats go {}",
            BEATS.join(", ")
        ));
    }
    let suggested = args
        .get("suggest")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    if suggested > MAX_SUGGESTED {
        return Err(format!("suggest at most {MAX_SUGGESTED} plugins"));
    }
    Ok(beat)
}

/// Runs one `setup_beat` call.
pub(super) async fn beat(port: &UiPort, args: Value) -> ToolResult {
    let mut record = read_record(port);
    let beat = match refusal(&record, &args) {
        Ok(beat) => beat,
        Err(refused) => return fail(refused),
    };
    let timeout = check::deadline("setup_beat", &args);
    let (recorded, outcome) = match window::ask(port, "setup_beat", args, timeout).await {
        Ok(value) => (value.get("answer").cloned().unwrap_or(Value::Null), value),
        Err(window::Unanswered::TimedOut) => {
            let note = format!(
                "the {beat} card timed out after {} s; the defaults stay in place",
                timeout.as_secs()
            );
            (
                json!({"timed_out": true}),
                json!({"timed_out": true, "error": note}),
            )
        }
        Err(unanswered) => return unanswered.result("setup_beat", timeout),
    };
    let ended = outcome.get("ended").and_then(Value::as_bool) == Some(true);
    let beats = record
        .entry("beats")
        .or_insert_with(|| Value::Object(Map::new()));
    beats[beat] = recorded.clone();
    let next = beats.as_object().and_then(next_beat);
    if ended {
        record.insert("ended".to_owned(), Value::Bool(true));
    }
    if let Err(err) = write_record(port, &record) {
        return fail(format!("the onboarding record could not be saved: {err}"));
    }
    answer(&reply(
        beat,
        &recorded,
        &outcome,
        if ended { None } else { next },
    ))
}

fn reply(beat: &str, recorded: &Value, outcome: &Value, next: Option<&str>) -> Value {
    let reminder = match next {
        Some(next) => format!("next: show the {next} card"),
        None => "onboarding is complete; no more setup cards".to_owned(),
    };
    let mut reply = json!({
        "success": outcome.get("timed_out").is_none(),
        "beat": beat,
        "answer": recorded,
        "next": next,
        "reminder": reminder,
    });
    for key in ["error", "timed_out", "ended"] {
        if let Some(value) = outcome.get(key) {
            reply[key] = value.clone();
        }
    }
    reply
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn record(beats: &[&str]) -> Map<String, Value> {
        let answers: Map<String, Value> = beats
            .iter()
            .map(|b| ((*b).to_owned(), json!("x")))
            .collect();
        let mut record = Map::new();
        record.insert("beats".to_owned(), Value::Object(answers));
        record
    }

    #[test]
    fn beats_go_in_order_and_skipping_ahead_is_refused() {
        assert_eq!(refusal(&record(&[]), &json!({"beat": "name"})), Ok("name"));
        let ahead = refusal(&record(&["name"]), &json!({"beat": "layout"})).unwrap_err();
        assert!(
            ahead.contains("show the theme card before layout"),
            "{ahead}"
        );
        assert_eq!(
            refusal(&record(&["name"]), &json!({"beat": "name"})),
            Ok("name")
        );
        let many = json!({"beat": "plugins", "suggest": ["a", "b", "c", "d", "e", "f"]});
        assert!(refusal(&record(&["name", "theme", "layout", "connectors"]), &many).is_err());
    }

    #[test]
    fn an_ended_onboarding_takes_no_more_beats() {
        let mut ended = record(&["name"]);
        ended.insert("ended".to_owned(), Value::Bool(true));
        assert!(refusal(&ended, &json!({"beat": "theme"})).is_err());
    }
}
