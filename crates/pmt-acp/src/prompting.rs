//! `session/prompt` and `session/cancel` (spec: acp-adapter, Prompt
//! submission and terminal outcome; Cancel targets only this editor's
//! admission; Exactly-once submit across lost acknowledgements; Daemon
//! restart yields an unknown outcome, never a resend; Slash commands are
//! answered or refused, never failed).

use std::sync::Arc;

use pmt_rpc::client::Client;
use pmt_rpc::contracts::{
    CommandDispatch, DispatchParams, Dispatched, InterruptParams, PromptSubmit, SessionInterrupt,
    SubmitParams, Submitted, TurnFinished,
};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::adapter::{Adapter, Admission, Ended};
use crate::content::Converted;
use crate::frames::AcpError;
use crate::kept::{Kept, fingerprint};
use crate::sessions::not_attached;

/// What an admission in flight answers when the daemon connection is lost.
pub(crate) const GATEWAY_LOST: &str = "Gateway disconnected; turn outcome is unknown. Load the session to check whether it completed; do not resend the work.";

/// The stop reason of a finished turn, or the error of a failed one.
fn stop_reason(finished: &TurnFinished) -> Result<&'static str, AcpError> {
    match finished.exit_reason.as_str() {
        "completed" => Ok("end_turn"),
        "interrupted_by_user" => Ok("cancelled"),
        "token_budget_exhausted" => Ok("max_tokens"),
        "iteration_limit" => Ok("max_turn_requests"),
        other => {
            let reason = finished
                .error
                .as_ref()
                .map_or_else(|| other.to_owned(), |e| e.message.clone());
            Err(AcpError::new(
                -32000,
                format!("admitted_turn_failed: {reason}"),
            ))
        }
    }
}

fn stopped(reason: &str) -> Value {
    json!({"stopReason": reason})
}

/// A daemon refusal of an admission with an unknown outcome.
fn unknown_execution(input_id: &str) -> AcpError {
    AcpError::new(
        -32004,
        format!(
            "unknown_execution: the outcome of admission {input_id} is unknown after the gateway connection was lost; load the session to check it, or send /discard {input_id} to drop it"
        ),
    )
}

/// Whether `error` is the connection failing (not the daemon answering).
fn lost(error: &pmt_rpc::RpcError) -> bool {
    error
        .data
        .as_ref()
        .is_some_and(|data| data.code == "rpc.connection")
}

impl Adapter {
    pub(crate) async fn prompt(self: &Arc<Self>, params: &Value) -> Result<Value, AcpError> {
        let editor = params["sessionId"].as_str().unwrap_or_default().to_owned();
        if self.daemon_id(&editor).is_none() {
            return Err(not_attached(&editor));
        }
        let blocks = params["prompt"].as_array().cloned().unwrap_or_default();
        let converted = crate::content::convert(&blocks, &*self.options.fetcher).await?;
        // Connecting re-attaches the held sessions and may settle an
        // unknown admission.
        let client = self.client().await?;
        if let Some(answer) = self.check_unknown(&editor, &converted).await? {
            return Ok(answer);
        }
        let (daemon, commands) = {
            let state = self.state();
            let session = state
                .sessions
                .get(&editor)
                .ok_or_else(|| not_attached(&editor))?;
            (session.daemon_id.clone(), session.commands.clone())
        };
        if let Some((name, args)) = crate::slash::command_of(&converted.text, &commands) {
            self.state().kept.retire(&editor);
            return self
                .dispatch(&client, &editor, &daemon, name, args.to_owned())
                .await;
        }
        self.submit(&client, &editor, &daemon, converted).await
    }

    /// Refuses new work while an admission's outcome is unknown, unless it
    /// is `/discard <id>` (dropping it) or the very prompt resent to the
    /// daemon instance that may have admitted it (which answers with the
    /// original admission). `Some` is the prompt's answer.
    async fn check_unknown(
        &self,
        editor: &str,
        converted: &Converted,
    ) -> Result<Option<Value>, AcpError> {
        let Some(unknown) = self
            .state()
            .sessions
            .get(editor)
            .and_then(|s| s.unknown.clone())
        else {
            return Ok(None);
        };
        let id = unknown.input_id.clone();
        if converted.text.trim() == format!("/discard {id}") {
            if let Some(session) = self.state().sessions.get_mut(editor) {
                session.unknown = None;
            }
            self.state().kept.retire(editor);
            self.say(editor, &format!("Discarded admission {id}."));
            return Ok(Some(stopped("end_turn")));
        }
        let instance = self.instance().await.unwrap_or_default();
        let print = fingerprint(&converted.text, &converted.images);
        let resend = self
            .state()
            .kept
            .matching(editor, &print)
            .is_some_and(|k| k.input_id == id && k.instance == instance);
        if resend {
            return Ok(None);
        }
        Err(unknown_execution(&id))
    }

    /// A registered command: its notice or refusal shown as an agent
    /// message (the prompt still ends normally), or the message it sends
    /// submitted as the prompt.
    async fn dispatch(
        self: &Arc<Self>,
        client: &Client,
        editor: &str,
        daemon: &str,
        name: String,
        args: String,
    ) -> Result<Value, AcpError> {
        let params = DispatchParams {
            session_id: daemon.to_owned(),
            name,
            args,
        };
        match client.call_typed::<CommandDispatch>(&params).await {
            Ok(Dispatched::Done { notice }) => {
                if !notice.is_empty() {
                    self.say(editor, &notice);
                }
                Ok(stopped("end_turn"))
            }
            Ok(Dispatched::Refused { reason }) => {
                self.say(editor, &reason);
                Ok(stopped("end_turn"))
            }
            Ok(Dispatched::Send { message, images }) => {
                let converted = Converted {
                    text: message,
                    images,
                };
                self.submit(client, editor, daemon, converted).await
            }
            Err(err) => {
                self.say(editor, &err.message);
                Ok(stopped("end_turn"))
            }
        }
    }

    /// Submits a prompt exactly once and waits for its admission's end.
    async fn submit(
        self: &Arc<Self>,
        client: &Client,
        editor: &str,
        daemon: &str,
        converted: Converted,
    ) -> Result<Value, AcpError> {
        let instance = self.instance().await.unwrap_or_default();
        let print = fingerprint(&converted.text, &converted.images);
        let text = converted.text.clone();
        let params = {
            let mut state = self.state();
            let reused = state
                .kept
                .matching(editor, &print)
                .filter(|k| k.instance == instance)
                .map(|k| k.params.clone());
            state.kept.retire(editor);
            let params = reused.unwrap_or_else(|| SubmitParams {
                session_id: daemon.to_owned(),
                text: converted.text,
                images: converted.images,
                files: Vec::new(),
                run_budget_secs: None,
                input_id: Some(ulid::Ulid::generate().to_string()),
                author: None,
                voice: false,
            });
            let session = state
                .sessions
                .get_mut(editor)
                .ok_or_else(|| not_attached(editor))?;
            session.unknown = None;
            session.admission = Some(Admission {
                input_id: params.input_id.clone().unwrap_or_default(),
                turn_id: None,
                cancel: false,
                text,
            });
            params
        };
        let submitted = self.send_submit(client, &params).await;
        match submitted {
            Ok(submitted) => self.wait(client, editor, submitted).await,
            Err(err) if lost(&err) => {
                self.keep_unknown(
                    editor,
                    Kept {
                        fingerprint: print,
                        input_id: params.input_id.clone().unwrap_or_default(),
                        params,
                        instance,
                    },
                );
                Err(AcpError::new(-32000, GATEWAY_LOST))
            }
            Err(err) => {
                if let Some(session) = self.state().sessions.get_mut(editor) {
                    session.admission = None;
                }
                Err(AcpError::of(&err))
            }
        }
    }

    /// `prompt.submit`; with the fault-injection seam armed, the first
    /// acknowledgement is lost with the connection after the daemon
    /// admitted the input.
    async fn send_submit(
        &self,
        client: &Client,
        params: &SubmitParams,
    ) -> Result<Submitted, pmt_rpc::RpcError> {
        if self.take_fault() {
            let frame = json!({
                "jsonrpc": "2.0", "id": "acp-fault", "method": "prompt.submit", "params": params,
            });
            client.send_raw(frame.to_string());
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            client.close();
            let _ = client.wait_closed().await;
        }
        client.call_typed::<PromptSubmit>(params).await
    }

    /// The admission's identity is kept for an exact resend, and its
    /// outcome is unknown.
    fn keep_unknown(&self, editor: &str, kept: Kept) {
        let mut state = self.state();
        state.kept.keep(editor, kept);
        if let Some(session) = state.sessions.get_mut(editor)
            && let Some(admission) = session.admission.take()
        {
            session.unknown = Some(admission);
        }
    }

    /// Waits for the end of admission `submitted`.
    async fn wait(
        self: &Arc<Self>,
        client: &Client,
        editor: &str,
        submitted: Submitted,
    ) -> Result<Value, AcpError> {
        let turn = submitted.turn_id.clone();
        let (tx, rx) = oneshot::channel();
        let cancel = {
            let mut state = self.state();
            let early = state.early.iter().position(|f| f.turn_id == turn);
            let early = early.and_then(|at| state.early.remove(at));
            match early {
                Some(finished) => {
                    let _ = tx.send(Ended::Finished(Box::new(finished)));
                }
                None => {
                    state.waiters.insert(turn.clone(), tx);
                }
            }
            let session = state.sessions.get_mut(editor);
            let admission = session.and_then(|s| s.admission.as_mut());
            admission.is_some_and(|a| {
                a.turn_id = Some(turn.clone());
                a.cancel
            })
        };
        if cancel {
            self.interrupt(client, editor, &turn).await;
        }
        let ended = rx.await.unwrap_or(Ended::Lost);
        let mut state = self.state();
        let session = state.sessions.get_mut(editor);
        match ended {
            Ended::Finished(finished) => {
                if let Some(session) = session {
                    session.admission = None;
                }
                drop(state);
                stop_reason(&finished).map(stopped)
            }
            Ended::Lost => {
                if let Some(session) = session {
                    session.unknown = session.admission.take().or(session.unknown.take());
                }
                Err(AcpError::new(-32000, GATEWAY_LOST))
            }
        }
    }

    async fn interrupt(self: &Arc<Self>, client: &Client, editor: &str, turn: &str) {
        let Some(daemon) = self.daemon_id(editor) else {
            return;
        };
        let params = InterruptParams {
            session_id: daemon,
            turn_id: Some(turn.to_owned()),
        };
        if let Err(err) = client.call_typed::<SessionInterrupt>(&params).await {
            tracing::warn!("cancel not delivered: {}", err.message);
        }
    }

    /// `session/cancel`: only this editor's admission; one whose
    /// acknowledgement has not arrived is cancelled when it does.
    pub(crate) async fn cancel(self: &Arc<Self>, params: &Value) {
        let editor = params["sessionId"].as_str().unwrap_or_default().to_owned();
        let turn = {
            let mut state = self.state();
            let Some(admission) = state
                .sessions
                .get_mut(&editor)
                .and_then(|s| s.admission.as_mut())
            else {
                return;
            };
            admission.cancel = true;
            admission.turn_id.clone()
        };
        let Some(turn) = turn else {
            return;
        };
        if let Ok(client) = self.client().await {
            self.interrupt(&client, &editor, &turn).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use pmt_rpc::contracts::{TurnError, TurnFinished};

    use super::stop_reason;

    fn finished(reason: &str, error: Option<&str>) -> TurnFinished {
        TurnFinished {
            turn_id: "t".into(),
            exit_reason: reason.into(),
            final_response: None,
            error: error.map(|m| TurnError {
                kind: "provider".into(),
                code: "provider.failed".into(),
                message: m.into(),
                hint: None,
            }),
            usage: None,
            report: None,
        }
    }

    /// Scenario: Turn fails.
    #[test]
    fn a_failed_turn_is_an_error_and_budgets_have_their_stop_reasons() {
        assert_eq!(stop_reason(&finished("completed", None)), Ok("end_turn"));
        assert_eq!(
            stop_reason(&finished("interrupted_by_user", None)),
            Ok("cancelled")
        );
        assert_eq!(
            stop_reason(&finished("token_budget_exhausted", None)),
            Ok("max_tokens")
        );
        assert_eq!(
            stop_reason(&finished("iteration_limit", None)),
            Ok("max_turn_requests")
        );
        let err = stop_reason(&finished("provider_failed", Some("upstream 500"))).unwrap_err();
        assert_eq!(err.message, "admitted_turn_failed: upstream 500");
    }
}
