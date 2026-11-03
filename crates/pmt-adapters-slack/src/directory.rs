//! What the gateway asks Slack about people and conversations (spec:
//! messaging-gateway, Role-based access grants; Channel history backfill):
//! user groups are Slack's roles (ids `S…`, from `usergroups.list` with
//! their members), and a conversation's or thread's recent messages come
//! from `conversations.history` / `conversations.replies`.

use pmt_gateway::messaging::{HistoryEntry, SendError};
use serde_json::{Value, json};

use crate::api::Api;

/// The workspace's user groups with their members.
async fn usergroups(api: &Api, team: Option<&str>) -> Result<Vec<Value>, SendError> {
    let mut args = json!({"include_users": true, "include_disabled": false});
    if let Some(team) = team {
        args["team_id"] = json!(team);
    }
    let listed = api.call("usergroups.list", &args).await?;
    Ok(listed["usergroups"].as_array().cloned().unwrap_or_default())
}

/// The ids of the user groups `user` belongs to; an error (a missing
/// `usergroups:read` scope, a network failure) denies the grant upstream.
pub(crate) async fn member_groups(
    api: &Api,
    team: Option<&str>,
    user: &str,
) -> Result<Vec<String>, SendError> {
    Ok(usergroups(api, team)
        .await?
        .iter()
        .filter(|g| {
            g["users"]
                .as_array()
                .is_some_and(|users| users.iter().any(|u| u == user))
        })
        .filter_map(|g| g["id"].as_str().map(str::to_owned))
        .collect())
}

/// The id of the user group whose handle (`@oncall` or `oncall`) or name
/// is `name`, ignoring case.
pub(crate) async fn group_id(api: &Api, name: &str) -> Option<String> {
    let wanted = name.trim_start_matches('@');
    usergroups(api, None)
        .await
        .ok()?
        .iter()
        .find(|g| {
            ["handle", "name"].iter().any(|field| {
                g[field]
                    .as_str()
                    .is_some_and(|v| v.eq_ignore_ascii_case(wanted))
            })
        })
        .and_then(|g| g["id"].as_str().map(str::to_owned))
}

/// The latest `limit` messages of a channel, or of a thread (its root
/// included), oldest first.
pub(crate) async fn history(
    api: &Api,
    bot_user: &str,
    channel: &str,
    thread: Option<&str>,
    limit: usize,
) -> Result<Vec<HistoryEntry>, SendError> {
    let limit = limit.clamp(1, 1000);
    // Replies come oldest first and start with the root (the newest
    // `limit` are the tail); history comes newest first.
    let mut messages = if let Some(ts) = thread {
        let found = api
            .call(
                "conversations.replies",
                &json!({"channel": channel, "ts": ts, "limit": 1000}),
            )
            .await?;
        let all = found["messages"].as_array().cloned().unwrap_or_default();
        let skip = all.len().saturating_sub(limit);
        all.into_iter().skip(skip).collect::<Vec<_>>()
    } else {
        let found = api
            .call(
                "conversations.history",
                &json!({"channel": channel, "limit": limit}),
            )
            .await?;
        let mut all = found["messages"].as_array().cloned().unwrap_or_default();
        all.reverse();
        all
    };
    messages.retain(|m| m["subtype"].as_str().is_none_or(|s| s == "bot_message"));
    Ok(messages.iter().filter_map(|m| entry(m, bot_user)).collect())
}

fn entry(message: &Value, bot_user: &str) -> Option<HistoryEntry> {
    let author = message["user"]
        .as_str()
        .or_else(|| message["bot_id"].as_str())?
        .to_owned();
    let name = message["user_profile"]["display_name"]
        .as_str()
        .filter(|n| !n.is_empty())
        .or_else(|| message["username"].as_str())
        .unwrap_or(&author)
        .to_owned();
    Some(HistoryEntry {
        message_id: message["ts"].as_str()?.to_owned(),
        from_self: !bot_user.is_empty() && author == bot_user,
        author_id: author,
        author_name: name,
        text: message["text"].as_str().unwrap_or_default().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_history_entry_names_its_author_and_knows_the_bot() {
        let own = json!({"ts": "1.0", "user": "UBOT", "text": "done"});
        let theirs = json!({"ts": "2.0", "user": "U1", "text": "hi",
            "user_profile": {"display_name": "ana"}});
        let own = entry(&own, "UBOT").unwrap();
        assert!(own.from_self);
        let theirs = entry(&theirs, "UBOT").unwrap();
        assert_eq!(
            (theirs.author_name.as_str(), theirs.from_self),
            ("ana", false)
        );
    }
}
