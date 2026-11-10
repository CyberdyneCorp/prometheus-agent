//! A stand-in `llama-server` for tests (never shipped): it speaks the parts
//! of the router-mode HTTP API Prometheus uses (`/health`, `/props`,
//! `/v1/models`, `/models`, `/models/load`, `/models/unload`,
//! `/v1/chat/completions` with and without streaming), loads a model on
//! its first request, and with `--sleep-idle-seconds` unloads a model idle
//! that long. It appends one line per event (`start <args>`, `load
//! <model>`, `unload <model>`, `request <model>`) to `PMT_LLAMA_STUB_LOG`,
//! else to `pmt-llama-stub.log` beside its `--models-preset`, so tests can
//! see what the real server would have done.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

#[derive(Default)]
struct State {
    /// Known models and when each was last used (None: unloaded).
    models: BTreeMap<String, Option<Instant>>,
    context: u64,
}

type Shared = Arc<Mutex<State>>;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1).cloned())
    };
    let port: u16 = flag("--port").and_then(|p| p.parse().ok()).unwrap_or(8080);
    let sleep: Option<u64> = flag("--sleep-idle-seconds")
        .and_then(|s| s.parse::<i64>().ok())
        .and_then(|s| u64::try_from(s).ok());
    let beside_preset = flag("--models-preset")
        .map(|p| std::path::Path::new(&p).with_file_name("pmt-llama-stub.log"));
    let _ = LOG.set(
        std::env::var_os("PMT_LLAMA_STUB_LOG")
            .map(Into::into)
            .or(beside_preset),
    );
    let mut state = State {
        context: 65_536,
        ..State::default()
    };
    match flag("--models-preset") {
        Some(preset) => {
            let text = std::fs::read_to_string(preset).unwrap_or_default();
            for line in text.lines() {
                if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']'))
                    && name != "*"
                {
                    state.models.insert(name.to_owned(), None);
                }
            }
        }
        None => {
            state.models.insert(
                flag("--alias").unwrap_or_else(|| "external".to_owned()),
                None,
            );
        }
    }
    log(&format!("start {}", args.join(" ")));
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind the stub port");
    let shared: Shared = Arc::new(Mutex::new(state));
    if let Some(secs) = sleep {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || idle_unloader(&shared, Duration::from_secs(secs)));
    }
    for stream in listener.incoming().flatten() {
        let shared = Arc::clone(&shared);
        std::thread::spawn(move || {
            let _ = serve(stream, &shared);
        });
    }
}

fn idle_unloader(shared: &Shared, idle: Duration) {
    loop {
        std::thread::sleep(Duration::from_millis(100));
        let mut state = shared.lock().unwrap();
        for (name, used) in &mut state.models {
            if used.is_some_and(|t| t.elapsed() >= idle) {
                *used = None;
                log(&format!("unload {name}"));
            }
        }
    }
}

static LOG: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();

fn log(line: &str) {
    if let Some(path) = LOG.get().and_then(Option::as_ref)
        && let Ok(mut file) = std::fs::File::options()
            .create(true)
            .append(true)
            .open(path)
    {
        let _ = writeln!(file, "{line}");
    }
}

struct Request {
    method: String,
    path: String,
    query: String,
    body: Value,
}

fn read_request(stream: &TcpStream) -> Option<Request> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    let mut length = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).ok()?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    Some(Request {
        method,
        path: path.to_owned(),
        query: query.to_owned(),
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    })
}

fn serve(mut stream: TcpStream, shared: &Shared) -> std::io::Result<()> {
    let Some(request) = read_request(&stream) else {
        return Ok(());
    };
    let model = request
        .body
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| query_model(&request.query));
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/health") => respond(&mut stream, 200, &json!({"status": "ok"})),
        ("GET", "/props") => {
            let state = shared.lock().unwrap();
            let sleeping = model
                .as_ref()
                .and_then(|m| state.models.get(m))
                .is_some_and(Option::is_none);
            let body = json!({
                "default_generation_settings": {"n_ctx": state.context},
                "is_sleeping": sleeping,
            });
            drop(state);
            respond(&mut stream, 200, &body)
        }
        ("GET", "/v1/models") => {
            let state = shared.lock().unwrap();
            let data: Vec<Value> = state
                .models
                .keys()
                .map(|m| json!({"id": m, "object": "model"}))
                .collect();
            drop(state);
            respond(&mut stream, 200, &json!({"object": "list", "data": data}))
        }
        ("GET", "/models") => {
            let state = shared.lock().unwrap();
            let data: Vec<Value> = state
                .models
                .iter()
                .map(|(m, used)| {
                    let status = if used.is_some() { "loaded" } else { "unloaded" };
                    json!({"id": m, "status": {"value": status}})
                })
                .collect();
            drop(state);
            respond(&mut stream, 200, &json!({"data": data}))
        }
        ("POST", "/models/load") => {
            touch(shared, model.as_deref());
            respond(&mut stream, 200, &json!({"success": true}))
        }
        ("POST", "/models/unload") => {
            if let Some(m) = &model
                && let Some(used) = shared.lock().unwrap().models.get_mut(m)
            {
                *used = None;
                log(&format!("unload {m}"));
            }
            respond(&mut stream, 200, &json!({"success": true}))
        }
        ("POST", "/v1/chat/completions") => chat(&mut stream, shared, model, &request.body),
        _ => respond(
            &mut stream,
            404,
            &json!({"error": {"message": "not found"}}),
        ),
    }
}

fn query_model(query: &str) -> Option<String> {
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix("model="))
        .map(|m| m.replace("%2F", "/").replace("%3A", ":"))
}

/// Marks `model` used, loading it first when it is not resident.
fn touch(shared: &Shared, model: Option<&str>) -> bool {
    let Some(model) = model else {
        return false;
    };
    let mut state = shared.lock().unwrap();
    let Some(used) = state.models.get_mut(model) else {
        return false;
    };
    if used.is_none() {
        log(&format!("load {model}"));
    }
    *used = Some(Instant::now());
    true
}

fn chat(
    stream: &mut TcpStream,
    shared: &Shared,
    model: Option<String>,
    body: &Value,
) -> std::io::Result<()> {
    if !touch(shared, model.as_deref()) {
        return respond(
            stream,
            404,
            &json!({"error": {"message": "model not found"}}),
        );
    }
    let model = model.unwrap_or_default();
    log(&format!("request {model}"));
    let text = format!("stub answer from {model}");
    let usage = json!({"prompt_tokens": 12, "completion_tokens": 4, "total_tokens": 16});
    if body.get("stream").and_then(Value::as_bool) != Some(true) {
        return respond(
            stream,
            200,
            &json!({
                "id": "chatcmpl-stub", "object": "chat.completion", "created": 0, "model": model,
                "choices": [{"index": 0, "message": {"role": "assistant", "content": text}, "finish_reason": "stop"}],
                "usage": usage,
            }),
        );
    }
    let chunk = |delta: Value, finish: Value| {
        json!({"id": "chatcmpl-stub", "object": "chat.completion.chunk", "created": 0, "model": model,
               "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    };
    let mut events = vec![
        chunk(json!({"role": "assistant", "content": text}), Value::Null),
        chunk(json!({}), json!("stop")),
    ];
    events.push(json!({"id": "chatcmpl-stub", "object": "chat.completion.chunk", "created": 0, "model": model, "choices": [], "usage": usage}));
    let mut out = String::new();
    for event in events {
        let _ = write!(out, "data: {event}\n\n");
    }
    out.push_str("data: [DONE]\n\n");
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
        out.len()
    )
}

fn respond(stream: &mut TcpStream, code: u16, body: &Value) -> std::io::Result<()> {
    let text = body.to_string();
    let reason = if code == 200 { "OK" } else { "Error" };
    write!(
        stream,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    )
}
