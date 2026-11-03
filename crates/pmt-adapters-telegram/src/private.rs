//! Replies only one member of a chat sees (spec: messaging-gateway,
//! Bystander cannot approve). A pressed button is answered with
//! `answerCallbackQuery`, which Telegram allows once: the answer is held
//! for [`HOLD`] so a refusal can go into it as an alert only the presser
//! sees; otherwise it is answered empty, which stops the button's spinner.
//! Without a pressed button the member gets a direct message (Telegram
//! refuses it when they never started the bot; the gateway then posts in
//! the chat).

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pmt_gateway::messaging::{Private, SendError};
use serde_json::json;
use tracing::Instrument as _;

use crate::api::Api;

/// How long a callback answer waits for a private reply.
const HOLD: Duration = Duration::from_secs(1);

/// Callback queries not answered yet.
#[derive(Debug, Default)]
pub(crate) struct Callbacks(Mutex<HashSet<String>>);

impl Callbacks {
    /// Holds the answer to callback query `id`, answering it empty after
    /// [`HOLD`] unless a private reply took it.
    pub(crate) fn hold(self: &Arc<Self>, api: &Api, id: &str) {
        self.0
            .lock()
            .expect("callbacks poisoned")
            .insert(id.to_owned());
        let (callbacks, api, id) = (Arc::clone(self), api.clone(), id.to_owned());
        tokio::spawn(
            async move {
                tokio::time::sleep(HOLD).await;
                if callbacks.take(&id) {
                    let _ = api
                        .call("answerCallbackQuery", &json!({"callback_query_id": id}))
                        .await;
                }
            }
            .in_current_span(),
        );
    }

    /// Takes callback query `id` when it is still unanswered.
    fn take(&self, id: &str) -> bool {
        self.0.lock().expect("callbacks poisoned").remove(id)
    }
}

/// Tells `to.sender_id` alone `to.text`.
pub(crate) async fn reply(api: &Api, callbacks: &Callbacks, to: &Private) -> Result<(), SendError> {
    if let Some(id) = to.interaction.as_deref().filter(|id| callbacks.take(id)) {
        let body = json!({"callback_query_id": id, "text": to.text, "show_alert": true});
        return api.call("answerCallbackQuery", &body).await.map(|_| ());
    }
    let body = json!({"chat_id": to.sender_id, "text": to.text});
    api.call("sendMessage", &body).await.map(|_| ())
}
