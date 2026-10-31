//! The `session_search` tool (spec: session-storage, Session search tool).
//!
//! Searches the calling session's own profile store (other profiles are
//! other stores, so they are never visible), leaving out the messages the
//! calling session already has in context. Two modes: a search returning
//! snippets grouped by session (plus a per-session summary from the
//! `session_search` auxiliary route in `adaptive` detail when more than 3
//! sessions match), and a window of active messages around one hit. The
//! result is capped at [`RESULT_CAP`] bytes.

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use jiff::Timestamp;
use jiff::civil::Date;
use pmt_core::id::{MessageId, SessionId};
use pmt_store::{Message, Role, SearchOptions, SearchSort, Store, Visibility};
use pmt_tools::ToolResult;
use pmt_tools::agent_level::{
    SEARCH_DEFAULT_LIMIT, SEARCH_DEFAULT_WINDOW, SEARCH_MAX_LIMIT, SEARCH_MAX_WINDOW,
};
use serde::Deserialize;
use serde_json::{Value, json};

/// Most bytes of one result.
pub const RESULT_CAP: usize = 16 * 1024;
/// Sessions matching before `adaptive` detail adds summaries.
const SUMMARY_THRESHOLD: usize = 3;
/// Characters of one message in window mode before it is shortened.
const WINDOW_MESSAGE_CHARS: usize = 2_000;

/// What a summary call returns.
pub type SummaryFuture<'a> = Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>>;

/// Summarizes one session's hits for a query (the turn loop binds it to
/// the `session_search` auxiliary task; `None` when that call fails).
pub trait Summarizer: Send + Sync {
    /// A short summary of what `session` says about `query`.
    fn summarize<'a>(
        &'a self,
        query: &'a str,
        session: SessionId,
        snippets: &'a [String],
    ) -> SummaryFuture<'a>;
}

/// `session_search` for one session.
pub struct SessionSearch {
    store: Arc<Store>,
    session: SessionId,
    summarizer: Option<Arc<dyn Summarizer>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    query: Option<String>,
    limit: Option<u64>,
    sort: Option<String>,
    after: Option<String>,
    before: Option<String>,
    #[serde(default)]
    role_filter: Vec<String>,
    #[serde(default)]
    exclude_session_ids: Vec<String>,
    detail: Option<String>,
    session_id: Option<String>,
    around_message_id: Option<String>,
    window: Option<u64>,
}

impl SessionSearch {
    /// The tool of `session`, searching `store` (the session's profile).
    #[must_use]
    pub fn new(store: Arc<Store>, session: SessionId) -> Self {
        Self {
            store,
            session,
            summarizer: None,
        }
    }

    /// Adds the summarizer used by `adaptive` detail.
    #[must_use]
    pub fn with_summarizer(mut self, summarizer: Arc<dyn Summarizer>) -> Self {
        self.summarizer = Some(summarizer);
        self
    }

    /// Runs one call.
    pub async fn call(&self, arguments: Value) -> ToolResult {
        let args: Args = match serde_json::from_value(arguments) {
            Ok(args) => args,
            Err(err) => return ToolResult::error(format!("invalid session_search call: {err}")),
        };
        let outcome = match (&args.session_id, &args.around_message_id) {
            (Some(session), Some(message)) => self.window(session, message, args.window),
            (None, None) => self.search(&args).await,
            _ => Err("window mode needs both session_id and around_message_id".to_owned()),
        };
        match outcome {
            Ok(reply) => ToolResult::ok(capped(reply)),
            Err(message) => ToolResult::error(message),
        }
    }

    async fn search(&self, args: &Args) -> Result<Value, String> {
        let query = args
            .query
            .as_deref()
            .filter(|q| !q.trim().is_empty())
            .ok_or("query is required")?;
        let options = Self::options(args)?;
        let in_context = self.in_context()?;
        let results = self
            .store
            .search(query, &options)
            .map_err(|err| err.message().to_owned())?;
        let hits: Vec<_> = results
            .hits
            .into_iter()
            .filter(|hit| !in_context.contains(&hit.message_id))
            .collect();
        let mut sessions: Vec<(SessionId, Option<String>, Vec<Value>)> = Vec::new();
        for hit in hits {
            let entry = json!({
                "message_id": hit.message_id.to_string(),
                "role": hit.role.as_str(),
                "created_at": hit.created_at.to_string(),
                "snippet": hit.snippet,
            });
            match sessions.iter_mut().find(|(id, ..)| *id == hit.session_id) {
                Some((.., list)) => list.push(entry),
                None => sessions.push((hit.session_id, hit.session_title, vec![entry])),
            }
        }
        let summarize = args.detail.as_deref() != Some("full")
            && sessions.len() > SUMMARY_THRESHOLD
            && self.summarizer.is_some();
        let mut groups = Vec::with_capacity(sessions.len());
        for (id, title, list) in sessions {
            let mut group = json!({"session_id": id.to_string(), "title": title, "hits": list});
            if summarize {
                group["summary"] = json!(self.summary(query, id, &group["hits"]).await);
            }
            groups.push(group);
        }
        Ok(json!({"query": query, "sessions": groups}))
    }

    async fn summary(&self, query: &str, session: SessionId, hits: &Value) -> Option<String> {
        let snippets: Vec<String> = hits
            .as_array()?
            .iter()
            .filter_map(|h| h["snippet"].as_str().map(str::to_owned))
            .collect();
        self.summarizer
            .as_ref()?
            .summarize(query, session, &snippets)
            .await
    }

    fn options(args: &Args) -> Result<SearchOptions, String> {
        let limit = args
            .limit
            .unwrap_or(SEARCH_DEFAULT_LIMIT)
            .clamp(1, SEARCH_MAX_LIMIT);
        let sort = match args.sort.as_deref().unwrap_or("relevance") {
            "relevance" => SearchSort::Rank,
            "newest" => SearchSort::Newest,
            "oldest" => SearchSort::Oldest,
            other => {
                return Err(format!(
                    "unknown sort '{other}' (relevance, newest, oldest)"
                ));
            }
        };
        let roles = args
            .role_filter
            .iter()
            .map(|r| match r.as_str() {
                "user" | "assistant" | "tool" => {
                    r.parse::<Role>().map_err(|e| e.message().to_owned())
                }
                other => Err(format!("unknown role '{other}' (user, assistant, tool)")),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let exclude_session_ids = args
            .exclude_session_ids
            .iter()
            .map(|s| s.parse::<SessionId>().map_err(|e| e.message().to_owned()))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(SearchOptions {
            limit: u32::try_from(limit).unwrap_or(u32::MAX),
            sort,
            roles,
            after: args.after.as_deref().map(time).transpose()?,
            before: args.before.as_deref().map(time).transpose()?,
            exclude_session_ids,
            ..SearchOptions::default()
        })
    }

    /// The calling session's messages the model already sees (active and
    /// not replaced by a compaction summary).
    fn in_context(&self) -> Result<BTreeSet<MessageId>, String> {
        let messages = self
            .store
            .messages(self.session, Visibility::Active)
            .or_else(|err| {
                if err.code().code == "session.not_found" {
                    Ok(Vec::new())
                } else {
                    Err(err)
                }
            })
            .map_err(|err| err.message().to_owned())?;
        Ok(messages
            .into_iter()
            .filter(|m| !m.compacted)
            .map(|m| m.id)
            .collect())
    }

    /// The active messages within `window` of `message` in `session`.
    fn window(&self, session: &str, message: &str, window: Option<u64>) -> Result<Value, String> {
        let session: SessionId = session
            .parse()
            .map_err(|e: pmt_core::Error| e.message().to_owned())?;
        let message: MessageId = message
            .parse()
            .map_err(|e: pmt_core::Error| e.message().to_owned())?;
        let span = usize::try_from(
            window
                .unwrap_or(SEARCH_DEFAULT_WINDOW)
                .min(SEARCH_MAX_WINDOW),
        )
        .unwrap_or(0);
        let messages = self
            .store
            .messages(session, Visibility::Active)
            .map_err(|err| err.message().to_owned())?;
        let at = messages
            .iter()
            .position(|m| m.id == message)
            .ok_or_else(|| format!("{message} is not an active message of {session}"))?;
        let around = &messages[at.saturating_sub(span)..(at + span + 1).min(messages.len())];
        let entries: Vec<Value> = around
            .iter()
            .map(|m| window_entry(m, m.id == message))
            .collect();
        Ok(
            json!({"session_id": session.to_string(), "around_message_id": message.to_string(), "messages": entries}),
        )
    }
}

fn window_entry(message: &Message, hit: bool) -> Value {
    let text = message.content.as_deref().unwrap_or_default();
    let content = if text.chars().count() > WINDOW_MESSAGE_CHARS {
        let cut: String = text.chars().take(WINDOW_MESSAGE_CHARS).collect();
        format!("{cut} [... shortened]")
    } else {
        text.to_owned()
    };
    json!({
        "message_id": message.id.to_string(),
        "role": message.role.as_str(),
        "created_at": message.created_at.to_string(),
        "content": content,
        "hit": hit,
    })
}

/// An RFC 3339 time or a `YYYY-MM-DD` date (midnight UTC).
fn time(text: &str) -> Result<Timestamp, String> {
    if let Ok(at) = text.parse::<Timestamp>() {
        return Ok(at);
    }
    text.parse::<Date>()
        .ok()
        .and_then(|date| date.to_zoned(jiff::tz::TimeZone::UTC).ok())
        .map(|zoned| zoned.timestamp())
        .ok_or_else(|| format!("'{text}' is not an RFC 3339 time or YYYY-MM-DD date"))
}

/// `reply` serialized within [`RESULT_CAP`] bytes: the last sessions, or the
/// window messages farthest from the hit, are dropped until it fits.
fn capped(mut reply: Value) -> String {
    let mut text = reply.to_string();
    let key = if reply.get("messages").is_some() {
        "messages"
    } else {
        "sessions"
    };
    while text.len() > RESULT_CAP {
        let Some(list) = reply.get_mut(key).and_then(Value::as_array_mut) else {
            break;
        };
        if list.len() <= 1 {
            break;
        }
        let hit = list.iter().position(|m| m["hit"] == true).unwrap_or(0);
        if hit > list.len() - 1 - hit {
            list.remove(0);
        } else {
            list.pop();
        }
        reply["truncated"] = json!(true);
        text = reply.to_string();
    }
    text
}
