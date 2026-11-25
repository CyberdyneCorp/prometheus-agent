//! The Teams meeting summary pipeline (spec: platform-adapters, Teams
//! meeting summary pipeline). It is built on the Microsoft Graph
//! change-notification listener ([`adapter_kit::graph::Listener`], the
//! same handshake, clientState, source-range and deduplication rules as
//! `msgraph_webhook`) at `/p/<profile>/teams_meetings/`, which its own
//! subscriptions name as their notification URL. [`pipeline`] turns
//! notifications into summary turns and delivers the answers, [`graph`]
//! talks to Microsoft Graph, [`records`] keeps state, [`cli`] answers
//! `prometheus plugins run teams_meetings ...` (the host hands a command
//! to `send` as `{"op": "cli"}`), and the poll's ticker renews
//! subscriptions before they expire.

mod cli;
mod graph;
mod pipeline;
mod records;

use std::sync::{Mutex, MutexGuard};

use adapter_kit::graph::Listener;
use adapter_kit::message::{self, Inbound, Outgoing};
use adapter_kit::{Guest, HttpRequest, HttpResponse, clock, respond};
use serde_json::{Value, json};

/// How often (in polls) asked replays are looked for.
const REPLAY_POLLS: u64 = 4;

/// Where summaries go.
#[derive(Debug, Clone)]
enum Delivery {
    /// POSTed to the incoming-webhook URL in this secret.
    Webhook(String),
    /// Posted into the meeting's chat.
    Graph,
}

#[derive(Debug, Clone)]
struct Settings {
    public_url: Option<String>,
    client_state_secret: String,
    resources: Vec<String>,
    use_recordings: bool,
    delivery: Delivery,
    prompt: String,
    renew_within_secs: u64,
    extend_secs: u64,
    check_every_secs: u64,
}

/// The started pipeline.
struct Meetings {
    settings: Settings,
    graph: graph::Graph,
    listener: Listener,
    /// Admitted notifications, worked on at the next poll.
    queue: Vec<Value>,
    inbox: Vec<Inbound>,
    /// Plugin clock ms of the last renewal check.
    checked: Option<u64>,
    polls: u64,
}

static MEETINGS: Mutex<Option<Meetings>> = Mutex::new(None);

fn lock() -> Result<MutexGuard<'static, Option<Meetings>>, String> {
    MEETINGS.lock().map_err(|e| e.to_string())
}

fn with<R>(f: impl FnOnce(&mut Meetings) -> Result<R, String>) -> Result<R, String> {
    let mut guard = lock()?;
    f(guard.as_mut().ok_or("not started")?)
}

const DEFAULT_PROMPT: &str = "Summarize this Microsoft Teams meeting from its transcript: the decisions taken, the action items with their owners, and the open questions.";

fn delivery(options: &Value) -> Result<Delivery, String> {
    match options["delivery"].as_str().unwrap_or("incoming_webhook") {
        "incoming_webhook" => Ok(Delivery::Webhook(
            options["webhook_secret"]
                .as_str()
                .unwrap_or("TEAMS_MEETINGS_WEBHOOK_URL")
                .to_owned(),
        )),
        "graph" => Ok(Delivery::Graph),
        other => Err(format!(
            "options.delivery: '{other}' is not incoming_webhook or graph"
        )),
    }
}

fn settings(options: &Value) -> Result<Settings, String> {
    let use_recordings = options["use_recordings"].as_bool().unwrap_or(false);
    let mut resources = vec!["communications/onlineMeetings/getAllTranscripts".to_owned()];
    if use_recordings {
        resources.push("communications/onlineMeetings/getAllRecordings".to_owned());
    }
    let minutes = |key: &str, default: u64| options[key].as_u64().unwrap_or(default) * 60;
    let public_url = options["public_url"].as_str().map(str::to_owned);
    if public_url
        .as_deref()
        .is_some_and(|u| !u.starts_with("https://"))
    {
        return Err("options.public_url must be an https URL".to_owned());
    }
    Ok(Settings {
        public_url,
        client_state_secret: options["client_state_secret"]
            .as_str()
            .unwrap_or("MSGRAPH_CLIENT_STATE")
            .to_owned(),
        resources,
        use_recordings,
        delivery: delivery(options)?,
        prompt: options["summary_prompt"]
            .as_str()
            .unwrap_or(DEFAULT_PROMPT)
            .to_owned(),
        renew_within_secs: minutes("renew_within_minutes", 60),
        // Graph keeps meeting artifact subscriptions at most 4230 minutes.
        extend_secs: minutes("extend_minutes", 4200),
        check_every_secs: options["renew_check_secs"].as_u64().unwrap_or(300),
    })
}

fn start(options: &Value) -> Result<Meetings, String> {
    let required = |key: &str| {
        options[key]
            .as_str()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("options.{key} is required"))
    };
    let (client_id, tenant) = (required("client_id")?, required("tenant_id")?);
    let mut listener_options = options.clone();
    if listener_options["client_state_secret"].is_null() {
        listener_options["client_state_secret"] = json!("MSGRAPH_CLIENT_STATE");
    }
    Ok(Meetings {
        settings: settings(options)?,
        graph: graph::Graph::new(options, client_id, tenant),
        listener: Listener::from_options(&listener_options)?,
        queue: Vec::new(),
        inbox: Vec::new(),
        checked: None,
        polls: 0,
    })
}

impl Meetings {
    fn poll(&mut self) -> String {
        self.polls += 1;
        for notification in std::mem::take(&mut self.queue) {
            self.notification(&notification);
        }
        if self.polls.is_multiple_of(REPLAY_POLLS) {
            self.replays();
        }
        let due = self
            .checked
            .is_none_or(|at| clock::elapsed(at, self.settings.check_every_secs));
        if due {
            self.checked = Some(clock::now_ms());
            for line in self.renew(false, None) {
                adapter_kit::host::info(&line);
            }
        }
        message::batch(&std::mem::take(&mut self.inbox))
    }
}

struct Adapter;

impl Guest for Adapter {
    fn descriptor() -> String {
        json!({
            "platform": "teams_meetings",
            "max_message_length": 28000,
            "length_unit": "chars",
            "splits_long_messages": false,
            "supports_edit": false,
            "supports_delete": false,
            "supports_typing": false,
            "supports_reactions": false,
            "supports_threads": false,
            "interactive": "none",
            // A recording standing in for a transcript is the pipeline's
            // own input, not media people send.
            "media_in": [],
            "media_out": [],
            "delivery_receipts": false,
            "read_receipts": false,
            "streaming_tts": false,
            "markup": "plain",
            "binds_http": true,
            "max_body_bytes": 1_048_576,
            "enforces_own_access_policy": "allowlist",
            "cli_commands": cli::commands(),
        })
        .to_string()
    }

    fn start(options: String) -> Result<(), String> {
        let options: Value = serde_json::from_str(&options).map_err(|e| e.to_string())?;
        *lock()? = Some(start(&options)?);
        Ok(())
    }

    fn stop() {
        if let Ok(mut meetings) = lock() {
            *meetings = None;
        }
    }

    fn poll() -> Result<String, String> {
        clock::tick();
        with(|m| Ok(m.poll()))
    }

    fn send(message: String) -> Result<String, String> {
        let value: Value = serde_json::from_str(&message).map_err(|e| format!("permanent: {e}"))?;
        if value["op"] == "cli" {
            let name = value["name"].as_str().unwrap_or_default().to_owned();
            let args: Vec<String> = value["args"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|a| a.as_str().map(str::to_owned))
                .collect();
            return with(|m| m.cli(&name, &args));
        }
        let out = Outgoing::parse(&message)?;
        if !out.op.is_empty() {
            return Err("unsupported".to_owned());
        }
        with(|m| m.deliver(&out.chat, &out.text))
    }

    fn chat_info(chat: String) -> Result<String, String> {
        Ok(message::chat_info(&chat, "dm"))
    }

    fn handle_http(request: HttpRequest) -> HttpResponse {
        let Ok(mut guard) = lock() else {
            return respond(503, "text/plain", "busy");
        };
        let Some(meetings) = guard.as_mut() else {
            return respond(503, "text/plain", "not started");
        };
        let (answer, admitted) = meetings.listener.handle(&request);
        meetings.queue.extend(admitted);
        answer
    }

    fn authorize_sender(inbound: String) -> String {
        let inbound: Value = serde_json::from_str(&inbound).unwrap_or_default();
        if inbound["sender"] == pipeline::SENDER {
            "allow"
        } else {
            "deny"
        }
        .to_owned()
    }
}

adapter_kit::bindings::export_adapter!(Adapter);
