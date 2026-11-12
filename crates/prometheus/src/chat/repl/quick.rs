//! Quick commands in the line interface: an `alias` re-dispatches its
//! target through the REPL, an `exec` runs on the session's terminal
//! ([`crate::chat::commands::quick`]).

use super::{Flow, Repl};
use crate::chat::commands::quick::{MAX_DEPTH, alias_line, exec};
use crate::slash::Source;
use pmt_config::schema::QuickCommandKind;

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
            self.notice(&pmt_i18n::t!(
                self.strings(),
                "cli.unknown_command",
                name = name
            ));
            return Flow::Continue;
        };
        match command.kind {
            QuickCommandKind::Alias => {
                if depth >= MAX_DEPTH {
                    let text = pmt_i18n::t!(self.strings(), "cli.quick_nested", name = name);
                    self.notice(&text);
                    return Flow::Continue;
                }
                Box::pin(self.slash_at_depth(alias_line(&command, args), depth + 1)).await
            }
            QuickCommandKind::Exec => {
                let Some(live) = self.live.take() else {
                    return Flow::Continue;
                };
                let text = exec(&self.ctx(&live), name, &command, args).await;
                self.live = Some(live);
                if !text.is_empty() {
                    self.input.print(&text);
                }
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
}
