//! The daemon's questions as editor requests (spec: acp-adapter, Approvals
//! and clarify bridge to editor cards with fail-closed semantics;
//! Editor-native file access; Editor terminal delegation).
//!
//! An approval becomes a permission card offering only the choices the
//! request allows, with a file edit shown as a diff; clarify questions use
//! the editor's elicitation forms when it has them, else one permission
//! card per question with a `Skip` option. A card the person dismisses
//! (or that fails) is answered to the daemon with an error: that withdraws
//! this editor's answer only, so another driver may still answer, and with
//! no other driver the daemon settles the approval as denied and the
//! clarify as unanswered at once. File reads, writes and terminal
//! commands the daemon hands this editor go to the editor's `fs/*` and
//! `terminal/*` methods.

use std::sync::Arc;

use pmt_rpc::RpcError;
use pmt_rpc::client::{Client, ServerCall};
use pmt_rpc::contracts::{
    ApprovalAnswer, ApprovalRequestParams, ClarifyRequestParams, Decision, EditorFsResult,
    EditorTerminalResult,
};
use serde_json::{Value, json};

use crate::adapter::Adapter;

/// The permission options of an approval: once and reject always; the
/// session and always grants only when the request can be remembered.
fn approval_options(rememberable: bool) -> Vec<Value> {
    let mut options =
        vec![json!({"optionId": "allow_once", "name": "Allow once", "kind": "allow_once"})];
    if rememberable {
        options.push(json!({"optionId": "allow_session", "name": "Allow for this session", "kind": "allow_always"}));
        options.push(
            json!({"optionId": "allow_always", "name": "Always allow", "kind": "allow_always"}),
        );
    }
    options.push(json!({"optionId": "reject_once", "name": "Reject", "kind": "reject_once"}));
    options
}

/// The decision of a picked option.
fn decision_of(option: &str) -> Option<Decision> {
    match option {
        "allow_once" => Some(Decision::Once),
        "allow_session" => Some(Decision::Session),
        "allow_always" => Some(Decision::Always),
        "reject_once" => Some(Decision::Deny),
        _ => None,
    }
}

/// The option an answered card picked (`None`: dismissed or failed).
fn picked(answer: &Result<Value, Value>) -> Option<String> {
    let outcome = &answer.as_ref().ok()?["outcome"];
    (outcome["outcome"] == "selected")
        .then(|| outcome["optionId"].as_str().map(str::to_owned))
        .flatten()
}

fn withdrawn() -> RpcError {
    RpcError::invalid_request("the editor's card was dismissed")
}

/// The shell an editor terminal runs a command line with.
fn shell(command: &str) -> (&'static str, Vec<String>) {
    if cfg!(windows) {
        ("cmd", vec!["/C".to_owned(), command.to_owned()])
    } else {
        ("sh", vec!["-c".to_owned(), command.to_owned()])
    }
}

/// The text of an editor error.
fn error_text(error: &Value) -> String {
    error["message"].as_str().map_or_else(
        || "the editor refused the request".to_owned(),
        str::to_owned,
    )
}

impl Adapter {
    pub(crate) async fn on_server_request(self: Arc<Self>, client: Client, call: ServerCall) {
        let daemon = call.params["session_id"].as_str().unwrap_or_default();
        let editor = self.editor_of(daemon);
        let answer = match (call.method.as_str(), editor) {
            ("approval.request", Some(editor)) => self.approval(&editor, &call.params).await,
            ("clarify.request", Some(editor)) => self.clarify(&editor, &call.params).await,
            ("editor.fs.read", Some(editor)) => Ok(self.fs_read(&editor, &call.params).await),
            ("editor.fs.write", Some(editor)) => Ok(self.fs_write(&editor, &call.params).await),
            ("editor.terminal.run", Some(editor)) => {
                Ok(self.terminal_run(&editor, &call.params).await)
            }
            (method, _) => Err(RpcError::invalid_request(&format!(
                "{method} is not answered by an editor"
            ))),
        };
        client.respond(&call.id, answer);
    }

    async fn approval(&self, editor: &str, params: &Value) -> Result<Value, RpcError> {
        let request: ApprovalRequestParams =
            serde_json::from_value(params.clone()).map_err(|_| withdrawn())?;
        let mut content: Vec<Value> = request
            .findings
            .iter()
            .map(|f| json!({"type": "content", "content": {"type": "text", "text": f.reason}}))
            .collect();
        if let Some(diff) = &request.diff {
            content.push(json!({
                "type": "diff",
                "path": diff.path,
                "oldText": diff.old_text,
                "newText": diff.new_text,
            }));
        }
        let title = request.subject.as_ref().map_or_else(
            || request.tool.clone(),
            |subject| format!("{}: {subject}", request.tool),
        );
        let card = json!({
            "sessionId": editor,
            "toolCall": {
                "toolCallId": request.call_id,
                "title": title,
                "kind": crate::project::kind_of(&request.tool),
                "status": "pending",
                "content": content,
            },
            "options": approval_options(request.remember.is_some()),
        });
        let answer = self.out.request("session/request_permission", card).await;
        let decision = picked(&answer)
            .as_deref()
            .and_then(decision_of)
            .ok_or_else(withdrawn)?;
        let answer = ApprovalAnswer {
            decision,
            accepted_hunks: None,
            replacement: None,
        };
        Ok(serde_json::to_value(answer).unwrap_or_default())
    }

    async fn clarify(&self, editor: &str, params: &Value) -> Result<Value, RpcError> {
        let request: ClarifyRequestParams =
            serde_json::from_value(params.clone()).map_err(|_| withdrawn())?;
        let answers = if self.caps().elicitation {
            self.elicit(editor, &request.questions).await
        } else {
            self.question_cards(editor, &request.questions).await
        };
        answers
            .map(|answers| json!({"answers": answers}))
            .ok_or_else(withdrawn)
    }

    /// One permission card per question: a choice, or `Skip`.
    async fn question_cards(&self, editor: &str, questions: &[Value]) -> Option<Vec<Value>> {
        let mut answers = Vec::new();
        for (index, question) in questions.iter().enumerate() {
            let choices = choices_of(question);
            let mut options: Vec<Value> = choices
                .iter()
                .enumerate()
                .map(|(n, c)| json!({"optionId": format!("choice-{n}"), "name": c, "kind": "allow_once"}))
                .collect();
            options.push(json!({"optionId": "skip", "name": "Skip", "kind": "reject_once"}));
            let card = json!({
                "sessionId": editor,
                "toolCall": {
                    "toolCallId": format!("clarify-{index}"),
                    "title": question["question"],
                    "kind": "think",
                    "status": "pending",
                },
                "options": options,
            });
            let answer = self.out.request("session/request_permission", card).await;
            let option = picked(&answer)?;
            answers.push(
                match option
                    .strip_prefix("choice-")
                    .and_then(|n| n.parse::<usize>().ok())
                {
                    Some(n) if n < choices.len() => {
                        json!({"status": "answered", "choices": [choices[n]]})
                    }
                    _ => json!({"status": "skipped"}),
                },
            );
        }
        Some(answers)
    }

    /// All questions in one elicitation form.
    async fn elicit(&self, editor: &str, questions: &[Value]) -> Option<Vec<Value>> {
        let mut properties = serde_json::Map::new();
        for (index, question) in questions.iter().enumerate() {
            let mut field = json!({"type": "string", "title": question["question"]});
            let choices = choices_of(question);
            if !choices.is_empty() {
                field["examples"] = json!(choices);
            }
            properties.insert(format!("q{index}"), field);
        }
        let form = json!({
            "sessionId": editor,
            "mode": "form",
            "message": "The agent has questions",
            "requestedSchema": {"type": "object", "properties": properties},
        });
        let answer = self.out.request("session/elicitation", form).await.ok()?;
        if answer["action"] != "accept" {
            return None;
        }
        let answers = questions
            .iter()
            .enumerate()
            .map(|(index, question)| {
                let value = answer["content"][format!("q{index}")]
                    .as_str()
                    .unwrap_or_default();
                let choices = choices_of(question);
                match value {
                    "" => json!({"status": "skipped"}),
                    chosen if choices.iter().any(|c| c == chosen) => {
                        json!({"status": "answered", "choices": [chosen]})
                    }
                    text => json!({"status": "answered", "text": text}),
                }
            })
            .collect();
        Some(answers)
    }

    async fn fs_read(&self, editor: &str, params: &Value) -> Value {
        let request = json!({"sessionId": editor, "path": params["path"]});
        let result = match self.out.request("fs/read_text_file", request).await {
            Ok(answer) => EditorFsResult {
                content: Some(answer["content"].as_str().unwrap_or_default().to_owned()),
                error: None,
            },
            Err(error) => EditorFsResult {
                content: None,
                error: Some(error_text(&error)),
            },
        };
        serde_json::to_value(result).unwrap_or_default()
    }

    async fn fs_write(&self, editor: &str, params: &Value) -> Value {
        let request =
            json!({"sessionId": editor, "path": params["path"], "content": params["content"]});
        let result = match self.out.request("fs/write_text_file", request).await {
            Ok(_) => EditorFsResult::default(),
            Err(error) => EditorFsResult {
                content: None,
                error: Some(error_text(&error)),
            },
        };
        serde_json::to_value(result).unwrap_or_default()
    }

    /// Runs a command in an editor terminal: create, wait for its exit,
    /// read its output, release it.
    async fn terminal_run(&self, editor: &str, params: &Value) -> Value {
        let (command, args) = shell(params["command"].as_str().unwrap_or_default());
        let create = json!({
            "sessionId": editor,
            "command": command,
            "args": args,
            "cwd": params["cwd"],
            "outputByteLimit": params["output_byte_limit"],
        });
        let result = match self.out.request("terminal/create", create).await {
            Ok(created) => {
                let terminal = json!({"sessionId": editor, "terminalId": created["terminalId"]});
                self.finish_terminal(&terminal).await
            }
            Err(error) => EditorTerminalResult {
                error: Some(error_text(&error)),
                ..EditorTerminalResult::default()
            },
        };
        serde_json::to_value(result).unwrap_or_default()
    }

    async fn finish_terminal(&self, terminal: &Value) -> EditorTerminalResult {
        let exited = self
            .out
            .request("terminal/wait_for_exit", terminal.clone())
            .await
            .unwrap_or_default();
        let output = self
            .out
            .request("terminal/output", terminal.clone())
            .await
            .unwrap_or_default();
        let _ = self.out.request("terminal/release", terminal.clone()).await;
        let exit_code = exited["exitCode"]
            .as_i64()
            .and_then(|c| i32::try_from(c).ok());
        EditorTerminalResult {
            output: output["output"].as_str().unwrap_or_default().to_owned(),
            truncated: output["truncated"].as_bool().unwrap_or(false),
            exit_code,
            signal: exited["signal"].as_str().map(str::to_owned),
            error: None,
        }
    }
}

/// The offered choices of a clarify question.
fn choices_of(question: &Value) -> Vec<String> {
    question["choices"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c.as_str().map(str::to_owned))
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{approval_options, decision_of, picked};

    #[test]
    fn a_card_offers_only_what_the_request_allows() {
        let once: Vec<_> = approval_options(false)
            .iter()
            .map(|o| o["optionId"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(once, ["allow_once", "reject_once"]);
        assert_eq!(approval_options(true).len(), 4);
        assert_eq!(
            decision_of("allow_session"),
            Some(pmt_rpc::contracts::Decision::Session)
        );
    }

    #[test]
    fn a_dismissed_or_failed_card_picks_nothing() {
        let cancelled = Ok(json!({"outcome": {"outcome": "cancelled"}}));
        assert_eq!(picked(&cancelled), None);
        assert_eq!(picked(&Err(json!({"code": -32603}))), None);
        let chosen = Ok(json!({"outcome": {"outcome": "selected", "optionId": "allow_once"}}));
        assert_eq!(picked(&chosen).as_deref(), Some("allow_once"));
    }
}
