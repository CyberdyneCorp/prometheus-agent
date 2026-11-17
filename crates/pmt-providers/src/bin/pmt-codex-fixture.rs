//! A fake Codex CLI for the Codex app-server runtime tests (built with the
//! crate, never shipped with Prometheus). `pmt-codex-fixture app-server`
//! speaks the app-server's JSON-RPC on stdio: it answers `initialize`,
//! `thread/start`, `plugin/list` and `turn/start`. A turn whose input
//! contains `run` asks to run `echo hi` and reports the command (declined
//! or completed with the answer it got); one containing `edit` reports a
//! file change; every turn then streams a thought and the message
//! `codex saw: <input>`, its token usage and `turn/completed`.
//!
//! Each process start, each request it receives and each approval answer
//! is appended as one JSON line to `codex-fixture.log` beside the
//! executable (tests run their own copy).

use std::io::{BufRead as _, Write};

use serde_json::{Value, json};

fn send(out: &mut impl Write, frame: &Value) {
    writeln!(out, "{frame}").expect("stdout");
    out.flush().expect("stdout");
}

fn notify(out: &mut impl Write, method: &str, params: &Value) {
    send(
        out,
        &json!({"jsonrpc": "2.0", "method": method, "params": params}),
    );
}

fn log(entry: &Value) {
    let exe = std::env::current_exe().expect("executable path");
    if let Some(path) = exe.parent().map(|dir| dir.join("codex-fixture.log")) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("fixture log");
        writeln!(file, "{entry}").expect("fixture log");
    }
}

struct Turn<'a, I: Iterator<Item = std::io::Result<String>>, W: Write> {
    out: &'a mut W,
    lines: &'a mut I,
    text: String,
}

impl<I: Iterator<Item = std::io::Result<String>>, W: Write> Turn<'_, I, W> {
    fn run(&mut self) {
        let turn = json!({"threadId": "thr-1", "turnId": "turn-1"});
        if self.text.contains("run") {
            self.command(&turn);
        }
        if self.text.contains("edit") {
            let item = json!({"type": "fileChange", "id": "chg-1", "status": "completed",
                "changes": [{"path": "notes.txt", "kind": {"type": "add"}, "diff": "+hello"}]});
            notify(self.out, "item/completed", &json!({"item": item}));
        }
        notify(
            self.out,
            "item/reasoning/textDelta",
            &json!({"delta": "thinking"}),
        );
        let message = format!("codex saw: {}", self.text);
        for delta in [&message[..message.len() / 2], &message[message.len() / 2..]] {
            notify(
                self.out,
                "item/agentMessage/delta",
                &json!({"itemId": "msg-1", "delta": delta}),
            );
        }
        let last = json!({"inputTokens": 120, "cachedInputTokens": 20, "outputTokens": 7, "reasoningOutputTokens": 2, "totalTokens": 127});
        notify(
            self.out,
            "thread/tokenUsage/updated",
            &json!({"tokenUsage": {"last": last, "total": last}}),
        );
        notify(
            self.out,
            "turn/completed",
            &json!({"turn": {"id": "turn-1", "status": "completed"}}),
        );
    }

    /// Asks to run `echo hi`, then reports the command as the answer
    /// decided.
    fn command(&mut self, turn: &Value) {
        let mut params = turn.clone();
        params["itemId"] = "cmd-1".into();
        params["command"] = "echo hi".into();
        params["cwd"] = "/work".into();
        send(
            self.out,
            &json!({"jsonrpc": "2.0", "id": "srv-1", "method": "item/commandExecution/requestApproval", "params": params}),
        );
        let answer = loop {
            let line = self.lines.next().expect("an answer").expect("stdin");
            let Ok(frame) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if frame["id"] == "srv-1" {
                break frame;
            }
        };
        let decision = answer["result"]["decision"]
            .as_str()
            .unwrap_or("none")
            .to_owned();
        log(&json!({"approval": decision}));
        let item = if decision == "accept" {
            json!({"type": "commandExecution", "id": "cmd-1", "command": "echo hi", "cwd": "/work",
                "status": "completed", "exitCode": 0, "aggregatedOutput": "hi\n"})
        } else {
            json!({"type": "commandExecution", "id": "cmd-1", "command": "echo hi", "cwd": "/work",
                "status": "declined"})
        };
        notify(self.out, "item/completed", &json!({"item": item}));
    }
}

fn main() {
    if std::env::args().nth(1).as_deref() != Some("app-server") {
        eprintln!("usage: pmt-codex-fixture app-server");
        std::process::exit(2);
    }
    log(&json!({"start": std::process::id()}));
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    let mut lines = stdin.lock().lines();
    let mut threads = 0;
    while let Some(Ok(line)) = lines.next() {
        let Ok(frame) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (Some(method), Some(id)) = (frame["method"].as_str(), frame.get("id").cloned()) else {
            continue;
        };
        let params = &frame["params"];
        log(&json!({"method": method, "params": params}));
        let result = match method {
            "initialize" => json!({"userAgent": "pmt-codex-fixture/0"}),
            "thread/start" => {
                threads += 1;
                json!({"thread": {"id": format!("thr-{threads}")}})
            }
            "plugin/list" => json!({"plugins": [
                {"name": "github", "installed": true, "available": true},
                {"name": "slack", "installed": false, "available": true},
                {"name": "retired", "installed": true, "available": false}
            ]}),
            "turn/start" => {
                send(
                    &mut out,
                    &json!({"jsonrpc": "2.0", "id": id, "result": {"turn": {"id": "turn-1", "status": "inProgress"}}}),
                );
                let text = params["input"][0]["text"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                Turn {
                    out: &mut out,
                    lines: &mut lines,
                    text,
                }
                .run();
                continue;
            }
            other => {
                send(
                    &mut out,
                    &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("{other} unknown")}}),
                );
                continue;
            }
        };
        send(
            &mut out,
            &json!({"jsonrpc": "2.0", "id": id, "result": result}),
        );
    }
}
