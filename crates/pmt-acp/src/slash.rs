//! Slash commands in editor prompts (spec: acp-adapter, Slash commands are
//! answered or refused, never failed): prompt text is a command only when
//! its first token names a command the daemon registers for the `acp`
//! surface; a path at the start of a line (`/var/log/x`) or an unknown
//! `/word` is prompt text.

use pmt_rpc::contracts::{CommandEntry, CommandSource, Disposition};
use serde_json::{Value, json};

/// Whether an editor may run `command`: listed, and run by the daemon
/// (client-side commands of other surfaces mean nothing in an editor).
fn runnable(command: &CommandEntry) -> bool {
    command.disposition != Disposition::Hidden && command.source != CommandSource::Client
}

/// The registered command `text` invokes and its arguments.
pub(crate) fn command_of<'a>(
    text: &'a str,
    commands: &[CommandEntry],
) -> Option<(String, &'a str)> {
    let rest = text.trim_start().strip_prefix('/')?;
    let (token, args) = match rest.split_once(char::is_whitespace) {
        Some((token, args)) => (token, args.trim()),
        None => (rest, ""),
    };
    if token.is_empty() || token.contains('/') {
        return None;
    }
    commands
        .iter()
        .filter(|c| runnable(c))
        .find(|c| c.name == token || c.aliases.iter().any(|a| a == token))
        .map(|c| (c.name.clone(), args))
}

/// The `available_commands_update` advertising exactly `commands` (the
/// registry's `acp`-surface commands a person can run).
pub(crate) fn advertised(commands: &[CommandEntry]) -> Value {
    let available: Vec<Value> = commands
        .iter()
        .filter(|c| runnable(c))
        .map(|c| {
            let mut entry = json!({"name": c.name, "description": c.description});
            if let Some(hint) = &c.args {
                entry["input"] = json!({"hint": hint});
            }
            entry
        })
        .collect();
    json!({"sessionUpdate": "available_commands_update", "availableCommands": available})
}

#[cfg(test)]
mod tests {
    use pmt_rpc::contracts::{CommandEntry, CommandSource, Disposition};

    use super::command_of;

    fn entry(name: &str, aliases: &[&str]) -> CommandEntry {
        CommandEntry {
            name: name.into(),
            aliases: aliases.iter().map(|a| (*a).to_owned()).collect(),
            description: String::new(),
            args: None,
            source: CommandSource::Builtin,
            disposition: Disposition::Exec,
            reason: None,
            subcommands: Vec::new(),
        }
    }

    /// Scenario: Path at line start.
    #[test]
    fn a_path_or_an_unknown_word_is_prompt_text() {
        let commands = [entry("compress", &["compact"]), entry("var", &[])];
        assert_eq!(command_of("/var/log/x is wrong", &commands), None);
        assert_eq!(command_of("/frobnicate now", &commands), None);
        assert_eq!(
            command_of("/compact keep the plan", &commands),
            Some(("compress".to_owned(), "keep the plan"))
        );
        assert_eq!(
            command_of("/compress", &commands),
            Some(("compress".to_owned(), ""))
        );
        assert_eq!(command_of("compress", &commands), None);
    }
}
