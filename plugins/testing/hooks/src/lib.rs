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

wit_bindgen::generate!({
    path: ["../../../wit/host", "../../../wit/hook"],
    world: "prometheus:hook/hook",
    generate_all,
});

use exports::prometheus::hook::extensions::{self, Command};
use exports::prometheus::hook::plugin::{Approval, Guest, HookEvent, Reply};
use prometheus::host::{config, events, session, state};

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
        let mode = setting("mode");
        let (verb, rest) = mode.split_once(':').unwrap_or((mode.as_str(), ""));
        Ok(match verb {
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

export!(Hooks);
