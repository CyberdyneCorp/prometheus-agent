//! Chat controls of the conversation's platform (spec: messaging-gateway,
//! Text approval and busy-mode commands): `/busy [queue|steer|interrupt|
//! status]`. A mode is written to `gateway.platforms.<name>.busy.
//! input_mode` in the profile's `config.toml` through the validated config
//! writer, so it applies to the next inbound message and survives
//! restarts; `status` names the effective mode and where it comes from.

use pmt_config::schema::BusyInputMode;

use super::Lane;
use crate::messaging::key::Routing;

/// The name of a busy mode as configured.
fn mode_name(mode: BusyInputMode) -> &'static str {
    match mode {
        BusyInputMode::Queue => "queue",
        BusyInputMode::Steer => "steer",
        BusyInputMode::Interrupt => "interrupt",
    }
}

impl Lane {
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
