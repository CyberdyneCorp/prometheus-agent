//! Local transcript and session commands (spec: cli, Local transcript and
//! session commands): `/history`, `/status`, `/version` and `/sessions`.
//! None of them calls a model.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use pmt_core::error::{Error, codes};
use pmt_store::{ListOptions, Message, Role, SearchOptions, Session, UsageScope, Visibility};
use serde_json::Value;

use super::Repl;
use crate::chat::session::Live;
use crate::slash::Surface;

/// Sessions `/sessions` lists.
const LISTED: usize = 10;

/// Characters of the latest prompt and reply `/status` shows.
const EXCERPT: usize = 200;

/// Who asks for `/sessions`: the surface and whether the user is an admin
/// there (always on the CLI, which belongs to its user).
#[derive(Debug, Clone, Copy)]
pub struct Caller {
    /// The surface.
    pub surface: Surface,
    /// An admin of the surface.
    pub admin: bool,
}

/// The `/sessions` refusal for a non-admin asking for every session.
pub const ALL_NEEDS_ADMIN: &str = "/sessions all requires an admin";

/// What `/sessions` arguments ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionsQuery {
    /// The surface scope (the workspace on the CLI).
    Here,
    /// Every session of the profile.
    All,
    /// Full-text search.
    Search(String),
}

/// Parses `/sessions` arguments, refusing `all` for a non-admin.
///
/// # Errors
///
/// `cli.usage` for unknown arguments; `slash.admin_only`
/// ([`ALL_NEEDS_ADMIN`]) for `all` without admin rights.
pub fn sessions_query(args: &str, caller: Caller) -> Result<SessionsQuery, Error> {
    let args = args.trim();
    let query = match args
        .split_once(' ')
        .map_or((args, ""), |(a, b)| (a, b.trim()))
    {
        ("", _) => SessionsQuery::Here,
        ("all", "") => SessionsQuery::All,
        ("search", q) if !q.is_empty() => SessionsQuery::Search(q.to_owned()),
        _ => {
            return Err(Error::new(
                &codes::CLI_USAGE,
                "usage: /sessions [all | search <query>]",
            ));
        }
    };
    if query == SessionsQuery::All && caller.surface == Surface::Gateway && !caller.admin {
        return Err(Error::new(&codes::SLASH_ADMIN_ONLY, ALL_NEEDS_ADMIN));
    }
    Ok(query)
}

impl Repl {
    /// `/history [N]`: the last N (default all) active user and assistant
    /// messages.
    pub(super) fn history(&self, live: &Live, args: &str) -> Result<String, Error> {
        let limit = match args.trim() {
            "" => usize::MAX,
            n => n.parse::<usize>().ok().filter(|n| *n > 0).ok_or_else(|| {
                Error::new(&codes::CLI_USAGE, "usage: /history [N] (a positive number)")
            })?,
        };
        let messages: Vec<Message> = self
            .factory
            .store
            .messages(live.id, Visibility::Active)?
            .into_iter()
            .filter(|m| matches!(m.role, Role::User | Role::Assistant) && !authored(m))
            .filter(|m| !text_of(m).trim().is_empty())
            .collect();
        if messages.is_empty() {
            return Ok("no messages yet".to_owned());
        }
        let format = &self.factory.scope.config().display.timestamp_format;
        let skip = messages.len().saturating_sub(limit);
        let mut text = String::new();
        for message in &messages[skip..] {
            if !text.is_empty() {
                text.push('\n');
            }
            if self.display.timestamps {
                let local = message.created_at.to_zoned(jiff::tz::TimeZone::system());
                let _ = write!(text, "[{}] ", local.strftime(format.as_str()));
            }
            let who = if message.role == Role::User {
                "you"
            } else {
                "assistant"
            };
            let _ = write!(text, "{who}: {}", text_of(message).trim());
        }
        Ok(text)
    }

    /// `/status`: the session's details and a recap, from the store only.
    pub(super) fn status(&self, live: &Live) -> Result<String, Error> {
        let store = &self.factory.store;
        let session = store.session(live.id)?;
        let messages = store.messages(live.id, Visibility::Active)?;
        let totals = store.usage_totals(UsageScope::Session(live.id))?;
        let target = live.model.target();
        let mut text = String::new();
        let mut field = |name: &str, value: &dyn std::fmt::Display| {
            let _ = writeln!(text, "{name:<14}{value}");
        };
        field("model", &target.model.as_deref().unwrap_or("default"));
        field("provider", &target.provider);
        field("profile", &self.factory.scope.name());
        field("session", &session.id);
        field("title", &session.title.as_deref().unwrap_or("-"));
        field("cwd", &session.cwd.as_deref().unwrap_or("-"));
        field("started", &minute(session.started_at));
        field("last activity", &minute(session.last_activity_at));
        let t = &totals.tokens;
        field(
            "tokens",
            &format!(
                "input {} output {} cache read {} cache write {} reasoning {}",
                t.input, t.output, t.cache_read, t.cache_write, t.reasoning
            ),
        );
        field("cost", &pmt_obs::usage::format_total_cost(&totals));
        field("state", &if self.running { "running" } else { "idle" });
        text.push_str(&recap(&messages));
        Ok(text.trim_end().to_owned())
    }

    /// `/sessions [all] [search <query>]`.
    pub(super) fn sessions(&self, live: &Live, args: &str) -> Result<String, Error> {
        let caller = Caller {
            surface: Surface::Cli,
            admin: true,
        };
        let store = &self.factory.store;
        let (heading, sessions) = match sessions_query(args, caller)? {
            SessionsQuery::Here => {
                let cwd = self.factory.options.cwd.to_string_lossy().into_owned();
                let here = store
                    .list_sessions(&ListOptions::default())?
                    .into_iter()
                    .filter(|s| s.cwd.as_deref() == Some(cwd.as_str()))
                    .take(LISTED)
                    .collect();
                ("sessions in this directory", here)
            }
            SessionsQuery::All => {
                let all = store.list_sessions(&ListOptions {
                    limit: Some(u32::try_from(LISTED).unwrap_or(10)),
                    ..ListOptions::default()
                })?;
                ("sessions of this profile", all)
            }
            SessionsQuery::Search(query) => {
                let options = SearchOptions {
                    limit: 50,
                    ..SearchOptions::default()
                };
                let mut ids = Vec::new();
                for hit in store.search(&query, &options)?.hits {
                    if !ids.contains(&hit.session_id) {
                        ids.push(hit.session_id);
                    }
                }
                let found = ids
                    .into_iter()
                    .take(LISTED)
                    .filter_map(|id| store.session(id).ok())
                    .collect();
                ("sessions matching the search", found)
            }
        };
        Ok(listing(heading, &sessions, live))
    }
}

fn listing(heading: &str, sessions: &[Session], live: &Live) -> String {
    if sessions.is_empty() {
        return format!("{heading}: none");
    }
    let mut text = format!("{heading} (/resume <id or title>):");
    for session in sessions {
        let current = if session.id == live.id {
            " (current)"
        } else {
            ""
        };
        let _ = write!(
            text,
            "\n  {}  {}  {}{current}",
            session.id,
            minute(session.last_activity_at),
            session.title.as_deref().unwrap_or("(untitled)")
        );
    }
    text
}

fn minute(ts: jiff::Timestamp) -> String {
    ts.to_zoned(jiff::tz::TimeZone::system())
        .strftime("%Y-%m-%d %H:%M")
        .to_string()
}

/// The recap block: turns, top tools, files touched, latest exchange.
fn recap(messages: &[Message]) -> String {
    let users: Vec<&Message> = messages
        .iter()
        .filter(|m| m.role == Role::User && !authored(m))
        .collect();
    let mut tools: BTreeMap<String, usize> = BTreeMap::new();
    let mut files = BTreeSet::new();
    for message in messages {
        for call in calls(message) {
            let name = call.get("name").and_then(Value::as_str).unwrap_or_default();
            *tools.entry(name.to_owned()).or_default() += 1;
            files.extend(paths(call));
        }
    }
    let mut ranked: Vec<(String, usize)> = tools.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let top: Vec<String> = ranked
        .iter()
        .take(5)
        .map(|(name, n)| format!("{name} ×{n}"))
        .collect();
    let latest_reply = messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant && !text_of(m).trim().is_empty());
    let mut text = String::from("recap\n");
    let _ = writeln!(text, "  turns         {}", users.len());
    let _ = writeln!(text, "  top tools     {}", or_none(&top.join(", ")));
    let files: Vec<String> = files.into_iter().collect();
    let _ = writeln!(text, "  files touched {}", or_none(&files.join(", ")));
    let prompt = users.last().map(|m| text_of(m)).unwrap_or_default();
    let reply = latest_reply.map(text_of).unwrap_or_default();
    let _ = writeln!(text, "  last prompt   {}", or_none(&excerpt(&prompt)));
    let _ = writeln!(text, "  last reply    {}", or_none(&excerpt(&reply)));
    text
}

fn or_none(text: &str) -> &str {
    if text.is_empty() { "-" } else { text }
}

/// `text` on one line, cut to [`EXCERPT`] characters.
fn excerpt(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= EXCERPT {
        return line;
    }
    let mut cut: String = line.chars().take(EXCERPT).collect();
    cut.push('…');
    cut
}

/// The tool calls of an assistant message.
fn calls(message: &Message) -> Vec<&Value> {
    message
        .tool_calls
        .as_ref()
        .and_then(Value::as_array)
        .map(|calls| calls.iter().collect())
        .unwrap_or_default()
}

/// File paths a tool call names (`path`, `file_path`, `paths`).
fn paths(call: &Value) -> Vec<String> {
    let arguments: Value = call
        .get("arguments")
        .and_then(Value::as_str)
        .and_then(|a| serde_json::from_str(a).ok())
        .unwrap_or(Value::Null);
    let mut found = Vec::new();
    for key in ["path", "file_path"] {
        if let Some(path) = arguments.get(key).and_then(Value::as_str) {
            found.push(path.to_owned());
        }
    }
    if let Some(list) = arguments.get("paths").and_then(Value::as_array) {
        found.extend(list.iter().filter_map(Value::as_str).map(str::to_owned));
    }
    found
}

/// What the user typed (without request-time additions), or the text.
fn text_of(message: &Message) -> String {
    message
        .metadata
        .as_ref()
        .and_then(|m| m.get("user_text"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| message.content.clone())
        .unwrap_or_default()
}

fn authored(message: &Message) -> bool {
    message
        .metadata
        .as_ref()
        .and_then(|m| m.get(pmt_agent::turn::transcript::AUTHORED))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_sessions_need_an_admin_on_messaging_only() {
        let member = Caller {
            surface: Surface::Gateway,
            admin: false,
        };
        let err = sessions_query("all", member).unwrap_err();
        assert_eq!(err.message(), ALL_NEEDS_ADMIN);
        assert_eq!(sessions_query("", member).unwrap(), SessionsQuery::Here);
        let admin = Caller {
            admin: true,
            ..member
        };
        assert_eq!(sessions_query("all", admin).unwrap(), SessionsQuery::All);
        let cli = Caller {
            surface: Surface::Cli,
            admin: true,
        };
        assert_eq!(
            sessions_query("search deploy plan", cli).unwrap(),
            SessionsQuery::Search("deploy plan".into())
        );
        assert!(sessions_query("bogus", cli).is_err());
    }

    #[test]
    fn excerpts_are_one_line_and_capped() {
        assert_eq!(excerpt("a\n b"), "a b");
        assert_eq!(excerpt(&"x".repeat(300)).chars().count(), EXCERPT + 1);
    }
}
