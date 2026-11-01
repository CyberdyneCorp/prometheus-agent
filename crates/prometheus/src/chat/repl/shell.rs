//! Shell escape (spec: cli, Shell escape in the REPL): a line starting with
//! `!` runs on the session's terminal backend under the sandbox and the
//! approval gate the `terminal` tool uses, and its output stays on the
//! screen. `!!` also hands the command and its output (redacted, capped at
//! [`MAX_CONTEXT`] bytes) to the next turn as user context.

use std::sync::Arc;

use pmt_tools::dispatch::GateVerdict;
use pmt_tools::environments::Environments;
use pmt_tools::terminal::TerminalArgs;

use super::Repl;
use super::input::Event;
use super::prompts;
use crate::chat::theme::Role;

/// Largest command-and-output block `!!` passes to the model.
pub const MAX_CONTEXT: usize = 16 * 1024;

/// What a `!` line typed while a turn runs gets.
pub const BUSY: &str = "shell escape is unavailable while the agent is working";

/// The call id approval prompts and logs show for an escape.
const CALL_ID: &str = "shell-escape";

impl Repl {
    /// Runs a `!` or `!!` line typed at an idle prompt.
    pub(super) async fn shell_escape(&mut self, line: &str) {
        let (share, command) = match line.strip_prefix("!!") {
            Some(rest) => (true, rest.trim()),
            None => (false, line.trim_start_matches('!').trim()),
        };
        if command.is_empty() {
            self.notice("usage: !<command> (output stays here) or !!<command> (also sent with your next message)");
            return;
        }
        let Some(terminal) = Environments::process()
            .session(&self.live().id.to_string())
            .and_then(|env| env.terminal.clone())
        else {
            self.notice("the shell escape needs the terminal, which is unavailable");
            return;
        };
        match self.gate(command).await {
            GateVerdict::Allow => {}
            GateVerdict::Blocked(why) | GateVerdict::Denied(why) => {
                self.notice(&why);
                return;
            }
        }
        let result = terminal.run(TerminalArgs::new(command.to_owned())).await;
        let output = self.factory.redactor().redact_all(result.output.trim_end());
        if !output.is_empty() {
            self.input.print(&output);
        }
        if let Some(error) = &result.error {
            self.notice(error);
        }
        if share {
            let block = context_block(command, &output);
            self.shell_context.push(block);
            self.notice("the command and its output go with your next message");
        }
    }

    /// The approval gate's verdict on `command`, answering its prompt from
    /// the next line.
    async fn gate(&mut self, command: &str) -> GateVerdict {
        let gate = Arc::clone(&self.live().gate);
        let check = gate.check_command("terminal", CALL_ID, command);
        let mut check = std::pin::pin!(check);
        loop {
            tokio::select! {
                verdict = &mut check => return verdict,
                Some(prompt) = super::turn::next(&mut self.approvals) => {
                    self.ring_bell();
                    self.say(Role::Notice, &prompts::approval_question(prompt.request()));
                    let ask = self.display.theme.paint(
                        Role::Prompt,
                        "approve? [o]nce [s]ession [a]lways [d]eny › ",
                        self.display.color,
                    );
                    self.input.request(&ask);
                    let decision = match self.input.next().await {
                        Event::Line(line) => prompts::decision(&line),
                        _ => None,
                    };
                    match decision {
                        Some(decision) => prompt.respond(decision),
                        None => self.notice("not approved"),
                    }
                }
            }
        }
    }
}

/// The `!!` context: the command and its output, capped at
/// [`MAX_CONTEXT`] bytes on a character boundary.
#[must_use]
pub fn context_block(command: &str, output: &str) -> String {
    let mut block = format!("[shell escape] $ {command}\n{output}");
    if block.len() > MAX_CONTEXT {
        let mut end = MAX_CONTEXT;
        while !block.is_char_boundary(end) {
            end -= 1;
        }
        block.truncate(end);
        block.push_str("\n[output truncated]");
    }
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_output_is_capped_at_16_kib() {
        let block = context_block("cat big", &"é".repeat(20_000));
        assert!(block.len() <= MAX_CONTEXT + "\n[output truncated]".len());
        assert!(block.starts_with("[shell escape] $ cat big\n"));
        assert!(block.ends_with("[output truncated]"));
        assert_eq!(context_block("ls", "a"), "[shell escape] $ ls\na");
    }
}
