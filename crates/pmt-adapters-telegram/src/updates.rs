//! What the bot does with one update, however it arrived (long polling
//! or a webhook post): inline queries are answered from the command list,
//! topic creations are remembered, root-DM chatter is dropped when
//! `ignore_root_dm` asks for it, and everything else is submitted.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_gateway::messaging::{
    AdapterError, AdapterLink, AdapterState, ChatType, MenuCommand, SendError,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::api::Api;
use crate::topics::Topics;
use crate::{PLATFORM, commands, normalize};

/// The updates the bot asks Telegram for.
pub(crate) const ALLOWED: &[&str] = &["message", "callback_query", "inline_query"];

/// Everything handling an update needs.
#[derive(Clone)]
pub(crate) struct Ctx {
    pub(crate) api: Api,
    pub(crate) link: Arc<dyn AdapterLink>,
    pub(crate) commands: Arc<Mutex<Vec<MenuCommand>>>,
    pub(crate) topics: Arc<Topics>,
    pub(crate) ignore_root_dm: bool,
    /// The bot (`getMe`), to recognise mentions of it.
    pub(crate) me: normalize::Me,
}

/// Handles one raw update.
pub(crate) async fn handle(ctx: &Ctx, raw: &Value) {
    if raw["inline_query"].is_object() {
        commands::answer_inline(
            &ctx.api,
            ctx.link.as_ref(),
            &ctx.commands,
            &raw["inline_query"],
        )
        .await;
        return;
    }
    let message = &raw["message"];
    if let (Some(name), Some(chat), Some(thread)) = (
        message["forum_topic_created"]["name"].as_str(),
        normalize::id(&message["chat"]["id"]),
        message["message_thread_id"].as_i64(),
    ) {
        ctx.topics.record(&chat, name, thread);
    }
    let Some(update) = normalize::update(raw, &ctx.me) else {
        return;
    };
    let event = &update.inbound;
    let root_chatter = event.chat_type == ChatType::Dm
        && event.thread_id.is_none()
        && event.callback.is_none()
        && !event.text.trim_start().starts_with('/');
    if ctx.ignore_root_dm && root_chatter {
        return;
    }
    if let Some(query) = &update.callback_query {
        let _ = ctx
            .api
            .call("answerCallbackQuery", &json!({"callback_query_id": query}))
            .await;
    }
    ctx.link.submit(update.inbound).await;
}

/// Polls `getUpdates` until stopped or the connection fails; the cursor
/// moves past an update only after the gateway took it.
pub(crate) async fn poll(
    ctx: Ctx,
    offset: Arc<AtomicI64>,
    timeout_secs: u64,
    stop: CancellationToken,
) {
    loop {
        let body = json!({
            "offset": offset.load(Ordering::Acquire),
            "timeout": timeout_secs,
            "allowed_updates": ALLOWED,
        });
        let wait = Duration::from_secs(timeout_secs + 10);
        let polled = tokio::select! {
            () = stop.cancelled() => return,
            polled = ctx.api.call_within("getUpdates", &body, wait) => polled,
        };
        let updates = match polled {
            Ok(updates) => updates,
            Err(err) => return lost(ctx.link.as_ref(), &err),
        };
        for raw in updates.as_array().into_iter().flatten() {
            handle(&ctx, raw).await;
            if let Some(id) = raw["update_id"].as_i64() {
                offset.store(id + 1, Ordering::Release);
            }
        }
    }
}

/// Reports a failed poll: retried by the gateway unless the token is bad.
fn lost(link: &dyn AdapterLink, err: &SendError) {
    let (state, code, retryable) = match err {
        SendError::Permanent(why) if why.starts_with("401") => {
            (AdapterState::Fatal, "unauthorized", false)
        }
        SendError::Permanent(why) if why.starts_with("409") => {
            (AdapterState::Retrying, "telegram_conflict", true)
        }
        _ => (AdapterState::Retrying, "telegram_unreachable", true),
    };
    tracing::warn!(platform = PLATFORM, "polling stopped: {}", err.describe());
    link.report(
        state,
        Some(AdapterError {
            retryable,
            code: code.to_owned(),
            message: err.describe(),
        }),
    );
}
