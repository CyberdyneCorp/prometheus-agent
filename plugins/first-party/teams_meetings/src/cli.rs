//! `prometheus plugins run teams_meetings <command>`: `subscribe` creates
//! the Graph subscriptions for the configured resources (transcripts, and
//! recordings with `use_recordings`) unless recorded already, `list`
//! shows them, `renew [<id>]` renews now, `remove <id>` deletes one,
//! `status` shows subscriptions, meetings and recent notifications, and
//! `replay <meeting-id>` has the running gateway summarize a meeting
//! again.

use std::fmt::Write as _;

use adapter_kit::{host, time};
use serde_json::{Value, json};

use crate::{Meetings, records};

/// The commands, as the descriptor lists them.
pub(crate) fn commands() -> Value {
    json!([
        {"name": "subscribe", "description": "Create the Graph subscriptions for meeting transcripts (and recordings)"},
        {"name": "list", "description": "List the recorded subscriptions"},
        {"name": "renew", "description": "Renew every subscription now, or the one named"},
        {"name": "remove", "description": "Delete a subscription: remove <subscription-id>"},
        {"name": "status", "description": "Subscriptions, meetings and how recent notifications fared"},
        {"name": "replay", "description": "Summarize a meeting again: replay <meeting-id>"},
    ])
}

fn subscription_line(sub: &Value) -> String {
    let field = |k: &str| sub[k].as_str().unwrap_or("-").to_owned();
    format!(
        "{}  {}  expires {}  {}",
        field("id"),
        field("resource"),
        field("expires"),
        field("status")
    )
}

impl Meetings {
    /// Runs `name` with `args`; its output.
    pub(crate) fn cli(&mut self, name: &str, args: &[String]) -> Result<String, String> {
        let first = args.first().map(String::as_str);
        match (name, first) {
            ("subscribe", _) => self.subscribe(),
            ("list", _) => Ok(list()),
            ("renew", only) => Ok(self.renew(true, only).join("\n")),
            ("remove", Some(id)) => self.remove(id),
            ("status", _) => Ok(status()),
            ("replay", Some(meeting)) => {
                records::request_replay(meeting);
                Ok(format!(
                    "replay of meeting {meeting} queued for the running gateway"
                ))
            }
            ("remove" | "replay", None) => Err(format!("usage: {name} <id>")),
            _ => Err(format!("unknown command {name}")),
        }
    }

    fn subscribe(&mut self) -> Result<String, String> {
        let url =
            self.settings.public_url.clone().ok_or(
                "options.public_url (the HTTPS URL Graph posts to) is required to subscribe",
            )?;
        let state = host::secret(&self.settings.client_state_secret)
            .ok_or_else(|| format!("secret {} is not set", self.settings.client_state_secret))?;
        let now = self
            .now()
            .ok_or("the time is unknown: Graph sent no Date")?;
        let known: Vec<String> = records::subscriptions()
            .iter()
            .filter_map(|s| s["resource"].as_str().map(str::to_owned))
            .collect();
        let mut out = String::new();
        for resource in self.settings.resources.clone() {
            if known.contains(&resource) {
                let _ = writeln!(out, "{resource}: already subscribed");
                continue;
            }
            let body = json!({
                "changeType": "created",
                "notificationUrl": url,
                "lifecycleNotificationUrl": url,
                "resource": resource,
                "expirationDateTime": time::iso(now + self.settings.extend_secs),
                "clientState": state,
            });
            let created = self.graph.json("POST", "subscriptions", Some(&body))?;
            let sub = json!({
                "id": created["id"],
                "resource": resource,
                "expires": created["expirationDateTime"],
                "status": "active",
            });
            let _ = writeln!(out, "{}", subscription_line(&sub));
            records::upsert_subscription(sub);
        }
        Ok(out)
    }

    fn remove(&mut self, id: &str) -> Result<String, String> {
        match self
            .graph
            .json("DELETE", &format!("subscriptions/{id}"), None)
        {
            Ok(_) => {}
            Err(why) if why.contains("status 404") => {}
            Err(why) => return Err(why),
        }
        let mut subs = records::subscriptions();
        subs.retain(|s| s["id"] != id);
        records::store_subscriptions(&subs);
        Ok(format!("{id} removed"))
    }
}

fn list() -> String {
    let subs = records::subscriptions();
    if subs.is_empty() {
        return "no subscriptions (run subscribe)".to_owned();
    }
    subs.iter()
        .map(subscription_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn status() -> String {
    let mut out = String::from("subscriptions:\n");
    for sub in records::subscriptions() {
        let _ = writeln!(out, "  {}", subscription_line(&sub));
    }
    out.push_str("meetings:\n");
    for (id, record) in records::meetings() {
        let _ = writeln!(
            out,
            "  {id}  {}  artifact {}",
            record["status"].as_str().unwrap_or("-"),
            record["artifact"].as_str().unwrap_or("-")
        );
    }
    out.push_str("notifications:\n");
    for note in records::notifications() {
        let field = |k: &str| note[k].as_str().unwrap_or("-").to_owned();
        let _ = writeln!(
            out,
            "  {}  meeting {}  {}  {}",
            field("id"),
            field("meeting"),
            field("artifact"),
            field("status")
        );
    }
    out
}
