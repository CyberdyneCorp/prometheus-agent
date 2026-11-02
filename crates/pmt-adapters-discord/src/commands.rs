//! Application (slash) commands (spec: platform-adapters, Native command
//! menus and slash registration): the gateway's commands and skills, each
//! with an optional `args` string, overwrite the application's commands up
//! to Discord's limit of 100; extras are skipped with one WARN naming
//! them. A follower daemon (`slash_commands = false`) neither registers
//! nor removes anything.

use pmt_gateway::messaging::{MenuCommand, SendError};
use reqwest::Method;
use serde_json::{Value, json};

use crate::api::{Api, Body};

/// Commands Discord keeps per application.
pub(crate) const LIMIT: usize = 100;

/// A command name as Discord accepts it (lower case, letters, digits,
/// `-` and `_`, at most 32); `None` when nothing is left.
fn discord_name(name: &str) -> Option<String> {
    let name: String = name
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .take(32)
        .collect();
    (!name.is_empty()).then_some(name)
}

/// The command bodies to register and the names that did not fit.
pub(crate) fn bodies(all: &[MenuCommand]) -> (Vec<Value>, Vec<String>) {
    let mut seen = std::collections::BTreeSet::new();
    let mut bodies = Vec::new();
    let mut skipped = Vec::new();
    for command in all {
        let Some(name) = discord_name(&command.name) else {
            continue;
        };
        if !seen.insert(name.clone()) {
            continue;
        }
        if bodies.len() == LIMIT {
            skipped.push(name);
            continue;
        }
        let description: String = if command.description.trim().is_empty() {
            format!("/{name}")
        } else {
            command.description.chars().take(100).collect()
        };
        bodies.push(json!({
            "name": name, "type": 1, "description": description,
            "options": [{"type": 3, "name": "args", "description": "arguments", "required": false}]
        }));
    }
    (bodies, skipped)
}

/// Overwrites the application's commands with `all` (capped).
pub(crate) async fn register(
    api: &Api,
    application: &str,
    all: &[MenuCommand],
) -> Result<(), SendError> {
    let (bodies, skipped) = bodies(all);
    if !skipped.is_empty() {
        tracing::warn!(
            platform = "discord",
            "Discord keeps at most {LIMIT} application commands; not registered: {}",
            skipped.join(", ")
        );
    }
    let path = format!("/applications/{application}/commands");
    api.call(Method::PUT, &path, Body::Json(json!(bodies)))
        .await
        .map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extras_beyond_the_limit_are_named() {
        let all: Vec<MenuCommand> = (0..105)
            .map(|i| MenuCommand {
                name: format!("Skill_{i}"),
                description: String::new(),
                takes_args: false,
            })
            .collect();
        let (bodies, skipped) = bodies(&all);
        assert_eq!(bodies.len(), LIMIT);
        assert_eq!(bodies[0]["name"], "skill_0");
        assert_eq!(bodies[0]["options"][0]["name"], "args");
        assert_eq!(
            skipped,
            [
                "skill_100",
                "skill_101",
                "skill_102",
                "skill_103",
                "skill_104"
            ]
        );
    }
}
