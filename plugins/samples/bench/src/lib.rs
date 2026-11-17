//! Sample `bench` plugin: a two-task ledger benchmark (`bench:<plugin
//! id>`). `bench.prepare` writes a note into the task workspace through
//! the `files` import, after trying to push a score to the host (refused
//! with -32002 during `prometheus eval run`); `bench.deliverables` records
//! in the run directory every method it was asked so far and that
//! refusal; `bench.score` grades `deliverables/answer.md` (and gives task
//! `e2` a null score: an ungraded row).

#[allow(warnings)]
mod bindings;

use std::cell::RefCell;

use bindings::exports::prometheus::bench::plugin::{Guest, RpcError};
use bindings::prometheus::bench::{files, requests};
use serde_json::{Value, json};

thread_local! {
    /// The methods asked of this instance, in order.
    static CALLS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    /// The code the last score push was refused with.
    static PUSH: RefCell<Option<i64>> = const { RefCell::new(None) };
}

struct Sample;

fn error(code: i64, message: &str) -> RpcError {
    RpcError {
        code,
        message: message.to_owned(),
    }
}

fn ledger(task: &str) -> Option<&'static str> {
    match task {
        "e1" => Some("+100.00\n-14.85\n"),
        "e2" => Some("+20.25\n"),
        _ => None,
    }
}

fn prepare(task: &str) -> Result<Value, RpcError> {
    let push = json!({"task_id": task, "n_passed": 1, "n_criteria": 1}).to_string();
    if let Err(refused) = requests::request("bench.report_score", &push) {
        PUSH.with(|p| *p.borrow_mut() = Some(refused.code));
    }
    files::write("notes.txt", b"prepared by the adapter\n").map_err(|e| error(-32000, &e))?;
    Ok(json!({}))
}

fn deliverables() -> Result<Value, RpcError> {
    let calls = CALLS.with(|c| c.borrow().join("\n"));
    let push = PUSH.with(|p| p.borrow().map_or("none".to_owned(), |c| c.to_string()));
    let text = format!("{calls}\npush: {push}\n");
    files::write("adapter-calls.txt", text.as_bytes()).map_err(|e| error(-32000, &e))?;
    Ok(json!({}))
}

fn score(task: &str) -> Result<Value, RpcError> {
    if task == "e2" {
        return Ok(Value::Null);
    }
    let answer = files::read("deliverables/answer.md").map_err(|e| error(-32000, &e))?;
    let passed = String::from_utf8_lossy(&answer).contains("85.15");
    Ok(json!({"n_passed": u32::from(passed), "n_criteria": 1}))
}

fn answer(method: &str, params: &Value) -> Result<Value, RpcError> {
    let task = params["task_id"].as_str().unwrap_or_default();
    match method {
        "bench.describe" => Ok(json!({
            "name": "example", "version": "1.0", "domains": ["accounting"],
            "leak_globs": ["answers*"],
        })),
        "bench.tasks" if params["split"] == "dev" => Ok(json!(["e1"])),
        "bench.tasks" => Ok(json!(["e1", "e2"])),
        "bench.task" => {
            let ledger = ledger(task).ok_or_else(|| error(-32602, "unknown task"))?;
            Ok(json!({
                "prompt": format!("sum ledger {task}"), "domain": "accounting",
                "timeout_secs": 60, "files": [{"path": "ledger.txt", "content": ledger}],
            }))
        }
        "bench.prepare" => prepare(task),
        "bench.deliverables" => deliverables(),
        "bench.preflight" => Ok(json!({"ok": true})),
        "bench.score" => score(task),
        other => Err(error(-32601, &format!("method not found: {other}"))),
    }
}

impl Guest for Sample {
    fn call(method: String, params: String) -> Result<String, RpcError> {
        CALLS.with(|c| c.borrow_mut().push(method.clone()));
        let params: Value = serde_json::from_str(&params).unwrap_or(Value::Null);
        answer(&method, &params).map(|v| v.to_string())
    }
}

bindings::export!(Sample with_types_in bindings);
