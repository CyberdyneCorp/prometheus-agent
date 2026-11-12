//! Hermes rows to Prometheus records: typed source, origin, end reason
//! and lineage, the OpenAI-shaped tool calls in the Prometheus shape,
//! reasoning items, provider-opaque fields in the provider payload, and
//! one usage event per session and model (cost source `hermes_import`).

use jiff::Timestamp;
use pmt_core::id::{MessageId, SessionId};
use pmt_store::messages::Message;
use pmt_store::model::{Archived, EndReason, LineageKind, Origin, Role, Source, TitleSource};
use pmt_store::{CostStatus, Session, Tokens, UsageEvent};
use serde_json::{Map, Value, json};

use super::read::{HermesMessage, HermesSession, ModelUsage, json as parse_json};

/// The `purpose_detail` of imported usage events.
pub const COST_SOURCE: &str = "hermes_import";

/// End reasons whose child is a reset continuation (Hermes'
/// `_RESET_END_REASONS`).
const RESET_REASONS: &[&str] = &[
    "session_reset",
    "session_switch",
    "idle",
    "daily",
    "suspended",
    "resume_pending_expired",
];

/// Hermes gateway platforms (the session source of a messaging chat).
const PLATFORMS: &[&str] = &[
    "telegram",
    "discord",
    "slack",
    "signal",
    "matrix",
    "email",
    "whatsapp",
    "whatsapp_cloud",
    "webhook",
    "mattermost",
    "dingtalk",
    "feishu",
    "wecom",
    "wecom_callback",
    "weixin",
    "bluebubbles",
    "qqbot",
    "yuanbao",
    "sms",
    "line",
    "ntfy",
    "irc",
    "teams",
    "google_chat",
    "homeassistant",
    "simplex",
    "buzz",
    "photon",
    "relay",
    "msgraph_webhook",
];

pub(super) fn timestamp(seconds: f64) -> Timestamp {
    #[allow(clippy::cast_possible_truncation)]
    Timestamp::from_millisecond((seconds * 1000.0) as i64).unwrap_or(Timestamp::UNIX_EPOCH)
}

/// The Prometheus source (and integration tag) of a Hermes source.
fn source_of(hermes: &str) -> (Source, Option<String>) {
    let source = match hermes {
        "cli" => Source::Cli,
        "tui" => Source::Tui,
        "cron" => Source::Cron,
        "subagent" => Source::Subagent,
        "acp" => Source::Acp,
        "desktop" => Source::Desktop,
        "dashboard" | "webapp" => Source::Dashboard,
        "api" | "api_server" => Source::Api,
        "tool" => return (Source::Cli, Some("tool".to_owned())),
        p if PLATFORMS.contains(&p) => Source::Gateway,
        _ => Source::Import,
    };
    (source, None)
}

fn origin_of(s: &HermesSession, source: Source) -> Origin {
    let origin = parse_json(s.origin_json.as_deref());
    let field = |k: &str| {
        origin
            .as_ref()
            .and_then(|o| o.get(k))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    match source {
        Source::Gateway => Origin::Gateway {
            platform: field("platform").unwrap_or_else(|| s.source.clone()),
            chat: s
                .chat_id
                .clone()
                .or_else(|| field("chat_id"))
                .unwrap_or_default(),
            thread: s.thread_id.clone().or_else(|| field("thread_id")),
            user: s.user_id.clone().or_else(|| field("user_id")),
        },
        Source::Cron => field("job_id").map_or(Origin::None, |job_id| Origin::Cron { job_id }),
        _ => Origin::None,
    }
}

fn end_reason_of(reason: &str) -> Option<EndReason> {
    Some(match reason {
        "compression" => EndReason::Compression,
        "user_exit" | "cli_close" => EndReason::UserExit,
        "new_session" | "session_reset" | "session_switch" | "daily" | "branched" => {
            EndReason::NewSession
        }
        "idle" | "idle_timeout" | "lru_evict" => EndReason::IdleTimeout,
        "done" | "job_complete" => EndReason::JobComplete,
        "error" => EndReason::Error,
        "budget_exhausted" => EndReason::BudgetExhausted,
        "" => return None,
        _ => EndReason::Interrupted,
    })
}

/// How a Hermes child relates to its parent (Hermes' lineage predicates:
/// stable `model_config` markers first, then the legacy heuristics).
pub(super) fn lineage_of(child: &HermesSession, parent: &HermesSession) -> LineageKind {
    let config = parse_json(child.model_config.as_deref()).unwrap_or(Value::Null);
    let marks = |marker: &str| config.get(marker).and_then(Value::as_str) == Some(&parent.id);
    let parent_reason = parent.end_reason.as_deref().unwrap_or("");
    if marks("_branched_from")
        || (parent_reason == "branched"
            && parent.ended_at.is_some_and(|end| child.started_at >= end))
    {
        return LineageKind::Fork;
    }
    if marks("_delegate_from") || child.source == "subagent" {
        return LineageKind::Delegate;
    }
    let same_key = child
        .session_key
        .as_deref()
        .is_some_and(|k| !k.is_empty() && parent.session_key.as_deref() == Some(k));
    if marks("_reset_from") || (RESET_REASONS.contains(&parent_reason) && same_key) {
        return LineageKind::Reset;
    }
    if parent_reason == "compression" {
        return LineageKind::Compression;
    }
    LineageKind::Fork
}

fn title_source_of(s: &HermesSession) -> Option<TitleSource> {
    s.title.as_ref()?;
    Some(match s.title_source.as_deref() {
        Some("user" | "manual") => TitleSource::User,
        Some("derived" | "preview") => TitleSource::Derived,
        _ => TitleSource::Llm,
    })
}

/// The Prometheus session record of `s` (lineage set by the caller).
pub(super) fn session(
    s: &HermesSession,
    id: SessionId,
    parent: Option<(SessionId, LineageKind)>,
) -> Session {
    let (source, source_tag) = source_of(&s.source);
    let started = timestamp(s.started_at);
    let ended = s.ended_at.map(timestamp);
    let last = s
        .last_activity_at
        .map(timestamp)
        .or(ended)
        .unwrap_or(started);
    let archived = match (s.archived, s.auto_archived) {
        (true, true) => Archived::Auto,
        (true, false) => Archived::User,
        _ => Archived::None,
    };
    Session {
        id,
        profile_id: String::new(),
        source,
        source_tag,
        origin: origin_of(s, source),
        model: s.model.clone(),
        provider: s.billing_provider.clone().filter(|p| !p.is_empty()),
        cwd: s.cwd.clone(),
        git_repo_root: s.git_repo_root.clone(),
        git_branch: s.git_branch.clone(),
        title: s.title.clone().filter(|t| !t.trim().is_empty()),
        title_source: title_source_of(s),
        started_at: started,
        last_activity_at: last,
        ended_at: ended,
        end_reason: s.end_reason.as_deref().and_then(end_reason_of),
        pinned: s.pinned,
        archived,
        hidden: s.hidden,
        bot_chat: false,
        system_prompt_hash: None,
        parent_session_id: parent.map(|(p, _)| p),
        lineage_kind: parent.map(|(_, k)| k),
        parent_deleted: false,
        root_session_id: id,
        message_count: 0,
        tool_call_count: 0,
        compaction_count: 0,
        rewind_count: 0,
        trajectory_purged_at: None,
        project_id: None,
    }
}

fn role_of(role: &str) -> Option<Role> {
    match role {
        "system" => Some(Role::System),
        "user" => Some(Role::User),
        "assistant" => Some(Role::Assistant),
        "tool" | "function" => Some(Role::Tool),
        _ => None,
    }
}

/// OpenAI `[{id, function: {name, arguments}}]` to the Prometheus
/// `[{id, name, arguments}]` (arguments parsed when they are JSON).
fn tool_calls(text: Option<&str>) -> Option<Value> {
    let calls = parse_json(text)?;
    let Value::Array(items) = calls else {
        return Some(calls);
    };
    let converted = items
        .into_iter()
        .map(|call| {
            let function = call.get("function").cloned().unwrap_or(Value::Null);
            let name = function
                .get("name")
                .or_else(|| call.get("name"))
                .cloned()
                .unwrap_or(Value::Null);
            let raw = function
                .get("arguments")
                .or_else(|| call.get("arguments"))
                .cloned()
                .unwrap_or(Value::Null);
            let arguments = match raw {
                Value::String(s) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
                other => other,
            };
            json!({ "id": call.get("id").cloned().unwrap_or(Value::Null), "name": name, "arguments": arguments })
        })
        .collect();
    Some(Value::Array(converted))
}

/// The provider-opaque fields Hermes kept per message.
fn provider_payload(m: &HermesMessage) -> Option<Value> {
    let mut hermes = Map::new();
    for (key, text) in [
        ("reasoning_details", &m.reasoning_details),
        ("codex_reasoning_items", &m.codex_reasoning_items),
        ("codex_message_items", &m.codex_message_items),
    ] {
        if let Some(value) = parse_json(text.as_deref()) {
            hermes.insert(key.to_owned(), value);
        }
    }
    (!hermes.is_empty()).then(|| json!({ "hermes": hermes }))
}

/// The Prometheus message of `m` (`None` for a role Prometheus has not).
pub(super) fn message(m: &HermesMessage, session: SessionId) -> Option<Message> {
    let role = role_of(&m.role)?;
    let reasoning = m
        .reasoning
        .as_deref()
        .or(m.reasoning_content.as_deref())
        .filter(|r| !r.trim().is_empty())
        .map(|r| json!([{ "text": r }]));
    let mut metadata = Map::new();
    if let Some(id) = &m.platform_message_id {
        metadata.insert("platform_message_id".to_owned(), Value::from(id.as_str()));
    }
    if let Some(reason) = &m.finish_reason {
        metadata.insert("finish_reason".to_owned(), Value::from(reason.as_str()));
    }
    Some(Message {
        id: MessageId::new(),
        session_id: session,
        seq: 0,
        role,
        content: m.content.clone(),
        tool_calls: tool_calls(m.tool_calls.as_deref()),
        reasoning,
        provider_payload: provider_payload(m),
        tool_call_id: m.tool_call_id.clone(),
        tool_name: m.tool_name.clone(),
        metadata: (!metadata.is_empty()).then_some(Value::Object(metadata)),
        content_hash: String::new(),
        created_at: timestamp(m.timestamp),
        active: m.active,
        compacted: m.compacted,
        compaction_epoch: None,
        content_ref: None,
    })
}

fn tokens(input: i64, output: i64, read: i64, write: i64, reasoning: i64) -> Tokens {
    let n = |v: i64| u64::try_from(v).unwrap_or(0);
    Tokens {
        input: n(input),
        output: n(output),
        cache_read: n(read),
        cache_write: n(write),
        reasoning: n(reasoning),
    }
}

fn billing(provider: &str, mode: &str) -> String {
    match (provider, mode) {
        ("", _) => "local".to_owned(),
        (p, "subscription") => format!("subscription:{p}"),
        (p, _) => format!("api_key:{p}"),
    }
}

fn cost(actual: f64, estimated: f64) -> (Option<f64>, CostStatus) {
    if actual > 0.0 {
        (Some(actual), CostStatus::Exact)
    } else if estimated > 0.0 {
        (Some(estimated), CostStatus::Estimated)
    } else {
        (None, CostStatus::Unknown)
    }
}

/// One usage event per model of `s` (its per-model rows, else its
/// totals); none for a session without tokens or cost.
pub(super) fn usage(s: &HermesSession, rows: &[ModelUsage], session: SessionId) -> Vec<UsageEvent> {
    let ts = timestamp(s.ended_at.or(s.last_activity_at).unwrap_or(s.started_at));
    let event = |i: usize,
                 model: &str,
                 provider: &str,
                 mode: &str,
                 t: Tokens,
                 c: (Option<f64>, CostStatus)| UsageEvent {
        call_id: format!("hermes:{}:{i}", s.id),
        ts,
        session_id: session,
        turn_id: None,
        parent_call_id: None,
        purpose: "import".to_owned(),
        purpose_detail: Some(COST_SOURCE.to_owned()),
        provider: provider.to_owned(),
        model: model.to_owned(),
        service_tier: None,
        tokens: t,
        cost_usd: c.0,
        cost_status: c.1,
        billing_source: billing(provider, mode),
        request_hash: None,
        cache_hit: false,
    };
    if !rows.is_empty() {
        return rows
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let t = tokens(
                    r.input_tokens,
                    r.output_tokens,
                    r.cache_read_tokens,
                    r.cache_write_tokens,
                    r.reasoning_tokens,
                );
                event(
                    i,
                    &r.model,
                    &r.billing_provider,
                    &r.billing_mode,
                    t,
                    cost(r.actual_cost_usd, r.estimated_cost_usd),
                )
            })
            .collect();
    }
    let t = tokens(
        s.input_tokens,
        s.output_tokens,
        s.cache_read_tokens,
        s.cache_write_tokens,
        s.reasoning_tokens,
    );
    let c = cost(
        s.actual_cost_usd.unwrap_or(0.0),
        s.estimated_cost_usd.unwrap_or(0.0),
    );
    if t == Tokens::default() && c.0.is_none() {
        return Vec::new();
    }
    vec![event(
        0,
        s.model.as_deref().unwrap_or("unknown"),
        s.billing_provider.as_deref().unwrap_or(""),
        s.billing_mode.as_deref().unwrap_or(""),
        t,
        c,
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, end: Option<&str>) -> HermesSession {
        HermesSession {
            id: id.to_owned(),
            source: "cli".to_owned(),
            end_reason: end.map(str::to_owned),
            started_at: 100.0,
            ended_at: end.map(|_| 200.0),
            ..HermesSession::default()
        }
    }

    #[test]
    fn hermes_parent_links_become_typed_lineage() {
        let parent = row("p", Some("compression"));
        let mut child = row("c", None);
        child.started_at = 300.0;
        assert_eq!(lineage_of(&child, &parent), LineageKind::Compression);
        child.model_config = Some(r#"{"_branched_from": "p"}"#.to_owned());
        assert_eq!(lineage_of(&child, &parent), LineageKind::Fork);
        child.model_config = Some(r#"{"_delegate_from": "p"}"#.to_owned());
        assert_eq!(lineage_of(&child, &parent), LineageKind::Delegate);
        // An inherited marker naming another session is not a fork.
        child.model_config = Some(r#"{"_branched_from": "older"}"#.to_owned());
        assert_eq!(lineage_of(&child, &parent), LineageKind::Compression);
        let reset_parent = HermesSession {
            session_key: Some("telegram:1".to_owned()),
            ..row("p", Some("session_reset"))
        };
        let reset_child = HermesSession {
            session_key: Some("telegram:1".to_owned()),
            ..row("c", None)
        };
        assert_eq!(lineage_of(&reset_child, &reset_parent), LineageKind::Reset);
        assert_eq!(
            lineage_of(&row("c", None), &row("p", Some("branched"))),
            LineageKind::Fork
        );
    }

    #[test]
    fn openai_tool_calls_and_opaque_fields_convert() {
        let m = HermesMessage {
            role: "assistant".to_owned(),
            tool_calls: Some(
                r#"[{"id": "c1", "type": "function", "function": {"name": "terminal", "arguments": "{\"command\": \"ls\"}"}}]"#
                    .to_owned(),
            ),
            reasoning: Some("thinking".to_owned()),
            codex_reasoning_items: Some(r#"[{"type": "reasoning", "encrypted_content": "x"}]"#.to_owned()),
            timestamp: 1.0,
            active: true,
            ..HermesMessage::default()
        };
        let converted = message(&m, SessionId::new()).unwrap();
        assert_eq!(
            converted.tool_calls,
            Some(json!([{ "id": "c1", "name": "terminal", "arguments": { "command": "ls" } }]))
        );
        assert_eq!(converted.reasoning, Some(json!([{ "text": "thinking" }])));
        assert_eq!(
            converted.provider_payload.unwrap()["hermes"]["codex_reasoning_items"][0]["type"],
            "reasoning"
        );
        let unknown = HermesMessage {
            role: "session_meta".to_owned(),
            ..HermesMessage::default()
        };
        assert!(message(&unknown, SessionId::new()).is_none());
    }

    #[test]
    fn usage_is_per_model_when_hermes_has_the_rows() {
        let s = HermesSession {
            input_tokens: 10,
            billing_provider: Some("anthropic".to_owned()),
            ..row("s", None)
        };
        let totals = usage(&s, &[], SessionId::new());
        assert_eq!(totals.len(), 1);
        assert_eq!(totals[0].tokens.input, 10);
        assert_eq!(totals[0].purpose_detail.as_deref(), Some(COST_SOURCE));
        let rows = [
            ModelUsage {
                model: "a".to_owned(),
                input_tokens: 3,
                actual_cost_usd: 0.5,
                ..ModelUsage::default()
            },
            ModelUsage {
                model: "b".to_owned(),
                input_tokens: 7,
                ..ModelUsage::default()
            },
        ];
        let per_model = usage(&s, &rows, SessionId::new());
        assert_eq!(per_model.len(), 2);
        assert_eq!(per_model[0].cost_usd, Some(0.5));
        assert_eq!(per_model[0].cost_status, CostStatus::Exact);
        assert!(usage(&row("empty", None), &[], SessionId::new()).is_empty());
    }
}
