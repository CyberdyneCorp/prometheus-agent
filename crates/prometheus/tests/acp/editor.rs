//! Task 66.4 editor fixture: content conversion with media staging,
//! metadata projection, editor-native file access and terminal delegation
//! (spec: acp-adapter).

use base64::Engine as _;
use serde_json::{Value, json};

use super::common::daemon;
use super::fixture::{Acp, Home};

fn fs_caps() -> Value {
    json!({"fs": {"readTextFile": true, "writeTextFile": true}})
}

/// The text of the editor's last update of tool call `id`.
fn call_text(acp: &Acp, session: &str, id: &str) -> String {
    acp.updates(session)
        .iter()
        .rev()
        .find(|u| u["toolCallId"] == id && u["sessionUpdate"] == "tool_call_update")
        .map(|u| {
            u["content"][0]["content"]["text"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .unwrap_or_default()
}

/// Scenario: Unsupported image type; a file link is inlined.
#[test]
fn an_svg_is_omitted_with_a_note_and_file_links_are_inlined() {
    let home = Home::new();
    std::fs::write(home.work.join("notes.md"), "remember the milk").unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let mut acp = Acp::spawn(home.path(), &[], &[], &[]);
    acp.initialize(&json!({}));
    let session = acp.new_session(&home.work);
    let svg = base64::engine::general_purpose::STANDARD
        .encode("<svg xmlns=\"http://www.w3.org/2000/svg\"/>");
    let link = url::Url::from_file_path(home.work.join("notes.md")).unwrap();
    let answer = acp.call(
        "session/prompt",
        &json!({"sessionId": session, "prompt": [
            {"type": "text", "text": "what is in here?"},
            {"type": "image", "mimeType": "image/svg+xml", "data": svg},
            {"type": "resource_link", "uri": link.as_str(), "name": "notes.md"}
        ]}),
    );
    assert_eq!(answer["result"]["stopReason"], "end_turn");
    let text = acp.agent_text(&session);
    assert!(
        text.contains("[Image omitted: unsupported image format]"),
        "{text}"
    );
    assert!(text.contains("what is in here?"), "{text}");
    assert!(text.contains("remember the milk"), "{text}");
    let many: Vec<Value> = (0..11)
        .map(|_| json!({"type": "resource_link", "uri": link.as_str(), "name": "n"}))
        .collect();
    let refused = acp.call(
        "session/prompt",
        &json!({"sessionId": session, "prompt": many}),
    );
    assert_eq!(refused["error"]["message"], "too many attachments (max 10)");
}

/// A remote image on a loopback address is not fetched: the prompt keeps a
/// text mention and the refusal is audited like every egress denial
/// (security-boundaries, In-process fetch SSRF protection).
#[test]
fn a_loopback_image_link_is_refused_and_audited() {
    let home = Home::new();
    let _daemon = daemon::start(home.path(), &[]);
    let mut acp = Acp::spawn(home.path(), &[], &[], &[]);
    acp.initialize(&json!({}));
    let session = acp.new_session(&home.work);
    let url = "http://127.0.0.1:9/shot.png";
    let answer = acp.call(
        "session/prompt",
        &json!({"sessionId": session, "prompt": [
            {"type": "text", "text": "look"},
            {"type": "image", "mimeType": "image/png", "uri": url}
        ]}),
    );
    assert_eq!(answer["result"]["stopReason"], "end_turn");
    let text = acp.agent_text(&session);
    assert!(text.contains(&format!("[Image attached: {url}]")), "{text}");
    let audit = std::fs::read_to_string(
        home.path()
            .join("profiles/default/logs/security-audit.jsonl"),
    )
    .unwrap();
    assert!(
        audit
            .lines()
            .any(|line| line.contains("\"kind\":\"egress_denied\"")
                && line.contains("127.0.0.1:9")
                && line.contains("acp (")),
        "{audit}"
    );
}

/// Scenario: Same title republished.
#[test]
fn a_title_is_shown_once_until_it_changes() {
    let home = Home::new();
    let _daemon = daemon::start(home.path(), &[]);
    let mut acp = Acp::spawn(home.path(), &[], &[], &[]);
    acp.initialize(&json!({}));
    let session = acp.new_session(&home.work);
    let titles = |acp: &Acp| {
        acp.updates(&session)
            .iter()
            .filter(|u| u["sessionUpdate"] == "session_info_update")
            .count()
    };
    for _ in 0..2 {
        let id = acp.send_prompt(&session, "/title Fix the build");
        acp.response(id);
        acp.pump(std::time::Duration::from_millis(500));
    }
    assert_eq!(titles(&acp), 1, "{:?}", acp.updates(&session));
    let id = acp.send_prompt(&session, "/title Ship it");
    acp.response(id);
    acp.wait_for("the new title", |f| {
        f["params"]["update"]["title"] == "Ship it"
    });
}

/// Scenario: Unsaved buffer. The editor's buffer is what the agent reads
/// and where its edits land; a client error is the tool's error; files
/// outside the editor cwd are read from disk.
#[test]
fn file_tools_under_the_editor_cwd_go_through_the_editor() {
    let home = Home::new();
    std::fs::create_dir_all(home.work.join("src")).unwrap();
    std::fs::write(home.work.join("src/main.rs"), "fn main() {} // on disk").unwrap();
    std::fs::write(home.work.join("src/broken.rs"), "x").unwrap();
    home.script(
        "[[rule]]\nuser = \"read\"\n[[rule.tool_calls]]\nid = \"call-read\"\nname = \"read_file\"\narguments = { path = \"src/main.rs\" }\n\n\
         [[rule]]\nuser = \"broken\"\n[[rule.tool_calls]]\nid = \"call-broken\"\nname = \"read_file\"\narguments = { path = \"src/broken.rs\" }\n\n\
         [[rule]]\nuser = \"write\"\n[[rule.tool_calls]]\nid = \"call-write\"\nname = \"write_file\"\narguments = { path = \"new.txt\", content = \"buffer notes\" }\n\n\
         [[rule]]\ntext = \"done\"\nrepeat = true\n",
    );
    let _daemon = daemon::start(home.path(), &[]);
    let mut acp = Acp::spawn(home.path(), &[], &[], &[]);
    acp.initialize(&fs_caps());
    let session = acp.new_session(&home.work);
    acp.ok(
        "session/set_mode",
        &json!({"sessionId": session, "modeId": "dont_ask"}),
    );
    acp.respond_with(Box::new(|request: &Value| {
        let path = request["params"]["path"].as_str().unwrap_or_default();
        match request["method"].as_str().unwrap_or_default() {
            "fs/read_text_file" if path.ends_with("main.rs") => {
                Ok(json!({"content": "fn main() { /* unsaved buffer */ }"}))
            }
            "fs/read_text_file" if path.ends_with("broken.rs") => {
                Err(json!({"code": -32603, "message": "the buffer is locked"}))
            }
            "fs/read_text_file" => Err(json!({"code": -32603, "message": "no such buffer"})),
            "fs/write_text_file" => Ok(json!(null)),
            _ => Err(json!({"code": -32601, "message": "not offered"})),
        }
    }));
    let id = acp.send_prompt(&session, "read");
    acp.response(id);
    assert!(
        call_text(&acp, &session, "call-read").contains("unsaved buffer"),
        "{:?}",
        acp.seen
    );
    let id = acp.send_prompt(&session, "broken");
    acp.response(id);
    assert!(call_text(&acp, &session, "call-broken").contains("the buffer is locked"));
    let id = acp.send_prompt(&session, "write");
    acp.response(id);
    let writes = acp.requests("fs/write_text_file");
    assert_eq!(writes.len(), 1, "{:?}", acp.seen);
    assert_eq!(writes[0]["params"]["content"], "buffer notes");
    assert!(
        !home.work.join("new.txt").exists(),
        "the edit landed in the editor's buffer, not on disk"
    );
}

/// The editor terminal answers of the fixture.
fn terminal_editor(request: &Value) -> Result<Value, Value> {
    match request["method"].as_str().unwrap_or_default() {
        "terminal/create" => Ok(json!({"terminalId": "term-1"})),
        "terminal/wait_for_exit" => Ok(json!({"exitCode": 0, "signal": null})),
        "terminal/output" => Ok(json!({"output": "test result: ok. 3 passed", "truncated": false})),
        "terminal/release" => Ok(json!(null)),
        _ => Err(json!({"code": -32601, "message": "not offered"})),
    }
}

fn terminal_script(home: &Home, command: &str) {
    home.script(&format!(
        "[[rule]]\nuser = \"run\"\n[[rule.tool_calls]]\nid = \"call-term\"\nname = \"terminal\"\narguments = {{ command = \"{command}\" }}\n\n[[rule]]\ntext = \"ran\"\nrepeat = true\n"
    ));
}

/// Scenario: Command in the editor terminal.
#[test]
fn an_unsandboxed_session_runs_commands_in_the_editor_terminal() {
    let home = Home::new();
    home.config(
        "\n[acp]\nuse_editor_terminal = true\n\n[security.sandbox]\nmode = \"off\"\nallow_degraded = [\"all\"]\n",
    );
    terminal_script(&home, "cargo test");
    let _daemon = daemon::start(home.path(), &[]);
    let mut acp = Acp::spawn(home.path(), &[], &[], &[]);
    acp.initialize(&json!({"terminal": true}));
    let session = acp.new_session(&home.work);
    acp.respond_with(Box::new(terminal_editor));
    let id = acp.send_prompt(&session, "run");
    assert_eq!(acp.response(id)["result"]["stopReason"], "end_turn");
    let created = acp.requests("terminal/create");
    assert_eq!(created.len(), 1, "{:?}", acp.seen);
    let args = &created[0]["params"]["args"];
    assert_eq!(args[args.as_array().unwrap().len() - 1], "cargo test");
    assert!(call_text(&acp, &session, "call-term").contains("test result: ok. 3 passed"));
}

/// Scenario: Sandboxed session ignores the editor terminal.
#[test]
fn a_sandboxed_session_runs_commands_in_the_daemon() {
    let home = Home::new();
    home.config("\n[acp]\nuse_editor_terminal = true\n");
    terminal_script(&home, "echo daemon-ran");
    let _daemon = daemon::start(home.path(), &[]);
    let mut acp = Acp::spawn(home.path(), &[], &[], &[]);
    acp.initialize(&json!({"terminal": true}));
    let session = acp.new_session(&home.work);
    acp.respond_with(Box::new(terminal_editor));
    let id = acp.send_prompt(&session, "run");
    assert_eq!(acp.response(id)["result"]["stopReason"], "end_turn");
    assert!(acp.requests("terminal/create").is_empty(), "{:?}", acp.seen);
    assert!(call_text(&acp, &session, "call-term").contains("daemon-ran"));
}
