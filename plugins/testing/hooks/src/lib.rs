//! Test plugin: a hook callback driven by its settings. `hooks` (a comma
//! list) is what `hooks()` answers; `mode` picks the reply: `none`,
//! `block`, `approve`, `modify:<json>`, `context:<text>`, `big:<n>` (n
//! characters of context), `error` or `spin` (never returns). Every
//! payload received is stored under the state key `last`.

wit_bindgen::generate!({
    path: ["../../../wit/host", "../../../wit/hook"],
    world: "prometheus:hook/hook",
    generate_all,
});

use exports::prometheus::hook::plugin::{Approval, Guest, HookEvent, Reply};
use prometheus::host::{config, state};

struct Hooks;

/// A string setting (stored as a JSON string).
fn setting(key: &str) -> String {
    config::get(key)
        .ok()
        .flatten()
        .map(|v| v.trim_matches('"').to_owned())
        .unwrap_or_default()
}

impl Guest for Hooks {
    fn hooks() -> Vec<String> {
        setting("hooks")
            .split(',')
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect()
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

export!(Hooks);
