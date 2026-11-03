//! Test plugin: a hook callback driven by its settings. `hooks` (a comma
//! list) is what `hooks()` answers; `mode` picks the reply: `none`,
//! `block`, `approve`, `modify:<json>`, `context:<text>`, `big:<n>` (n
//! characters of context), `error` or `spin` (never returns). Every
//! payload received is stored under the state key `last`.
//!
//! Its `extensions` (hook world 1.1) are driven the same way: `sections`
//! (comma list of ids) rendered as `<id>:<section>` or, with
//! `section_len`, that many `s`; `commands` (comma list) answered as
//! `ran /<name> <args> in <session> from <surface> at <cwd>` (its
//! `session.info`), where command `emit` emits event `<args>`;
//! `subscribe` (comma list of patterns) subscribed when `hooks()` runs
//! (`load = spin` makes `hooks()` never return);
//! every event received is appended to the state key `events`, and
//! `event_mode = error` fails the delivery.
//!
//! `mode_<hook>` overrides `mode` for one hook, and adds: `redact:<field>`
//! (context = the payload's string `<field>` with every `sk-...` key
//! replaced by `[redacted]`), `upper:<field>` (that field upper-cased),
//! `continue:<message>` (`pre_verify` continuation) and `classify:<json>`
//! (as `modify`). Every payload is also kept under `last_<hook>`, and
//! each hook name is appended to `seen` (`<hook>;`).
//!
//! Its `surfaces` (hook world 1.2): `hold` (comma list) is what
//! `hold-stream` answers; `cli` (comma list) the `prometheus plugins run`
//! commands, where `fail` exits 3 with a message on stderr, `crash`
//! traps, and any other prints `ran <name> <args> as <profile> tty=<b>
//! cols=<n>`; `transports` (comma list) the approval transports, answered
//! by `transport_mode`: `once` or `always` (that choice, bound to the
//! request), `stale` (a wrong digest), `error`, or `spin`. Commands
//! `post <text>` (posts, then edits three times to `<text> 1..3`),
//! `inject <session>`, `react <platform> <chat> <message>` and
//! `retitle <platform> <chat> <thread>` exercise the messaging imports.

wit_bindgen::generate!({
    path: ["../../../wit/host", "../../../wit/messaging", "../../../wit/hook"],
    world: "prometheus:hook/hook",
    generate_all,
});

use exports::prometheus::hook::extensions::{self, Command};
use exports::prometheus::hook::plugin::{Approval, Guest, HookEvent, Reply};
use exports::prometheus::hook::surfaces::{
    self, ApprovalRequest, ApprovalResponse, CliCommand, CliOutput, Terminal,
};
use prometheus::host::{config, events, session, state};
use prometheus::messaging::{conversation, inject, platform};

struct Hooks;

/// A string setting (stored as a JSON string).
fn setting(key: &str) -> String {
    config::get(key)
        .ok()
        .flatten()
        .map(|v| v.trim_matches('"').to_owned())
        .unwrap_or_default()
}

fn list(key: &str) -> Vec<String> {
    setting(key)
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

impl Guest for Hooks {
    fn hooks() -> Vec<String> {
        if setting("load") == "spin" {
            loop {
                core::hint::black_box(());
            }
        }
        for pattern in list("subscribe") {
            let _ = events::subscribe(&pattern);
        }
        list("hooks")
    }

    fn on_hook(event: HookEvent) -> Result<Reply, String> {
        let _ = state::set("last", event.payload.as_bytes());
        let _ = state::set(&format!("last_{}", event.hook), event.payload.as_bytes());
        let mut seen = state::get("seen").ok().flatten().unwrap_or_default();
        seen.extend_from_slice(format!("{};", event.hook).as_bytes());
        let _ = state::set("seen", &seen);
        let mut mode = setting(&format!("mode_{}", event.hook));
        if mode.is_empty() {
            mode = setting("mode");
        }
        let (verb, rest) = mode.split_once(':').unwrap_or((mode.as_str(), ""));
        Ok(match verb {
            "redact" => Reply::Context(redact(&field(&event.payload, rest))),
            "upper" => Reply::Context(field(&event.payload, rest).to_uppercase()),
            "continue" => Reply::Modify(
                serde_json::json!({"action": "continue", "message": rest}).to_string(),
            ),
            "classify" => Reply::Modify(rest.replace('\'', "\"")),
            "block" => Reply::Block(format!("blocked by {}", setting("name"))),
            "approve" => Reply::Approve(Approval {
                message: "needs a look".to_owned(),
                rule_key: Some("test-rule".to_owned()),
            }),
            "modify" => Reply::Modify(rest.replace('\'', "\"")),
            "context" => Reply::Context(rest.to_owned()),
            "big" => Reply::Context("x".repeat(rest.parse().unwrap_or(0))),
            "error" => return Err("hook failed on purpose".to_owned()),
            "spin" => loop {
                core::hint::black_box(());
            },
            _ => Reply::None,
        })
    }
}

impl extensions::Guest for Hooks {
    fn sections() -> Vec<String> {
        list("sections")
    }

    fn render_section(id: String) -> Result<String, String> {
        match setting("section_len").parse::<usize>() {
            Ok(n) => Ok("s".repeat(n)),
            Err(_) => Ok(format!("{id}:{}", setting("section"))),
        }
    }

    fn commands() -> Vec<Command> {
        list("commands")
            .into_iter()
            .map(|name| Command {
                description: format!("test command {name}"),
                name,
                surfaces: Vec::new(),
            })
            .collect()
    }

    fn run_command(name: String, args: String) -> Result<String, String> {
        if let Some(answer) = messaging(&name, &args) {
            return Ok(answer);
        }
        if name == "emit" {
            events::emit(&args, "{}").map_err(|e| format!("{e:?}"))?;
        }
        let info = session::current();
        let id = info.session_id.unwrap_or_default();
        let cwd = info.cwd.unwrap_or_else(|| "-".to_owned());
        Ok(format!(
            "ran /{name} {args} in {id} from {} at {cwd}",
            info.surface
        ))
    }

    fn on_event(name: String, payload: String) -> Result<(), String> {
        if setting("event_mode") == "error" {
            return Err("subscriber failed on purpose".to_owned());
        }
        let mut seen = state::get("events").ok().flatten().unwrap_or_default();
        seen.extend_from_slice(format!("{name}={payload};").as_bytes());
        state::set("events", &seen).map_err(|e| format!("{e:?}"))
    }
}

/// The payload's string field `name`.
fn field(payload: &str, name: &str) -> String {
    serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .and_then(|v| v.get(name).and_then(|f| f.as_str().map(str::to_owned)))
        .unwrap_or_default()
}

/// `text` with every `sk-<alphanumerics>` key replaced by `[redacted]`.
fn redact(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("sk-") {
        out.push_str(&rest[..at]);
        let tail = &rest[at + 3..];
        let end = tail
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(tail.len());
        out.push_str("[redacted]");
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

impl surfaces::Guest for Hooks {
    fn hold_stream() -> Vec<String> {
        list("hold")
    }

    fn cli_commands() -> Vec<CliCommand> {
        list("cli")
            .into_iter()
            .map(|name| CliCommand {
                description: format!("test cli command {name}"),
                name,
            })
            .collect()
    }

    fn run_cli(name: String, args: Vec<String>, terminal: Terminal) -> Result<CliOutput, String> {
        match name.as_str() {
            "fail" => Ok(CliOutput {
                code: 3,
                stdout: Vec::new(),
                stderr: b"failed on purpose\n".to_vec(),
            }),
            "crash" => panic!("crashed on purpose"),
            _ => Ok(CliOutput {
                code: 0,
                stdout: format!(
                    "ran {name} {} as {} tty={} cols={}\n",
                    args.join(" "),
                    session::current().profile,
                    terminal.tty,
                    terminal.columns
                )
                .into_bytes(),
                stderr: Vec::new(),
            }),
        }
    }

    fn transports() -> Vec<String> {
        list("transports")
    }

    fn present_approval(_name: String, request: ApprovalRequest) -> Result<ApprovalResponse, String> {
        let _ = state::set("approval", format!("{}|{}", request.command, request.choices.join(",")).as_bytes());
        let mode = setting("transport_mode");
        let mut digest = request.digest.clone();
        let choice = match mode.as_str() {
            "error" => return Err("transport failed on purpose".to_owned()),
            "spin" => loop {
                core::hint::black_box(());
            },
            "stale" => {
                digest = "stale".to_owned();
                "once".to_owned()
            }
            other => other.to_owned(),
        };
        Ok(ApprovalResponse {
            id: request.id,
            digest,
            choice,
        })
    }
}

/// The messaging commands: what each call answered, as text.
fn messaging(name: &str, args: &str) -> Option<String> {
    let words: Vec<&str> = args.split_whitespace().collect();
    let word = |i: usize| words.get(i).copied().unwrap_or_default().to_owned();
    Some(match name {
        "post" => match conversation::post(args) {
            Ok(posted) => {
                let mut last = format!("posted fallback={}", posted.fallback);
                for n in 1..=3 {
                    last = match conversation::edit(&posted.handle, &format!("{args} {n}")) {
                        Ok(edited) => format!("edited fallback={}", edited.fallback),
                        Err(e) => format!("edit failed {}", e.name()),
                    };
                }
                last
            }
            Err(e) => format!("post failed {}", e.name()),
        },
        "inject" => format!(
            "injected={}",
            inject::inject("from a plugin", "user", &inject::Target::SessionKey(word(0)))
        ),
        "react" => outcome(&platform::add_reaction(&word(0), &word(1), &word(2), "eyes")),
        "retitle" => outcome(&platform::set_thread_title(&word(0), &word(1), &word(2), "renamed")),
        _ => return None,
    })
}

fn outcome(outcome: &platform::Outcome) -> String {
    format!(
        "ok={} action={} error={} detail={}",
        outcome.ok,
        outcome.action,
        outcome.error.clone().unwrap_or_default(),
        outcome.detail.clone().unwrap_or_default()
    )
}

export!(Hooks);
