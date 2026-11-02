//! The bot's command menu and inline picker (spec: platform-adapters,
//! Native command menus and slash registration): `setMyCommands` with at
//! most `options.command_menu.max_commands` entries after placing the
//! priority ones, and an inline query (`@bot <term>`) that searches every
//! command and skill without the cap, answering nothing to anyone the
//! gateway would not admit.

use std::sync::{Arc, Mutex};

use pmt_config::platforms::TelegramOptions;
use pmt_gateway::messaging::menu;
use pmt_gateway::messaging::{AdapterLink, MenuCommand, SendError};
use serde_json::{Value, json};

use crate::api::Api;

/// Results Telegram takes in one inline answer.
const PAGE: usize = 50;

/// A command name as Telegram accepts it (`a-z`, `0-9`, `_`, at most 32);
/// `None` when nothing is left.
fn telegram_name(name: &str) -> Option<String> {
    let name: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c == '-' { '_' } else { c })
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '_')
        .take(32)
        .collect();
    (!name.is_empty()).then_some(name)
}

fn description(command: &MenuCommand) -> String {
    let text = if command.description.trim().is_empty() {
        format!("/{}", command.name)
    } else {
        command.description.clone()
    };
    text.chars().take(256).collect()
}

/// The commands under Telegram's naming rules (duplicates dropped).
pub(crate) fn sanitized(all: &[MenuCommand]) -> Vec<MenuCommand> {
    let mut seen = std::collections::BTreeSet::new();
    all.iter()
        .filter_map(|c| {
            let name = telegram_name(&c.name)?;
            seen.insert(name.clone()).then(|| MenuCommand {
                name,
                description: description(c),
                takes_args: c.takes_args,
            })
        })
        .collect()
}

/// Registers the menu; a follower (`slash_commands = false`) registers
/// and removes nothing.
pub(crate) async fn register(
    api: &Api,
    options: &TelegramOptions,
    all: &[MenuCommand],
) -> Result<(), SendError> {
    if !options.slash_commands {
        return Ok(());
    }
    let entries: Vec<Value> = menu::select(&sanitized(all), &options.command_menu)
        .iter()
        .map(|c| json!({"command": c.name, "description": c.description}))
        .collect();
    api.call("setMyCommands", &json!({"commands": entries}))
        .await
        .map(drop)
}

/// Answers an inline query: matching commands and skills, one page at a
/// time; nothing for a sender the gateway would not admit.
pub(crate) async fn answer_inline(
    api: &Api,
    link: &dyn AdapterLink,
    commands: &Arc<Mutex<Vec<MenuCommand>>>,
    query: &Value,
) {
    let Some(id) = query["id"].as_str() else {
        return;
    };
    let sender = crate::normalize::id(&query["from"]["id"]).unwrap_or_default();
    let results = if link.authorized(&sender).await {
        let all = commands.lock().expect("commands poisoned").clone();
        let offset: usize = query["offset"]
            .as_str()
            .and_then(|o| o.parse().ok())
            .unwrap_or(0);
        let found = menu::search(&all, query["query"].as_str().unwrap_or_default());
        let next = (offset + PAGE < found.len()).then(|| (offset + PAGE).to_string());
        let page: Vec<Value> = found
            .iter()
            .skip(offset)
            .take(PAGE)
            .map(|c| {
                json!({"type": "article", "id": c.name, "title": format!("/{}", c.name),
                       "description": c.description,
                       "input_message_content": {"message_text": format!("/{}", c.name)}})
            })
            .collect();
        (page, next)
    } else {
        (Vec::new(), None)
    };
    let mut body = json!({"inline_query_id": id, "results": results.0, "cache_time": 0,
                          "is_personal": true});
    if let Some(next) = results.1 {
        body["next_offset"] = json!(next);
    }
    if let Err(err) = api.call("answerInlineQuery", &body).await {
        tracing::debug!("inline query not answered: {}", err.describe());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_follow_telegram_rules() {
        assert_eq!(telegram_name("set-home").as_deref(), Some("set_home"));
        assert_eq!(telegram_name("Model").as_deref(), Some("model"));
        assert_eq!(telegram_name("!!!"), None);
        let long = "x".repeat(40);
        assert_eq!(telegram_name(&long).map(|n| n.len()), Some(32));
    }
}
