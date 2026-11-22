//! Several Slack workspaces served by one app (spec: platform-adapters,
//! Slack multiple workspaces): the bot tokens are the table's
//! `token_secret`, the secrets `options.bot_token_secrets` names and the
//! tokens of workspaces installed through OAuth (secrets named
//! `SLACK_BOT_TOKEN_<team id>`), de-duplicated by value. Each is checked
//! with `auth.test` at connect, which gives its workspace (team) and the
//! bot's user id there; one app-level token's Socket Mode connection
//! carries the events of every workspace, each routed by its team id, and
//! a reply goes out with the token of the workspace its channel belongs to
//! (learned from inbound events, else asked of each workspace).

use std::collections::HashMap;
use std::sync::Mutex;

use pmt_config::ProfileScope;
use pmt_config::schema::SecretName;
use pmt_gateway::messaging::adapter::credential;
use pmt_gateway::messaging::{AdapterError, SendError};
use serde_json::json;

use crate::api::{self, Api};

/// The prefix of the secrets holding OAuth-installed workspace tokens.
pub const OAUTH_SECRET_PREFIX: &str = "SLACK_BOT_TOKEN_";

/// One bot token and where it came from.
#[derive(Debug, Clone)]
pub(crate) struct Token {
    /// The secret holding it.
    pub(crate) secret: String,
    pub(crate) value: String,
    /// The team its secret name says it belongs to (OAuth installs).
    pub(crate) team: Option<String>,
}

/// Whether `name` is an OAuth-installed workspace's token secret; its
/// team id.
pub(crate) fn oauth_team(name: &str) -> Option<&str> {
    let team = name.strip_prefix(OAUTH_SECRET_PREFIX)?;
    let shaped = team.len() > 2
        && team.starts_with(['T', 'E'])
        && team
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    shaped.then_some(team)
}

/// Every bot token of the profile, the primary's first, each value once.
///
/// # Errors
///
/// `missing_secret` when a named secret is missing (the primary only when
/// no other token exists); `secrets_unreadable` when the store fails.
pub(crate) fn tokens(
    scope: &ProfileScope,
    primary: &str,
    listed: &[SecretName],
) -> Result<Vec<Token>, AdapterError> {
    let mut found: Vec<Token> = Vec::new();
    let mut add = |secret: &str, value: String, team: Option<&str>| {
        if !found.iter().any(|t| t.value == value) {
            found.push(Token {
                secret: secret.to_owned(),
                value,
                team: team.map(str::to_owned),
            });
        }
    };
    let first = credential(scope, primary);
    if let Ok(value) = &first {
        add(primary, value.clone(), None);
    }
    for name in listed {
        add(&name.0, credential(scope, &name.0)?, None);
    }
    let names = scope.secrets().store().names().map_err(|e| AdapterError {
        retryable: true,
        code: "secrets_unreadable".to_owned(),
        message: e.message().to_owned(),
    })?;
    for name in &names {
        if let Some(team) = oauth_team(name) {
            add(name, credential(scope, name)?, Some(team));
        }
    }
    match first {
        Err(err) if found.is_empty() => Err(err),
        _ => Ok(found),
    }
}

/// One workspace: its team, the bot's user id there and its client.
#[derive(Debug, Clone)]
pub(crate) struct Workspace {
    pub(crate) team: String,
    pub(crate) bot_user: String,
    pub(crate) api: Api,
}

/// A token `auth.test` refused, or could not check.
#[derive(Debug)]
pub(crate) struct Refused {
    pub(crate) token: Token,
    pub(crate) error: SendError,
}

impl Refused {
    /// The workspace it names: its team id when known, else its secret.
    pub(crate) fn label(&self) -> String {
        self.token.team.as_ref().map_or_else(
            || format!("secret {}", self.token.secret),
            |team| format!("team {team}"),
        )
    }
}

/// Checks each token with `auth.test`.
pub(crate) async fn verify(
    tokens: Vec<Token>,
    api_for: impl Fn(String) -> Api,
) -> (Vec<Workspace>, Vec<Refused>) {
    let mut ok = Vec::new();
    let mut refused = Vec::new();
    for token in tokens {
        let api = api_for(token.value.clone());
        match api.call("auth.test", &json!({})).await {
            Ok(me) => {
                let team = me["team_id"].as_str().unwrap_or_default().to_owned();
                if ok.iter().any(|w: &Workspace| w.team == team) {
                    continue;
                }
                ok.push(Workspace {
                    team,
                    bot_user: me["user_id"].as_str().unwrap_or_default().to_owned(),
                    api,
                });
            }
            Err(error) => refused.push(Refused { token, error }),
        }
    }
    (ok, refused)
}

/// The connected workspaces (the primary first) and which workspace owns
/// each channel seen.
#[derive(Debug)]
pub(crate) struct Workspaces {
    list: Vec<Workspace>,
    channels: Mutex<HashMap<String, String>>,
}

impl Workspaces {
    /// `list`, the primary first; `None` when empty.
    pub(crate) fn new(list: Vec<Workspace>) -> Option<Self> {
        (!list.is_empty()).then(|| Self {
            list,
            channels: Mutex::default(),
        })
    }

    /// The primary workspace (the table's token's, when it connected).
    pub(crate) fn primary(&self) -> &Workspace {
        &self.list[0]
    }

    /// Every workspace, the primary first.
    pub(crate) fn all(&self) -> &[Workspace] {
        &self.list
    }

    /// The workspace of `team`, else the primary.
    pub(crate) fn of_team(&self, team: Option<&str>) -> &Workspace {
        team.and_then(|t| self.list.iter().find(|w| w.team == t))
            .unwrap_or_else(|| self.primary())
    }

    /// `channel` belongs to `team` (seen in one of its events).
    pub(crate) fn learn(&self, channel: &str, team: &str) {
        if self.list.len() > 1 && self.list.iter().any(|w| w.team == team) {
            self.channels
                .lock()
                .expect("channels poisoned")
                .insert(channel.to_owned(), team.to_owned());
        }
    }

    /// The workspace `channel` belongs to: as learned, else the first
    /// whose `conversations.info` knows it, else the primary.
    pub(crate) async fn of_channel(&self, channel: &str) -> &Workspace {
        if self.list.len() == 1 {
            return self.primary();
        }
        let known = self
            .channels
            .lock()
            .expect("channels poisoned")
            .get(channel)
            .cloned();
        if let Some(team) = known {
            return self.of_team(Some(&team));
        }
        for workspace in &self.list {
            let info = workspace
                .api
                .call("conversations.info", &json!({"channel": channel}))
                .await;
            if info.is_ok() {
                self.learn(channel, &workspace.team);
                return workspace;
            }
        }
        self.primary()
    }
}

/// The error of a connect where no token passed `auth.test`.
pub(crate) fn none_connected(refused: &[Refused]) -> AdapterError {
    let unauthorized = refused.iter().all(|r| api::unauthorized(&r.error));
    let detail = refused
        .iter()
        .map(|r| format!("{}: {}", r.label(), r.error.describe()))
        .collect::<Vec<_>>()
        .join("; ");
    if unauthorized {
        crate::socket::lost(
            "unauthorized",
            format!("Slack rejected the bot token ({detail})"),
            false,
        )
    } else {
        crate::socket::lost("slack_unreachable", format!("Slack: {detail}"), true)
    }
}

/// The partial outage of a connect where some workspaces failed.
pub(crate) fn partial(refused: &[Refused]) -> AdapterError {
    let names = refused
        .iter()
        .map(|r| format!("{} ({})", r.label(), r.error.describe()))
        .collect::<Vec<_>>()
        .join(", ");
    AdapterError {
        retryable: true,
        code: pmt_gateway::messaging::adapter::PARTIAL_OUTAGE.to_owned(),
        message: format!("Slack workspaces not served: {names}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_secrets_are_named_by_team_id() {
        assert_eq!(oauth_team("SLACK_BOT_TOKEN_T0123ABC"), Some("T0123ABC"));
        assert_eq!(oauth_team("SLACK_BOT_TOKEN_E99"), Some("E99"));
        assert_eq!(oauth_team("SLACK_BOT_TOKEN_2"), None);
        assert_eq!(oauth_team("SLACK_BOT_TOKEN_t0123"), None);
        assert_eq!(oauth_team("SLACK_APP_TOKEN"), None);
    }
}
