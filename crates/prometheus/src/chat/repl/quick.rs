//! Quick commands (spec: cli, Quick commands): `[quick_commands.<name>]`
//! entries of type `alias` re-dispatch their target with the user's
//! arguments appended; `exec` entries run their command on the session's
//! sandboxed terminal (an environment without secrets, output redacted)
//! and are killed after `timeout_secs`.

use std::collections::BTreeMap;

use pmt_config::schema::{QuickCommand, QuickCommandKind};
use pmt_tools::environments::Environments;
use pmt_tools::terminal::{Status, TerminalArgs};

use super::{Flow, Repl};
use crate::slash::{Declaration, Registry, Source};

/// Alias chains deeper than this stop (config validation refuses cycles;
/// this bounds chains through other commands).
const MAX_DEPTH: u8 = 8;

/// Registers the profile's quick commands.
pub(super) fn register(registry: &mut Registry, commands: &BTreeMap<String, QuickCommand>) {
    for (name, command) in commands {
        let description = match command.kind {
            QuickCommandKind::Alias => {
                format!("alias of {}", command.target.as_deref().unwrap_or(""))
            }
            QuickCommandKind::Exec => {
                format!("runs `{}`", command.command.as_deref().unwrap_or(""))
            }
        };
        let declaration = Declaration {
            name: name.clone(),
            description,
            ..Declaration::default()
        };
        if let Err(err) = registry.register(&declaration, Source::Quick) {
            tracing::warn!(error = %err, "quick command not loaded");
        }
    }
}

impl Repl {
    /// Runs quick command `name` with `args`.
    pub(super) async fn quick(&mut self, name: &str, args: &str, depth: u8) -> Flow {
        let Some(command) = self
            .factory
            .scope
            .config()
            .quick_commands
            .get(name)
            .cloned()
        else {
            self.notice(&format!("Unknown command: /{name}"));
            return Flow::Continue;
        };
        match command.kind {
            QuickCommandKind::Alias => {
                let target = command.target.as_deref().unwrap_or_default().trim();
                let target = target.strip_prefix('/').unwrap_or(target);
                let line = if args.is_empty() {
                    format!("/{target}")
                } else {
                    format!("/{target} {args}")
                };
                if depth >= MAX_DEPTH {
                    self.notice(&format!("quick command '{name}' nests too deeply"));
                    return Flow::Continue;
                }
                Box::pin(self.slash_at_depth(line, depth + 1)).await
            }
            QuickCommandKind::Exec => {
                self.exec(name, &command, args).await;
                Flow::Continue
            }
        }
    }

    /// Like [`Repl::slash`], counting alias depth.
    async fn slash_at_depth(&mut self, line: String, depth: u8) -> Flow {
        if let crate::slash::Resolution::External { command, args } =
            self.registry.resolve(&line, crate::slash::Surface::Cli)
            && command.source == Source::Quick
        {
            return Box::pin(self.quick(&command.name, &args, depth)).await;
        }
        Box::pin(self.slash(line)).await
    }

    async fn exec(&mut self, name: &str, command: &QuickCommand, args: &str) {
        let Some(terminal) = Environments::process()
            .session(&self.live().id.to_string())
            .and_then(|env| env.terminal.clone())
        else {
            self.notice(&format!(
                "quick command '{name}' needs the terminal, which is unavailable"
            ));
            return;
        };
        let line = command.command.as_deref().unwrap_or_default();
        let line = if args.is_empty() {
            line.to_owned()
        } else {
            format!("{line} {args}")
        };
        let mut run = TerminalArgs::new(line);
        #[allow(clippy::cast_precision_loss)] // whole seconds, far below 2^52
        let seconds = command.timeout_secs as f64;
        run.timeout = Some(seconds);
        let result = terminal.run(run).await;
        let output = self.factory.redactor().redact_all(result.output.trim_end());
        if !output.is_empty() {
            self.input.print(&output);
        }
        match result.status {
            Status::Timeout => self.notice(&format!(
                "quick command '{name}' timed out after {}s",
                command.timeout_secs
            )),
            Status::Error => self.notice(&format!(
                "quick command '{name}' failed{}",
                result.error.map(|e| format!(": {e}")).unwrap_or_default()
            )),
            _ => {}
        }
    }
}
