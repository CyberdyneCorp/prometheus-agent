//! Sends through the Bot Connector of each conversation's service URL
//! (learned from its activities and kept in profile state for proactive
//! sends, `options.service_url` before any): Markdown text, Adaptive Cards
//! for choices (one `Action.Execute` per choice; approvals labelled Allow
//! Once, Allow Session, Always Allow and Deny; the card's fallback text,
//! shown where a card cannot be, numbers the choices with the gateway's
//! own labels, and a numbered answer picks one), edits that
//! replace a card in place (the decision, without its actions), deletes
//! and typing activities.

use adapter_kit::message::{Button, Outgoing};
use adapter_kit::{host, net};
use serde_json::{Value, json};

use crate::{Choices, Teams};

/// Bot message ids remembered (a reply to one addresses the bot).
const SENT_KEPT: usize = 500;
const CARD_TYPE: &str = "application/vnd.microsoft.card.adaptive";

/// The label of an approval choice (`appr:<id>:<choice>:<mac>`).
fn approval_label(data: &str) -> Option<&'static str> {
    if !data.starts_with("appr:") {
        return None;
    }
    let choice = data.rsplit(':').nth(1)?;
    Some(match choice {
        "once" => "Allow Once",
        "session" => "Allow Session",
        "always" => "Always Allow",
        "deny" => "Deny",
        _ => return None,
    })
}

fn label(button: &Button) -> String {
    approval_label(&button.data).map_or_else(|| button.label.clone(), str::to_owned)
}

fn numbered(text: &str, buttons: &[Button]) -> String {
    let lines: Vec<String> = buttons
        .iter()
        .enumerate()
        .map(|(i, b)| format!("{}. {}", i + 1, b.label))
        .collect();
    if lines.is_empty() {
        text.to_owned()
    } else {
        format!("{text}\n\n{}", lines.join("\n"))
    }
}

/// An Adaptive Card of `text` with one action per button.
pub(crate) fn card(text: &str, buttons: &[Button]) -> Value {
    let actions: Vec<Value> = buttons
        .iter()
        .map(|b| {
            json!({
                "type": "Action.Execute",
                "title": label(b),
                "verb": "prometheus",
                "data": {"prometheus": b.data},
            })
        })
        .collect();
    json!({
        "type": "AdaptiveCard",
        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
        "version": "1.4",
        "body": [{"type": "TextBlock", "text": text, "wrap": true}],
        "actions": actions,
        "fallbackText": numbered(text, buttons),
    })
}

fn message(out: &Outgoing, card_like: bool) -> Value {
    let mut activity = if card_like || !out.buttons.is_empty() {
        json!({
            "type": "message",
            "attachments": [{"contentType": CARD_TYPE, "content": card(&out.text, &out.buttons)}],
        })
    } else {
        json!({"type": "message", "text": out.text, "textFormat": "markdown"})
    };
    if let Some(reply_to) = &out.reply_to {
        activity["replyToId"] = json!(reply_to);
    }
    activity
}

impl Teams {
    pub(crate) fn remember_service(&mut self, chat: &str, url: &str) {
        let url = url.trim_end_matches('/').to_owned();
        if self.services.get(chat) != Some(&url) {
            host::store(&format!("svc:{chat}"), &json!(url));
            self.services.insert(chat.to_owned(), url);
        }
    }

    fn service(&self, chat: &str) -> String {
        self.services
            .get(chat)
            .cloned()
            .or_else(|| {
                host::load(&format!("svc:{chat}")).and_then(|v| v.as_str().map(str::to_owned))
            })
            .unwrap_or_else(|| self.default_service.clone())
    }

    /// Whether `url` is on a known Bot Connector service (where the bot
    /// token may go).
    pub(crate) fn is_service_url(&self, url: &str) -> bool {
        let mut known = self
            .services
            .values()
            .chain(std::iter::once(&self.default_service));
        known.any(|s| url.starts_with(&format!("{s}/")))
    }

    fn activities_url(&self, chat: &str) -> String {
        format!(
            "{}/v3/conversations/{}/activities",
            self.service(chat),
            adapter_kit::text::percent_encode(chat)
        )
    }

    fn call(&mut self, method: &str, url: &str, body: Option<&Value>) -> Result<Value, String> {
        let bearer = self.auth.bearer()?;
        let mut headers = vec![("authorization", bearer.as_str())];
        if body.is_some() {
            headers.push(("content-type", "application/json"));
        }
        let response = net::send(
            method,
            url,
            &headers,
            body.map(|b| b.to_string().into_bytes()),
        )?;
        net::classify(&response)?;
        Ok(response.json())
    }

    /// Sends `out`; its activity id.
    pub(crate) fn send(&mut self, out: &Outgoing) -> Result<String, String> {
        let url = self.activities_url(&out.chat);
        let answer = self.call("POST", &url, Some(&message(out, false)))?;
        let id = answer["id"].as_str().unwrap_or_default().to_owned();
        self.sent.push(id.clone());
        if self.sent.len() > SENT_KEPT {
            self.sent.remove(0);
        }
        if !out.buttons.is_empty() {
            self.cards.push(id.clone());
            self.choices.retain(|c| c.chat != out.chat);
            self.choices.push(Choices {
                chat: out.chat.clone(),
                data: out.buttons.iter().map(|b| b.data.clone()).collect(),
            });
        }
        Ok(id)
    }

    /// Replaces activity `out.message_id` (a card stays a card: an
    /// approval's outcome shows without its actions).
    pub(crate) fn edit(&mut self, out: &Outgoing) -> Result<(), String> {
        let id = out.message_id.clone().ok_or("permanent: no message id")?;
        let was_card = self.cards.contains(&id);
        let mut activity = message(out, was_card);
        activity["id"] = json!(id);
        let url = format!("{}/{}", self.activities_url(&out.chat), id);
        self.call("PUT", &url, Some(&activity)).map(|_| ())
    }

    pub(crate) fn delete(&mut self, out: &Outgoing) -> Result<(), String> {
        let id = out.message_id.clone().ok_or("permanent: no message id")?;
        let url = format!("{}/{}", self.activities_url(&out.chat), id);
        self.call("DELETE", &url, None).map(|_| ())
    }

    pub(crate) fn typing(&mut self, chat: &str) -> Result<(), String> {
        let url = self.activities_url(chat);
        self.call("POST", &url, Some(&json!({"type": "typing"})))
            .map(|_| ())
    }

    /// The choice a numbered answer in `chat` picks, if one is pending.
    pub(crate) fn numbered_answer(&mut self, chat: &str, text: &str) -> Option<String> {
        let n = text.trim().parse::<usize>().ok().filter(|n| *n >= 1)?;
        let pending = self.choices.iter().rposition(|c| c.chat == chat)?;
        let data = self.choices[pending].data.get(n - 1).cloned()?;
        self.choices.remove(pending);
        Some(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_cards_use_the_four_labels() {
        let buttons: Vec<Button> = ["once", "session", "always", "deny"]
            .iter()
            .map(|c| Button {
                label: (*c).to_owned(),
                data: format!("appr:r1:{c}:mac"),
            })
            .collect();
        let card = card("Approval needed", &buttons);
        let titles: Vec<&str> = card["actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["title"].as_str().unwrap())
            .collect();
        assert_eq!(
            titles,
            ["Allow Once", "Allow Session", "Always Allow", "Deny"]
        );
        assert_eq!(card["actions"][3]["data"]["prometheus"], "appr:r1:deny:mac");
        assert!(card["fallbackText"].as_str().unwrap().contains("4. deny"));
    }
}
