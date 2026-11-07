//! Test plugin: one tool per host interface, so tests can check what each
//! import does with and without a grant. Arguments are `{"v": "<text>"}`.

wit_bindgen::generate!({
    path: ["../../../wit/host", "../../../wit/messaging", "../../../wit/subagents", "../../../wit/tool"],
    world: "prometheus:tool/tool",
    generate_all,
});

use exports::prometheus::tool::plugin::{Guest, ToolSpec};
use prometheus::host::types::HostError;
use prometheus::host::{config, events, http, llm, secrets, session, state, tools};
use prometheus::subagents::lifecycle::{self, Failure};

struct Probe;

fn failure(err: Failure) -> String {
    match err {
        Failure::Denied(m) | Failure::Refused(m) => m,
        Failure::UnknownHandle => "unknown_handle".to_owned(),
    }
}

/// Launches a child with goal `goal` and waits up to 30 s for it: its
/// state and result.
fn subagent(goal: &str) -> Result<String, String> {
    let handle = lifecycle::launch(&format!(r#"{{"goal":"{goal}"}}"#)).map_err(failure)?;
    let result = lifecycle::wait(&handle, 30_000).map_err(failure)?;
    let state = lifecycle::status(&handle).map_err(failure)?;
    Ok(format!("{state:?}|{}", result.unwrap_or_default()))
}

/// Launches a child, then asks for the status of its handle with the last
/// character of the signature changed.
fn forged_handle(goal: &str) -> Result<String, String> {
    let mut handle = lifecycle::launch(&format!(r#"{{"goal":"{goal}"}}"#)).map_err(failure)?;
    let last = handle.pop().unwrap_or('0');
    handle.push(if last == '0' { '1' } else { '0' });
    let _ = lifecycle::cancel(&handle);
    lifecycle::status(&handle).map(|s| format!("{s:?}")).map_err(failure)
}

const TOOLS: &[&str] = &[
    "secret", "fetch", "put", "get", "session", "emit", "llm", "llm_model", "dispatch", "setting",
    "subagent", "forged_handle",
];

/// The `v` string of `{"v": "..."}` (test arguments never hold quotes).
fn arg(arguments: &str) -> String {
    arguments
        .split_once(r#""v":""#)
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(v, _)| v.to_owned())
        .unwrap_or_default()
}

fn text(err: HostError) -> String {
    match err {
        HostError::Denied(m) | HostError::Invalid(m) | HostError::Failed(m) => m,
    }
}

fn ask(model: Option<String>) -> Result<String, String> {
    let request = llm::Request {
        messages: vec![llm::Message {
            role: "user".to_owned(),
            content: "hi".to_owned(),
        }],
        model,
        provider: None,
        max_tokens: None,
    };
    llm::complete(&request).map_err(text)
}

impl Guest for Probe {
    fn tools() -> Vec<ToolSpec> {
        TOOLS
            .iter()
            .map(|name| ToolSpec {
                name: (*name).to_owned(),
                description: format!("probe {name}"),
                parameters: r#"{"type":"object","properties":{"v":{"type":"string"}}}"#.to_owned(),
            })
            .collect()
    }

    fn call(name: String, arguments: String) -> Result<String, String> {
        let v = arg(&arguments);
        match name.as_str() {
            "secret" => secrets::get(&v).map_err(text),
            "fetch" => {
                let request = http::Request {
                    method: "GET".to_owned(),
                    url: v,
                    headers: Vec::new(),
                    body: None,
                };
                http::send(&request)
                    .map(|r| format!("{} {}", r.status, String::from_utf8_lossy(&r.body)))
                    .map_err(text)
            }
            "put" => state::set("k", v.as_bytes()).map(|()| "stored".to_owned()).map_err(text),
            "get" => state::get("k")
                .map(|v| v.map_or_else(|| "<none>".to_owned(), |b| String::from_utf8_lossy(&b).into_owned()))
                .map_err(text),
            "session" => {
                let info = session::current();
                Ok(format!(
                    "{}|{}|{}|{}",
                    info.profile,
                    info.surface,
                    info.session_id.unwrap_or_default(),
                    info.cwd.unwrap_or_default()
                ))
            }
            "emit" => events::emit(&v, "{}").map(|()| "emitted".to_owned()).map_err(text),
            "llm" => ask(None),
            "llm_model" => ask(Some(v)),
            "dispatch" => tools::dispatch(&v, "{}").map_err(text),
            "setting" => config::get(&v)
                .map(|s| s.unwrap_or_else(|| "<unset>".to_owned()))
                .map_err(text),
            "subagent" => subagent(&v),
            "forged_handle" => forged_handle(&v),
            other => Err(format!("no tool {other}")),
        }
    }
}

export!(Probe);
