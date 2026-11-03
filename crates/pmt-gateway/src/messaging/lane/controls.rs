//! Chat controls of the profile's platforms (spec: messaging-gateway,
//! Text approval and busy-mode commands; Adapter operations from chat):
//!
//! - `/busy [queue|steer|interrupt|status]`: a mode is written to
//!   `gateway.platforms.<name>.busy.input_mode` in the profile's
//!   `config.toml` through the validated config writer, so it applies to
//!   the next inbound message and survives restarts; `status` names the
//!   effective mode and where it comes from;
//! - `/platform list|pause <name>|resume <name>` (admin tier; `list` is
//!   `/platforms`).

use pmt_config::schema::BusyInputMode;

use super::Lane;
use crate::messaging::key::Routing;
use crate::messaging::operate;

/// The name of a busy mode as configured.
fn mode_name(mode: BusyInputMode) -> &'static str {
    match mode {
        BusyInputMode::Queue => "queue",
        BusyInputMode::Steer => "steer",
        BusyInputMode::Interrupt => "interrupt",
    }
}

impl Lane {
    /// `/platform list | pause <name> | resume <name>`.
    pub(super) async fn platform_command(&self, routing: &Routing, args: &str) {
        let words: Vec<&str> = args.split_whitespace().collect();
        let shared = &self.shared;
        let reply = match words.as_slice() {
            [] | ["list"] => operate::listing(shared, &self.profile),
            ["pause", name] => {
                let actor = format!("{}:{}", routing.platform, routing.sender_id);
                match operate::pause(shared, &self.profile, &name.to_lowercase(), &actor) {
                    Ok(true) => format!(
                        "Paused {name}: its messages are acknowledged and dropped until /platform resume {name}."
                    ),
                    Ok(false) => format!("{name} is already paused."),
                    Err(err) => err.message().to_owned(),
                }
            }
            ["resume", name] => {
                match operate::resume(shared, &self.profile, &name.to_lowercase()).await {
                    Ok(status) => format!("Resumed. {}", operate::platform_line(&status)),
                    Err(err) => err.message().to_owned(),
                }
            }
            _ => "Usage: /platform [list | pause <name> | resume <name>]".to_owned(),
        };
        self.say(&reply);
    }

    /// `/footer [on|off|status]`: the chat's runtime footer, stored with
    /// the chat (the display policy's `runtime_footer` applies until set).
    pub(super) fn footer_command(&self, routing: &Routing, args: &str) {
        let Some(store) = self.shared.store(&self.profile) else {
            return;
        };
        let chosen = match args.trim() {
            "on" => Some(true),
            "off" => Some(false),
            "" | "status" => {
                let stored = store
                    .chat_prefs(&routing.platform, &routing.chat_id)
                    .unwrap_or_default()
                    .footer;
                let on = stored.unwrap_or_else(|| self.display().runtime_footer);
                let source = if stored.is_some() {
                    "this chat"
                } else {
                    "display policy"
                };
                self.say(&format!(
                    "Footer: {} ({source})",
                    if on { "on" } else { "off" }
                ));
                return;
            }
            _ => {
                self.say("Usage: /footer [on | off | status]");
                return;
            }
        };
        match store.set_chat_footer(&routing.platform, &routing.chat_id, chosen) {
            Ok(()) => self.say(&format!(
                "Footer {} for this chat.",
                if chosen == Some(true) { "on" } else { "off" }
            )),
            Err(err) => self.say(err.message()),
        }
    }

    /// `/busy [queue|steer|interrupt|status]`.
    pub(super) fn busy_command(&self, routing: &Routing, args: &str) {
        match args.trim() {
            "" | "status" => self.busy_status(routing),
            mode @ ("queue" | "steer" | "interrupt") => self.set_busy(routing, mode),
            _ => self.say("Usage: /busy [queue | steer | interrupt | status]"),
        }
    }

    fn busy_status(&self, routing: &Routing) {
        let Some(scope) = self.config() else {
            return;
        };
        let config = scope.config();
        let platform = config.gateway.platform(&routing.platform);
        let reply = match platform.busy.input_mode {
            Some(mode) => format!("{} (platform override)", mode_name(mode)),
            None => format!("{} (busy.input_mode)", mode_name(config.busy.input_mode)),
        };
        self.say(&reply);
    }

    fn set_busy(&self, routing: &Routing, mode: &str) {
        let Some(scope) = self.config() else {
            return;
        };
        let key = format!("gateway.platforms.{}.busy.input_mode", routing.platform);
        let value = serde_json::to_string(mode).expect("a string serializes");
        match scope.profile().config_file().set(&key, &value) {
            Ok(()) => {
                tracing::info!(
                    profile = self.profile,
                    platform = routing.platform,
                    mode,
                    "busy mode set"
                );
                self.say(&format!(
                    "Busy mode on {} is now {mode}; it applies from the next message.",
                    routing.platform
                ));
            }
            Err(err) => self.say(&format!("Busy mode not set: {}", err.message())),
        }
    }
}
