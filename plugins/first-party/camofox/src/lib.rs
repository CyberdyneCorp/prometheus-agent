//! The `camofox` catalog plugin (spec: plugin-system, Catalog plugins for
//! Home Assistant and Camofox): browser tools against a Camofox
//! anti-detection Firefox REST server at `plugins.entries.camofox.url`
//! (default `http://localhost:9377`).
//!
//! Each session drives its own tab (kept in plugin state under the
//! session id, with the session id as Camofox's `userId`, so sessions
//! never see each other's cookies). `camofox_open` opens the tab (or
//! navigates it), the other tools act on it; pages are read as Camofox's
//! accessibility snapshot whose element refs (`e12`) the click and type
//! tools take.

wit_bindgen::generate!({
    path: ["../../../wit/host", "../../../wit/messaging", "../../../wit/subagents", "../../../wit/tool"],
    world: "prometheus:tool/tool",
    generate_all,
});

use exports::prometheus::tool::plugin::{Guest, ToolSpec};
use prometheus::host::{config, http, session, state};
use serde_json::{Value, json};

const DEFAULT_URL: &str = "http://localhost:9377";
/// Most snapshot characters one answer carries.
const MAX_SNAPSHOT: usize = 20_000;

struct Camofox;

fn base_url() -> String {
    config::get("url")
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| DEFAULT_URL.to_owned())
        .trim_end_matches('/')
        .to_owned()
}

/// The session the call runs for (`default` outside one).
fn user() -> String {
    session::current()
        .session_id
        .unwrap_or_else(|| "default".to_owned())
}

fn tab_key() -> String {
    format!("tab:{}", user())
}

fn current_tab() -> Option<String> {
    state::get(&tab_key())
        .ok()
        .flatten()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

fn need_tab() -> Result<String, String> {
    current_tab()
        .ok_or_else(|| "no Camofox tab is open; call camofox_open with a url first".to_owned())
}

/// One request to the server; answers the JSON body.
fn call(method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
    let mut headers = Vec::new();
    let body = body.map(|v| {
        headers.push(("content-type".to_owned(), "application/json".to_owned()));
        v.to_string().into_bytes()
    });
    let url = format!("{}{path}", base_url());
    let response = http::send(&http::Request {
        method: method.to_owned(),
        url: url.clone(),
        headers,
        body,
    })
    .map_err(|e| format!("the Camofox server at {} is unreachable: {e:?}", base_url()))?;
    let text = String::from_utf8_lossy(&response.body).into_owned();
    if !(200..300).contains(&response.status) {
        let detail = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_else(|| text.chars().take(200).collect());
        return Err(format!(
            "Camofox {method} {path} answered {}: {detail}",
            response.status
        ));
    }
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).map_err(|e| format!("Camofox {method} {path}: bad JSON: {e}"))
}

fn text_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{key} is required"))
}

/// Only web pages: a `file:`, `about:` or `javascript:` URL would read the
/// server's own machine or run script outside the page.
fn web_url(url: &str) -> Result<(), String> {
    let lower = url.trim().to_ascii_lowercase();
    if lower.starts_with("https://") || lower.starts_with("http://") {
        Ok(())
    } else {
        Err("only http(s) URLs can be opened".to_owned())
    }
}

fn snapshot(tab: &str) -> Result<String, String> {
    let answer = call(
        "GET",
        &format!("/tabs/{tab}/snapshot?userId={}", user()),
        None,
    )?;
    let text = answer
        .get("snapshot")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let url = answer
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut page: String = text.chars().take(MAX_SNAPSHOT).collect();
    if text.chars().count() > MAX_SNAPSHOT {
        page.push_str("\n[snapshot cut at 20000 characters]");
    }
    Ok(if url.is_empty() {
        page
    } else {
        format!("{url}\n\n{page}")
    })
}

fn open(args: &Value) -> Result<String, String> {
    let url = text_arg(args, "url")?;
    web_url(url)?;
    let tab = match current_tab() {
        Some(tab) => {
            call(
                "POST",
                &format!("/tabs/{tab}/navigate"),
                Some(json!({"userId": user(), "url": url})),
            )?;
            tab
        }
        None => {
            let created = call(
                "POST",
                "/tabs",
                Some(json!({"userId": user(), "sessionKey": "prometheus", "url": url})),
            )?;
            let tab = created
                .get("tabId")
                .and_then(Value::as_str)
                .ok_or("Camofox opened no tab")?
                .to_owned();
            state::set(&tab_key(), tab.as_bytes()).map_err(|e| format!("{e:?}"))?;
            tab
        }
    };
    snapshot(&tab)
}

fn act(path: &str, mut body: Value) -> Result<String, String> {
    let tab = need_tab()?;
    body["userId"] = json!(user());
    call("POST", &format!("/tabs/{tab}/{path}"), Some(body))?;
    snapshot(&tab)
}

fn click(args: &Value) -> Result<String, String> {
    act("click", json!({"ref": text_arg(args, "ref")?}))
}

fn type_text(args: &Value) -> Result<String, String> {
    let submit = args.get("submit").and_then(Value::as_bool).unwrap_or(false);
    act(
        "type",
        json!({"ref": text_arg(args, "ref")?, "text": text_arg(args, "text")?, "pressEnter": submit}),
    )
}

fn scroll(args: &Value) -> Result<String, String> {
    let direction = args
        .get("direction")
        .and_then(Value::as_str)
        .unwrap_or("down");
    if !matches!(direction, "up" | "down") {
        return Err("direction must be up or down".to_owned());
    }
    act("scroll", json!({"direction": direction}))
}

fn close() -> Result<String, String> {
    let Some(tab) = current_tab() else {
        return Ok("no tab was open".to_owned());
    };
    let _ = state::delete(&tab_key());
    call("DELETE", &format!("/tabs/{tab}?userId={}", user()), None)?;
    Ok("closed the tab".to_owned())
}

fn spec(name: &str, description: &str, parameters: Value) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: description.to_owned(),
        parameters: parameters.to_string(),
    }
}

fn object(properties: Value, required: &[&str]) -> Value {
    json!({"type": "object", "additionalProperties": false, "properties": properties, "required": required})
}

impl Guest for Camofox {
    fn tools() -> Vec<ToolSpec> {
        let reference =
            json!({"type": "string", "description": "An element ref from the snapshot (e12)"});
        vec![
            spec(
                "camofox_open",
                "Open an http(s) page in this session's Camofox tab and read it (an accessibility snapshot with element refs).",
                object(json!({"url": {"type": "string"}}), &["url"]),
            ),
            spec(
                "camofox_snapshot",
                "Read the current page of this session's Camofox tab.",
                object(json!({}), &[]),
            ),
            spec(
                "camofox_click",
                "Click an element by its snapshot ref, then read the page.",
                object(json!({"ref": reference}), &["ref"]),
            ),
            spec(
                "camofox_type",
                "Type text into an element by its snapshot ref (submit presses Enter), then read the page.",
                object(
                    json!({"ref": reference, "text": {"type": "string"}, "submit": {"type": "boolean"}}),
                    &["ref", "text"],
                ),
            ),
            spec(
                "camofox_scroll",
                "Scroll the page up or down, then read it.",
                object(
                    json!({"direction": {"type": "string", "enum": ["up", "down"]}}),
                    &[],
                ),
            ),
            spec(
                "camofox_back",
                "Go back one page, then read it.",
                object(json!({}), &[]),
            ),
            spec(
                "camofox_close",
                "Close this session's Camofox tab.",
                object(json!({}), &[]),
            ),
        ]
    }

    fn call(name: String, arguments: String) -> Result<String, String> {
        let args: Value = serde_json::from_str(&arguments).unwrap_or(Value::Null);
        match name.as_str() {
            "camofox_open" => open(&args),
            "camofox_snapshot" => snapshot(&need_tab()?),
            "camofox_click" => click(&args),
            "camofox_type" => type_text(&args),
            "camofox_scroll" => scroll(&args),
            "camofox_back" => act("back", json!({})),
            "camofox_close" => close(),
            other => Err(format!("unknown tool {other}")),
        }
    }
}

export!(Camofox);
