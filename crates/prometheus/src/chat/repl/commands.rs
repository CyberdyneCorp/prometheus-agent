//! Slash commands of the line interface (spec: cli, Built-in slash command
//! set; Destructive command confirmation; Deferred effects with `--now`;
//! Skill invocation as user message). Resolution and `/help` come from the
//! registry; this module runs what resolved.

use pmt_agent::turn::TurnInput;
use pmt_core::error::Error;

use super::prompts::Pending;
use super::{Flow, Repl};
use crate::chat::commands::{self, Reply};
use crate::chat::session::Live;
use crate::slash::{
    Command, Confirmation, Resolution, Source, Surface, confirmation, exit_deletes, is_destructive,
};

/// `args` without `--yes`/`-y`, and whether one was there.
fn take_yes(args: &str) -> (String, bool) {
    let mut yes = false;
    let kept: Vec<&str> = args
        .split_whitespace()
        .filter(|w| {
            let flag = matches!(*w, "--yes" | "-y");
            yes |= flag;
            !flag
        })
        .collect();
    (kept.join(" "), yes)
}

impl Repl {
    /// Runs a slash line typed at an idle prompt.
    pub(super) async fn slash(&mut self, line: String) -> Flow {
        match self.registry.resolve(&line, Surface::Cli) {
            Resolution::Builtin { command, args } => {
                Box::pin(self.builtin(command, &args, &line)).await
            }
            Resolution::External { command, args } => match command.source {
                Source::Quick => self.quick(&command.name, &args, 0).await,
                Source::Plugin => {
                    match commands::plugins::run_line(self.live(), &line, "cli").await {
                        Ok(text) | Err(text) => self.notice(&text),
                    }
                    Flow::Continue
                }
                Source::Skill => {
                    self.skill_line(&line).await;
                    Flow::Continue
                }
            },
            other => {
                if let Some(message) = other.message() {
                    self.notice(&message);
                }
                Flow::Continue
            }
        }
    }

    async fn builtin(&mut self, command: &'static Command, args: &str, line: &str) -> Flow {
        let (args, yes) = take_yes(args);
        if is_destructive(command.name, &args) && self.confirm_destructive && !yes {
            let text = pmt_i18n::t!(
                self.strings(),
                "cli.changes_session",
                command = command.name
            );
            self.notice(&text);
            self.pending = Some(Pending::Confirm {
                line: line.to_owned(),
            });
            return Flow::Continue;
        }
        Box::pin(self.run_builtin(command.name, &args)).await
    }

    /// The answer to a destructive command's confirmation.
    pub(super) async fn confirmed(&mut self, line: &str, answer: &str) -> Flow {
        match confirmation(answer) {
            Confirmation::Cancel => {
                self.notice("cancelled");
                Flow::Continue
            }
            choice => {
                if choice == Confirmation::Always {
                    self.confirm_destructive = false;
                    if let Err(err) = self
                        .factory
                        .scope
                        .profile()
                        .config_file()
                        .set("cli.confirm_destructive", "false")
                    {
                        self.error(&err);
                    }
                }
                let (name, args) = crate::slash::split_line(line);
                match self.registry.builtin(&name) {
                    Some(command) => {
                        Box::pin(self.run_builtin(command.name, &take_yes(&args).0)).await
                    }
                    None => Flow::Continue,
                }
            }
        }
    }

    /// `/stop all` between turns: every subagent of the session stops.
    fn stop_all(&self) -> String {
        if let Some(delegation) = self.live.as_ref().and_then(|l| l.delegation.as_ref()) {
            delegation.stop_all();
        }
        pmt_i18n::t!(self.strings(), "cli.stopped_all")
    }

    async fn run_builtin(&mut self, name: &str, args: &str) -> Flow {
        let result = match name {
            "exit" => {
                self.delete_on_exit = exit_deletes(args);
                return Flow::Exit;
            }
            "handoff" => return self.handoff(args).await,
            "new" => self.new_session(None, None).await,
            "clear" => {
                self.input.print("\x1b[2J\x1b[H");
                self.new_session(None, None).await
            }
            "stop" if args.trim() == "all" => Ok(self.stop_all()),
            "stop" => Ok(pmt_i18n::t!(self.strings(), "cli.nothing_running")),
            "fork" => self.fork(args).await,
            "resume" => self.resume(args).await,
            "prompt" => return self.compose(args).await,
            "snapshot" => self.snapshot(args).await,
            "voice" => {
                self.voice_command(args).await;
                return Flow::Continue;
            }
            "retry" | "learn" | "tools" | "model" | "moa" | "plan" => {
                return Box::pin(self.session_reply(name, args)).await;
            }
            _ => {
                let Some(live) = self.live.take() else {
                    return Flow::Continue;
                };
                self.immediate(name, args, &live).await;
                self.live = Some(live);
                if name == "plugins" {
                    // A reload replaces the plugins' slash commands at once.
                    self.load_registry();
                }
                return Flow::Continue;
            }
        };
        self.show(result);
        Flow::Continue
    }

    /// `/handoff`: the session moves to a chat; this client leaves.
    async fn handoff(&mut self, args: &str) -> Flow {
        let Some(session) = self.live.as_ref().map(|l| l.id.to_string()) else {
            return Flow::Continue;
        };
        let scope = &self.factory.scope;
        let home = scope.home().to_path_buf();
        let profile = scope.name().to_string();
        let strings = self.strings();
        match crate::chat::handoff::handoff(&home, &profile, &session, args, strings).await {
            Ok(line) => {
                self.notice(&line);
                Flow::Exit
            }
            Err(err) => {
                self.error(&err);
                Flow::Continue
            }
        }
    }

    /// A shared command that may send a message or change the session's
    /// tools or model (it runs with the session in place).
    async fn session_reply(&mut self, name: &str, args: &str) -> Flow {
        let Some(live) = self.live.take() else {
            return Flow::Continue;
        };
        let reply = commands::run(&self.ctx(&live), name, args).await;
        self.live = Some(live);
        match reply {
            Some(Ok(Reply::Send(input))) if name == "learn" => self.user_turn(input.text).await,
            Some(Ok(Reply::Send(input))) => self.run_input(input).await,
            Some(Ok(Reply::Text(text))) => self.show(Ok(text)),
            Some(Err(err)) => self.error(&err),
            None => {}
        }
        Flow::Continue
    }

    /// Shows a command's text or error.
    pub(super) fn show(&self, result: Result<String, Error>) {
        match result {
            Ok(text) if text.is_empty() => {}
            Ok(text) => self.notice(&text),
            Err(err) => self.error(&err),
        }
    }

    /// Commands that only read the session or change display state; they
    /// also run while a turn works (`immediate` busy policy) or need only
    /// shared access to the session.
    pub(super) async fn immediate(&mut self, name: &str, args: &str, live: &Live) {
        let result = match name {
            "queue" => super::queue::parse(args)
                .and_then(|action| super::queue::apply(&self.strings(), &mut self.queue, action)),
            "image" => self.image(args),
            "theme" => self.theme(args),
            "verbose" => Ok(self.verbose()),
            "focus" => Ok(self.focus(args)),
            "timestamps" => self.timestamps(args),
            "redraw" => {
                self.input.redraw();
                Ok(String::new())
            }
            "statusbar" => self.status_bar.statusbar(&self.factory.scope, args),
            "battery" => self.status_bar.battery_command(&self.factory.scope, args),
            other => match commands::run(&self.ctx(live), other, args).await {
                Some(Ok(Reply::Text(text))) => Ok(text),
                // Only `/learn`, `/retry` and `/moa` send, and they run with the
                // session in place (`session_reply`).
                Some(Ok(Reply::Send(_))) => Ok(String::new()),
                Some(Err(err)) => Err(err),
                None => Err(Error::new(
                    &pmt_core::error::codes::CLI_USAGE,
                    format!("/{other} is not available in the line interface yet"),
                )),
            },
        };
        self.show(result);
    }

    fn theme(&mut self, args: &str) -> Result<String, Error> {
        self.display.set_theme(&self.factory.scope, args)
    }

    /// A message that starts with skill commands: one user message that
    /// loads the skills, then the instruction (never a system prompt
    /// change).
    pub(super) async fn skill_line(&mut self, line: &str) {
        let expanded = commands::skill_line(self.live(), &self.registry, line).await;
        match expanded {
            Ok((notices, message)) => {
                for notice in &notices {
                    self.notice(notice);
                }
                self.run_input(TurnInput::text(message)).await;
            }
            Err(message) => self.notice(&message),
        }
    }
}
