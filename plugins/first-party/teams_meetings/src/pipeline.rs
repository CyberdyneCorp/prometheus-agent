//! The summary pipeline: a transcript notification becomes one summary
//! turn (a message from `teams_meetings` in the chat `meeting:<id>:<key>`
//! holding the prompt and the transcript), the agent's answer is
//! delivered by the incoming webhook or into the meeting chat, and each
//! transcript of a meeting is summarized at most once: a later
//! notification for it is a `duplicate`. The key is the transcript's
//! `contentCorrelationId`, which the recording of the same occurrence
//! shares (so the two are summarized once together), else its id; a
//! recurring meeting's next occurrence keeps its meeting id but brings a
//! new key. Without a transcript, the recording stands in (as audio the
//! gateway transcribes) only with `use_recordings = true`. Subscriptions
//! are renewed by the ticker before they expire.

use adapter_kit::message::{InFile, Inbound};
use adapter_kit::{host, net, time};
use serde_json::{Value, json};

use crate::graph::{Artifact, artifact};
use crate::{Delivery, Meetings, records};

/// The sender of summary turns.
pub(crate) const SENDER: &str = "teams_meetings";
/// The most transcript text put in one turn.
const TRANSCRIPT_CHARS: usize = 200_000;

/// The chat of the summary turn of key `key` of `meeting` (its own
/// session, so one summary never sees another's transcript).
fn chat_of(meeting: &str, key: &str) -> String {
    format!("meeting:{meeting}:{key}")
}

/// The meeting and key of a summary turn's chat. Graph ids are base64 or
/// GUIDs, so neither holds a `:`.
fn parse_chat(chat: &str) -> Option<(&str, &str)> {
    chat.strip_prefix("meeting:")?
        .split_once(':')
        .filter(|(meeting, key)| !meeting.is_empty() && !key.is_empty())
}

fn turn_text(prompt: &str, body: &str) -> String {
    if prompt.is_empty() {
        body.to_owned()
    } else {
        format!("{prompt}\n\n{body}")
    }
}

impl Meetings {
    /// Takes one admitted notification.
    pub(crate) fn notification(&mut self, notification: &Value) {
        let receipt = adapter_kit::graph::receipt(notification);
        let Some(found) = artifact(notification["resource"].as_str().unwrap_or_default()) else {
            records::note(&receipt, "", "", "ignored");
            return;
        };
        if found.kind == "recordings" && !self.settings.use_recordings {
            records::note(&receipt, &found.meeting, &found.id, "ignored");
            return;
        }
        let key = self.key_of(&found);
        let status = if records::held(&found.meeting, &key, time::now()) {
            "duplicate"
        } else {
            match self.summarize(&found, &key, &receipt) {
                Ok(()) => "queued",
                Err(why) => {
                    host::warn(&format!("meeting {} not summarized: {why}", found.meeting));
                    "failed"
                }
            }
        };
        records::note(&receipt, &found.meeting, &key, status);
    }

    /// The key `found` is summarized once under: its correlation id when
    /// Graph tells it, else its own id.
    fn key_of(&mut self, found: &Artifact) -> String {
        self.graph
            .correlation(&found.path)
            .unwrap_or_else(|| found.id.clone())
    }

    /// Queues the summary turn of `found` (a transcript, or the recording
    /// standing in for a missing one) under `key`.
    fn summarize(&mut self, found: &Artifact, key: &str, receipt: &str) -> Result<(), String> {
        let mut inbound = Inbound::text(receipt, &chat_of(&found.meeting, key), "dm", SENDER, "");
        inbound.sender_name = Some("Teams meetings".to_owned());
        let transcript = if found.kind == "transcripts" {
            self.graph
                .transcript(&found.path)
                .ok()
                .filter(|t| !t.trim().is_empty())
        } else {
            None
        };
        match transcript {
            Some(text) => {
                let text: String = text.chars().take(TRANSCRIPT_CHARS).collect();
                inbound.text = turn_text(&self.settings.prompt, &text);
            }
            None if self.settings.use_recordings => {
                let recording = if found.kind == "recordings" {
                    found.clone()
                } else {
                    self.graph
                        .latest(&found.meeting_path, "recordings")?
                        .ok_or("no transcript and no recording")?
                };
                let bytes = self.graph.recording(&recording.path)?;
                inbound.text = turn_text(
                    &self.settings.prompt,
                    "(The meeting has no transcript: its recording is attached.)",
                );
                let name = format!("{}.mp4", recording.id);
                inbound
                    .attachments
                    .push(InFile::bytes("audio", &name, bytes));
            }
            None => return Err("the transcript is not available".to_owned()),
        }
        records::mark(
            &found.meeting,
            key,
            "queued",
            time::now(),
            Some(&found.meeting_path),
        );
        self.inbox.push(inbound);
        Ok(())
    }

    /// Summarizes again the meetings `prometheus plugins run teams_meetings
    /// replay` asked for (their newest transcript).
    pub(crate) fn replays(&mut self) {
        for meeting in records::take_replays() {
            let record = records::meeting(&meeting);
            let path = record["path"].as_str().map_or_else(
                || format!("communications/onlineMeetings/{meeting}"),
                str::to_owned,
            );
            let receipt = format!("replay:{meeting}:{}", adapter_kit::clock::now_ms());
            let (key, status) = match self.graph.latest(&path, "transcripts") {
                Ok(Some(found)) => {
                    let key = self.key_of(&found);
                    let status = match self.summarize(&found, &key, &receipt) {
                        Ok(()) => "queued",
                        Err(_) => "failed",
                    };
                    (key, status)
                }
                _ => ("replay".to_owned(), "failed"),
            };
            records::note(&receipt, &meeting, &key, status);
        }
    }

    /// Delivers a summary of `chat` (`meeting:<id>:<key>`). A permanent
    /// failure marks the key `failed`, so a later notification retries it.
    pub(crate) fn deliver(&mut self, chat: &str, text: &str) -> Result<String, String> {
        let (meeting, key) =
            parse_chat(chat).ok_or("permanent: teams_meetings only delivers meeting summaries")?;
        let sent = self.send_summary(meeting, text);
        let status = match &sent {
            Ok(_) => "summarized",
            Err(why) if why.starts_with("permanent:") => "failed",
            Err(_) => return sent,
        };
        records::mark(meeting, key, status, time::now(), None);
        records::settle_notes(meeting, key, status);
        sent
    }

    fn send_summary(&mut self, meeting: &str, text: &str) -> Result<String, String> {
        let record = records::meeting(meeting);
        match self.settings.delivery.clone() {
            Delivery::Webhook(secret) => {
                let url = host::secret(&secret)
                    .ok_or_else(|| format!("permanent: secret {secret} is not set"))?;
                net::json("POST", &url, &[], &json!({"text": text}))?;
                let n = record["delivered"].as_u64().unwrap_or(0) + 1;
                Ok(format!("summary-{meeting}-{n}"))
            }
            Delivery::Graph => {
                let path = record["path"].as_str().map_or_else(
                    || format!("communications/onlineMeetings/{meeting}"),
                    str::to_owned,
                );
                self.graph.post_to_chat(&path, text)
            }
        }
    }

    /// Now, in epoch seconds (asking Graph once when no answer told it yet).
    pub(crate) fn now(&mut self) -> Option<u64> {
        if time::now().is_none() {
            let _ = self.graph.json("GET", "subscriptions", None);
        }
        time::now()
    }

    /// Renews the recorded subscriptions expiring within the threshold
    /// (all of them, or `only`, when `force`): what was done, a line each.
    pub(crate) fn renew(&mut self, force: bool, only: Option<&str>) -> Vec<String> {
        let Some(now) = self.now() else {
            return vec!["the time is unknown: Graph sent no Date".to_owned()];
        };
        let mut lines = Vec::new();
        let mut subs = records::subscriptions();
        for sub in &mut subs {
            let id = sub["id"].as_str().unwrap_or_default().to_owned();
            if only.is_some_and(|o| o != id) {
                continue;
            }
            let expires = sub["expires"]
                .as_str()
                .and_then(time::parse_iso)
                .unwrap_or(0);
            if !force && expires > now + self.settings.renew_within_secs {
                continue;
            }
            let wanted = time::iso(now + self.settings.extend_secs);
            let patch = json!({"expirationDateTime": wanted});
            match self
                .graph
                .json("PATCH", &format!("subscriptions/{id}"), Some(&patch))
            {
                Ok(answer) => {
                    let granted = answer["expirationDateTime"]
                        .as_str()
                        .unwrap_or(&wanted)
                        .to_owned();
                    sub["expires"] = json!(granted);
                    sub["status"] = json!("active");
                    sub["renewed_at"] = json!(time::iso(now));
                    lines.push(format!("{id} renewed until {granted}"));
                }
                Err(why) => {
                    sub["status"] = json!("renewal_failed");
                    lines.push(format!("{id} not renewed: {why}"));
                }
            }
        }
        records::store_subscriptions(&subs);
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_summary_chat_names_its_meeting_and_key() {
        let chat = chat_of("MSo1N2Y5", "c0ffee-01");
        assert_eq!(parse_chat(&chat), Some(("MSo1N2Y5", "c0ffee-01")));
        assert_eq!(parse_chat("meeting:M1"), None);
        assert_eq!(parse_chat("meeting::T1"), None);
        assert_eq!(parse_chat("dm:M1:T1"), None);
    }
}
