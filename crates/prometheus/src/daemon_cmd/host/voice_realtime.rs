//! The realtime full-duplex voice session (spec: voice-mode, Realtime
//! full-duplex voice chat mode (upgrade)): `voice.realtime.start` creates
//! the provider's call with the profile's key on the daemon and hands the
//! desktop only the call id and the SDP answer; `voice.realtime.usage`
//! records each realtime response's tokens as a `voice.realtime` row of
//! the usage ledger, apart from the agent's own turns.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

use pmt_config::ProfileScope;
use pmt_config::schema::VoiceChatMode;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_media::speech::realtime_voice::AGENT_TOOL;
use pmt_rpc::contracts::{
    Ack, VoiceRealtimeCall, VoiceRealtimeStartParams, VoiceRealtimeUsageParams,
};
use pmt_store::Store;
use serde_json::Value;

/// The usage ledger purpose of the voice layer.
pub(super) const PURPOSE: &str = "voice.realtime";

/// A call this daemon created: whose it is and how it bills.
struct Call {
    profile: String,
    session: SessionId,
    provider: String,
    model: String,
    billing_source: String,
    /// The responses already recorded.
    recorded: HashSet<String>,
}

/// The calls created by this process, by call id (a call outlives no
/// daemon: its usage reports come from the desktop that started it).
static CALLS: LazyLock<Mutex<HashMap<String, Call>>> = LazyLock::new(Mutex::default);

fn refused(message: impl Into<String>) -> Error {
    Error::new(&codes::RPC_NOT_SERVED, message)
}

fn to_value<T: serde::Serialize>(value: &T) -> Result<Value, Error> {
    serde_json::to_value(value).map_err(|e| Error::new(&codes::RPC_INTERNAL, e.to_string()))
}

/// `voice.realtime.start`.
pub(super) async fn start(
    scope: &ProfileScope,
    profile: &str,
    params: VoiceRealtimeStartParams,
) -> Result<Value, Error> {
    let voice = &scope.config().voice;
    if voice.chat_mode != VoiceChatMode::Realtime {
        return Err(refused(
            "voice.chat_mode is not \"realtime\" for this profile",
        ));
    }
    let session: SessionId = params.session_id.parse()?;
    let store = Store::open(scope)?;
    store.session(session)?;
    let media = crate::chat::voice::media(scope, "gui", scope.dir())
        .ok_or_else(|| refused("the profile cannot reach its speech providers"))?;
    let call = media
        .realtime_call(&voice.realtime, &params.sdp)
        .await
        .map_err(refused)?;
    CALLS.lock().expect("realtime calls poisoned").insert(
        call.call_id.clone(),
        Call {
            profile: profile.to_owned(),
            session,
            provider: call.provider,
            model: call.model.clone(),
            billing_source: call.billing_source,
            recorded: HashSet::new(),
        },
    );
    to_value(&VoiceRealtimeCall {
        call_id: call.call_id,
        sdp: call.answer,
        model: call.model,
        tool: AGENT_TOOL.to_owned(),
    })
}

/// `voice.realtime.usage`: one row per response of a call this daemon
/// created for `profile`.
pub(super) fn usage(
    scope: &ProfileScope,
    profile: &str,
    params: &VoiceRealtimeUsageParams,
) -> Result<Value, Error> {
    let event = {
        let mut calls = CALLS.lock().expect("realtime calls poisoned");
        let call = calls
            .get_mut(&params.call_id)
            .filter(|c| c.profile == profile)
            .ok_or_else(|| refused(format!("no realtime call {}", params.call_id)))?;
        if !call.recorded.insert(params.response_id.clone()) {
            return to_value(&Ack { ok: true });
        }
        let usage = &params.usage;
        pmt_store::usage::UsageEvent {
            call_id: format!("{PURPOSE}:{}:{}", params.call_id, params.response_id),
            ts: jiff::Timestamp::now(),
            session_id: call.session,
            turn_id: None,
            parent_call_id: None,
            purpose: PURPOSE.to_owned(),
            purpose_detail: None,
            provider: call.provider.clone(),
            model: call.model.clone(),
            service_tier: None,
            tokens: pmt_store::usage::Tokens {
                input: usage.input_tokens.saturating_sub(usage.cached_tokens),
                output: usage.output_tokens,
                cache_read: usage.cached_tokens,
                cache_write: 0,
                reasoning: 0,
            },
            cost_usd: None,
            cost_status: pmt_store::CostStatus::Unknown,
            billing_source: call.billing_source.clone(),
            request_hash: None,
            cache_hit: false,
        }
    };
    Store::open(scope)?.record_usage(&event)?;
    to_value(&Ack { ok: true })
}
