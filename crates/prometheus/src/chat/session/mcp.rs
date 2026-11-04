//! A session as the profile's MCP servers reach it (spec: plugin-system,
//! MCP sampling; MCP elicitation; MCP tool costs and spend limits;
//! tool-system, MCP media results; MCP tool list changes respect the
//! frozen surface).
//!
//! Media blocks go to the profile's blob store (referenced by the
//! session). Sampling runs on the session's route, charged to its ledger
//! as `mcp:<server>`. A form elicitation is asked through the session's
//! clarify channel, whatever the approval mode or YOLO says (answer once
//! or decline); a session no one attends declines at once. Spend totals
//! are kept per conversation in the store.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use pmt_agent::tools::AgentTools;
use pmt_agent::tools::clarify::{Answer, AnswerStatus, Question};
use pmt_core::human::Unanswered;
use pmt_core::id::SessionId;
use pmt_mcp::host::{ElicitAnswer, Elicitation, HostFuture, Sampled, Sampling};
use pmt_plugins::host::ports::LlmCall;
use pmt_store::Store;
use pmt_tools::agent_level::CLARIFY_MAX_CHOICES;
use serde_json::{Map, Value};

use super::Factory;
use super::model::SwitchableModel;
use super::plugins::PluginLlm;
use super::progress::Notices;
use crate::mcp_host::sessions::{McpSession, Registration, register};

/// One session's side.
struct LiveMcp {
    profile_dir: PathBuf,
    store: Arc<Store>,
    id: SessionId,
    images: bool,
    notices: Notices,
    tools: Arc<AgentTools>,
    llm: Arc<PluginLlm>,
}

impl Factory {
    /// Makes session `id` reachable by the profile's MCP servers while
    /// the registration lives.
    pub(super) fn mcp_session(
        &self,
        id: SessionId,
        target: &pmt_providers::resolve::Target,
        model: &Arc<SwitchableModel>,
        notices: Notices,
        tools: &Arc<AgentTools>,
    ) -> Registration {
        let images = self.image_input(target);
        let session = LiveMcp {
            profile_dir: self.scope.dir().to_path_buf(),
            store: Arc::clone(&self.store),
            id,
            images,
            notices,
            tools: Arc::clone(tools),
            llm: self.session_llm(id, model),
        };
        register(&id.to_string(), Arc::new(session))
    }
}

impl McpSession for LiveMcp {
    fn profile_dir(&self) -> PathBuf {
        self.profile_dir.clone()
    }

    fn store_media(&self, bytes: &[u8], mime: &str) -> Result<String, String> {
        let blob = self
            .store
            .put_blob(bytes)
            .map_err(|e| e.message().to_owned())?;
        self.store
            .reference_blob(&blob, "mcp_media", &self.id.to_string())
            .map_err(|e| e.message().to_owned())?;
        tracing::debug!(session = %self.id, mime, "MCP media block stored");
        Ok(self.store.blob_path(&blob).to_string_lossy().into_owned())
    }

    fn accepts_images(&self) -> bool {
        self.images
    }

    fn notice(&self, text: String) {
        self.notices.show(text);
    }

    fn elicit(&self, request: Elicitation) -> HostFuture<'_, ElicitAnswer> {
        Box::pin(async move {
            let fields = fields(&request.schema);
            let questions = questions(&request, &fields);
            // The server's own timeout bounds the wait.
            match self.tools.clarifier().ask(questions, Duration::MAX).await {
                Ok(reply) => answer(&fields, reply.answers),
                Err(Unanswered::Dismissed) => ElicitAnswer::Cancel,
                Err(_) => ElicitAnswer::Decline,
            }
        })
    }

    fn sample(&self, request: Sampling) -> HostFuture<'_, Result<Sampled, String>> {
        Box::pin(async move {
            let mut messages = Vec::new();
            if let Some(system) = &request.system {
                messages.push(("system".to_owned(), system.clone()));
            }
            messages.extend(request.messages.iter().cloned());
            let model = request
                .model
                .clone()
                .or_else(|| self.llm.model.target().model)
                .unwrap_or_default();
            let call = LlmCall {
                plugin: request.server.clone(),
                session_id: Some(self.id.to_string()),
                messages,
                model: request.model.clone(),
                provider: None,
                max_tokens: Some(request.max_tokens),
                schema: None,
            };
            let text = self
                .llm
                .run_as(&format!("mcp:{}", request.server), call)
                .await?;
            Ok(Sampled { model, text })
        })
    }

    fn spent(&self, server: &str) -> f64 {
        self.store.mcp_spent(self.id, server).unwrap_or_else(|err| {
            tracing::error!(session = %self.id, "MCP spend unreadable: {err}");
            0.0
        })
    }

    fn add_spent(&self, server: &str, amount: f64) {
        if let Err(err) = self.store.add_mcp_spend(self.id, server, amount) {
            tracing::error!(session = %self.id, "MCP spend not recorded: {err}");
        }
    }
}

/// One requested field.
struct Field {
    name: String,
    kind: String,
    required: bool,
    choices: Vec<String>,
    label: String,
}

/// The schema's properties, in order.
fn fields(schema: &Value) -> Vec<Field> {
    let required: Vec<&str> = schema["required"]
        .as_array()
        .map(|r| r.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let Some(properties) = schema["properties"].as_object() else {
        return Vec::new();
    };
    properties
        .iter()
        .map(|(name, property)| {
            let kind = property["type"].as_str().unwrap_or("string").to_owned();
            let mut choices: Vec<String> = property["enum"]
                .as_array()
                .map(|e| {
                    e.iter()
                        .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            if kind == "boolean" {
                choices = vec!["yes".to_owned(), "no".to_owned()];
            }
            if choices.len() > CLARIFY_MAX_CHOICES {
                choices.clear();
            }
            let title = property["title"].as_str().unwrap_or(name);
            let label = match property["description"].as_str() {
                Some(description) => format!("{title} ({description})"),
                None => title.to_owned(),
            };
            Field {
                name: name.clone(),
                kind,
                required: required.contains(&name.as_str()),
                choices,
                label,
            }
        })
        .collect()
}

fn questions(request: &Elicitation, fields: &[Field]) -> Vec<Question> {
    let intro = format!("MCP server '{}' asks: {}", request.server, request.message);
    if fields.is_empty() {
        return vec![Question {
            question: intro,
            choices: vec!["yes".to_owned(), "no".to_owned()],
            multi: false,
        }];
    }
    fields
        .iter()
        .enumerate()
        .map(|(i, field)| Question {
            question: if i == 0 {
                format!("{intro}\n{}", field.label)
            } else {
                field.label.clone()
            },
            choices: field.choices.clone(),
            multi: false,
        })
        .collect()
}

/// The elicitation answer of the person's answers: declined when a
/// required field was skipped or a value does not fit its type.
fn answer(fields: &[Field], answers: Vec<Answer>) -> ElicitAnswer {
    if fields.is_empty() {
        let yes = answers.first().is_some_and(|a| {
            a.status == AnswerStatus::Answered
                && a.choices.first().map(String::as_str) != Some("no")
        });
        return if yes {
            ElicitAnswer::Accept(Map::new())
        } else {
            ElicitAnswer::Decline
        };
    }
    let mut content = Map::new();
    let mut answers = answers.into_iter();
    for field in fields {
        let given = answers
            .next()
            .filter(|a| a.status == AnswerStatus::Answered)
            .and_then(|a| {
                a.choices
                    .into_iter()
                    .next()
                    .or(a.text)
                    .filter(|t| !t.is_empty())
            });
        match given.map(|text| typed(&field.kind, &text)) {
            Some(Some(value)) => {
                content.insert(field.name.clone(), value);
            }
            Some(None) => return ElicitAnswer::Decline,
            None if field.required => return ElicitAnswer::Decline,
            None => {}
        }
    }
    ElicitAnswer::Accept(content)
}

/// `text` as a value of JSON Schema type `kind`.
fn typed(kind: &str, text: &str) -> Option<Value> {
    let text = text.trim();
    match kind {
        "boolean" => match text.to_ascii_lowercase().as_str() {
            "yes" | "true" | "y" => Some(Value::Bool(true)),
            "no" | "false" | "n" => Some(Value::Bool(false)),
            _ => None,
        },
        "integer" => text.parse::<i64>().ok().map(Value::from),
        "number" => text.parse::<f64>().ok().map(Value::from),
        _ => Some(Value::String(text.to_owned())),
    }
}
