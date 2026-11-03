//! Native slash commands (spec: platform-adapters, Slack slash commands
//! and thread command prefix): each registry command arrives as its own
//! slash command, or through the umbrella `/prometheus <command> [args]`
//! (`/prometheus <free text>` is an ordinary message). The command is
//! acknowledged with a private "Running /<cmd>…" placeholder; what the
//! gateway answers to it replaces the placeholder, privately, through the
//! command's `response_url`, in at most 5 posts followed by a truncation
//! notice. It never falls back to a public post.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;

use pmt_gateway::messaging::{ChatType, Inbound, SendError};
use serde_json::{Value, json};

/// The umbrella command.
pub(crate) const UMBRELLA: &str = "/prometheus";
/// Private posts per command at most.
const MAX_POSTS: usize = 5;
/// Characters per private post.
const POST_CHARS: usize = 3000;
/// The message id prefix of a slash command's event.
const PREFIX: &str = "slash:";

/// Where the answers of the slash commands in flight go.
#[derive(Debug, Default)]
pub(crate) struct Replies {
    /// `response_url` and posts made, by the command's message id.
    open: Mutex<HashMap<String, (String, usize)>>,
    /// The registry's command names (the umbrella dispatches only those).
    pub(crate) commands: Mutex<BTreeSet<String>>,
}

/// The placeholder a command is acknowledged with.
pub(crate) fn placeholder(payload: &Value) -> Value {
    let name = invoked(payload).map_or_else(|| UMBRELLA.to_owned(), |(cmd, _)| cmd);
    json!({"response_type": "ephemeral", "text": format!("Running {name}…")})
}

/// The command and its text a payload invokes (`None`: umbrella free
/// text).
fn invoked(payload: &Value) -> Option<(String, String)> {
    let command = payload["command"].as_str()?;
    let text = payload["text"].as_str().unwrap_or_default().trim();
    if command != UMBRELLA {
        return Some((command.to_owned(), text.to_owned()));
    }
    let (first, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    Some((format!("/{first}"), rest.trim().to_owned())).filter(|_| !first.is_empty())
}

impl Replies {
    /// The event of a `slash_commands` payload; remembers where its
    /// answers go.
    pub(crate) fn event(&self, payload: &Value, team: Option<&str>) -> Option<Inbound> {
        let channel = payload["channel_id"].as_str()?;
        let user = payload["user_id"].as_str()?;
        let known = |cmd: &str| {
            self.commands
                .lock()
                .expect("slash poisoned")
                .contains(cmd.trim_start_matches('/'))
        };
        let text = match invoked(payload) {
            Some((cmd, args)) if payload["command"] != UMBRELLA || known(&cmd) => {
                if args.is_empty() {
                    cmd
                } else {
                    format!("{cmd} {args}")
                }
            }
            // `/prometheus <free text>` is an ordinary message.
            _ => payload["text"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .to_owned(),
        };
        let chat_type = if channel.starts_with('D') {
            ChatType::Dm
        } else {
            ChatType::Group
        };
        let mut inbound = Inbound::text(chat_type, channel, user, &text);
        let trigger = payload["trigger_id"].as_str().unwrap_or_default();
        inbound.message_id = format!("{PREFIX}{trigger}");
        inbound.sender_name = payload["user_name"].as_str().map(str::to_owned);
        inbound.scope_id = team.map(str::to_owned);
        if let Some(url) = payload["response_url"].as_str()
            && text.starts_with('/')
        {
            self.open
                .lock()
                .expect("slash poisoned")
                .insert(inbound.message_id.clone(), (url.to_owned(), 0));
        }
        Some(inbound)
    }

    /// The private posts answering command `reply_to` with `text`; `None`
    /// when the message does not answer an open slash command.
    pub(crate) fn posts(&self, reply_to: Option<&str>, text: &str) -> Option<(String, Vec<Value>)> {
        let id = reply_to.filter(|id| id.starts_with(PREFIX))?;
        let mut open = self.open.lock().expect("slash poisoned");
        let (url, made) = open.get_mut(id)?;
        let chunks: Vec<String> = text
            .chars()
            .collect::<Vec<_>>()
            .chunks(POST_CHARS)
            .map(|c| c.iter().collect())
            .collect();
        let room = MAX_POSTS.saturating_sub(*made);
        let truncated = chunks.len() > room;
        let mut posts: Vec<Value> = chunks
            .into_iter()
            .take(room)
            .enumerate()
            .map(|(i, chunk)| {
                json!({"response_type": "ephemeral", "text": chunk,
                       "replace_original": *made == 0 && i == 0})
            })
            .collect();
        if truncated && let Some(last) = posts.last_mut() {
            let text = format!(
                "{}\n… output truncated after {MAX_POSTS} messages",
                last["text"].as_str().unwrap_or_default()
            );
            last["text"] = json!(text);
        }
        *made += posts.len();
        Some((url.clone(), posts))
    }
}

/// Posts `bodies` to a command's `response_url` (never publicly).
pub(crate) async fn respond(
    client: &reqwest::Client,
    url: &str,
    bodies: Vec<Value>,
) -> Result<(), SendError> {
    for body in bodies {
        let answer = client
            .post(url)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| SendError::Network(e.to_string()))?;
        if !answer.status().is_success() {
            return Err(SendError::Permanent(format!(
                "the slash command's response URL answered {}",
                answer.status()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(command: &str, text: &str) -> Value {
        json!({"command": command, "text": text, "channel_id": "C1", "user_id": "U1",
               "trigger_id": "t1", "response_url": "http://x/r"})
    }

    #[test]
    fn the_umbrella_dispatches_registry_commands_and_passes_free_text() {
        let replies = Replies::default();
        replies.commands.lock().unwrap().insert("model".into());
        let event = replies
            .event(&payload(UMBRELLA, "model fast"), None)
            .unwrap();
        assert_eq!(event.text, "/model fast");
        let event = replies
            .event(&payload(UMBRELLA, "what is up"), None)
            .unwrap();
        assert_eq!(event.text, "what is up");
        let event = replies.event(&payload("/stop", ""), None).unwrap();
        assert_eq!(event.text, "/stop");
        assert_eq!(placeholder(&payload("/stop", ""))["text"], "Running /stop…");
    }

    #[test]
    fn long_output_is_at_most_five_private_posts() {
        let replies = Replies::default();
        let event = replies.event(&payload("/help", ""), None).unwrap();
        let long = "x".repeat(POST_CHARS * 7);
        let (url, posts) = replies.posts(Some(&event.message_id), &long).unwrap();
        assert_eq!(url, "http://x/r");
        assert_eq!(posts.len(), 5);
        assert_eq!(posts[0]["replace_original"], true);
        assert!(posts.iter().all(|p| p["response_type"] == "ephemeral"));
        assert!(
            posts[4]["text"]
                .as_str()
                .unwrap()
                .ends_with("truncated after 5 messages")
        );
        assert!(replies.posts(Some("1700.1"), "public").is_none());
    }
}
