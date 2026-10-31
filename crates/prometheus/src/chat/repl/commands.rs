//! Slash commands of the line interface (spec: cli, Built-in slash command
//! set; Destructive command confirmation; Deferred effects with `--now`;
//! Skill invocation as user message). Resolution and `/help` come from the
//! registry; this module runs what resolved.

use std::fmt::Write as _;

use pmt_agent::prompt::frozen::Rebuild;
use pmt_agent::turn::TurnInput;
use pmt_core::error::{Error, codes};

use super::prompts::{Confirmation, Pending};
use super::{Flow, Repl};
use crate::chat::session::Live;
use crate::chat::theme::{self, Theme};
use crate::slash::{Command, Resolution, Source, Surface};

/// Commands that ask `once / always / cancel` first.
const DESTRUCTIVE: &[&str] = &["clear", "new", "undo", "rewind"];

/// The `--now` notice of a deferred change.
pub(super) fn deferred(what: &str) -> String {
    format!(
        "{what} in your next session. Use /new to start one now, or add --now to apply immediately (invalidates prompt cache)."
    )
}

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

/// `args` without `--now`, and whether it was there.
fn take_now(args: &str) -> (String, bool) {
    let now = args.split_whitespace().any(|w| w == "--now");
    let kept: Vec<&str> = args.split_whitespace().filter(|w| *w != "--now").collect();
    (kept.join(" "), now)
}

impl Repl {
    /// Runs a slash line typed at an idle prompt.
    pub(super) async fn slash(&mut self, line: String) -> Flow {
        match self.registry.resolve(&line, Surface::Cli) {
            Resolution::Builtin { command, args } => self.builtin(command, &args, &line).await,
            Resolution::External { command, args } => match command.source {
                Source::Quick => self.quick(&command.name, &args, 0).await,
                Source::Skill | Source::Plugin => {
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
        if DESTRUCTIVE.contains(&command.name) && self.confirm_destructive && !yes {
            self.notice(&format!("/{} changes this session.", command.name));
            self.pending = Some(Pending::Confirm {
                line: line.to_owned(),
            });
            return Flow::Continue;
        }
        self.run_builtin(command.name, &args).await
    }

    /// The answer to a destructive command's confirmation.
    pub(super) async fn confirmed(&mut self, line: &str, answer: &str) -> Flow {
        match super::prompts::confirmation(answer) {
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
                    Some(command) => self.run_builtin(command.name, &take_yes(&args).0).await,
                    None => Flow::Continue,
                }
            }
        }
    }

    async fn run_builtin(&mut self, name: &str, args: &str) -> Flow {
        let result = match name {
            "exit" => return Flow::Exit,
            "new" => self.new_session(None, None).await,
            "clear" => {
                self.input.print("\x1b[2J\x1b[H");
                self.new_session(None, None).await
            }
            "stop" => Ok("nothing is running".to_owned()),
            "undo" => self.undo(),
            "retry" => return self.retry().await,
            "rewind" => self.rewind(args),
            "fork" => self.fork(args).await,
            "resume" => self.resume(args).await,
            "compress" => self.compress(args).await,
            "model" => self.model(args).await,
            "tools" => self.tools(args).await,
            "learn" => return self.learn(args).await,
            "prompt" => return self.compose(args).await,
            "snapshot" => self.snapshot(args).await,
            _ => {
                let Some(live) = self.live.take() else {
                    return Flow::Continue;
                };
                self.immediate(name, args, &live).await;
                self.live = Some(live);
                return Flow::Continue;
            }
        };
        self.show(result);
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
            "help" => Ok(self.registry.help(Surface::Cli).trim_end().to_owned()),
            "usage" => super::sessions::usage(&self.factory, live),
            "budget" => self.budget(live, args),
            "title" => self.title(live, args),
            "save" => crate::slash_handlers::save(
                &self.factory.scope,
                &self.factory.store,
                live.id,
                args,
                &self.factory.options.cwd,
            ),
            "sessions" => self.sessions(live, args),
            "history" => self.history(live, args),
            "status" => self.status(live),
            "version" => Ok(format!("prometheus {}", crate::build_info::identity())),
            "queue" => super::queue::parse(args)
                .and_then(|action| super::queue::apply(&mut self.queue, action)),
            "toolsets" => crate::tools_cmd::slash_toolsets(&self.factory.registry, &live.selection),
            "bundles" => Ok(pmt_skills::bundles::listing(&pmt_skills::bundles::load(
                self.factory.scope.dir(),
            ))),
            "approvals" => Ok(approvals(&self.factory.scope, live)),
            "reload-secrets" => crate::slash_handlers::reload_secrets(&self.factory.scope),
            "refine" => self.refine(live, args).await,
            "image" => self.image(args),
            "theme" => self.theme(args),
            "verbose" => Ok(self.verbose()),
            "focus" => Ok(self.focus(args)),
            "timestamps" => self.timestamps(args),
            "profile" => Ok(format!(
                "profile {} ({})",
                self.factory.scope.name(),
                self.factory.scope.dir().display()
            )),
            other => self.session_command(other, args, live).await,
        };
        self.show(result);
    }

    /// Commands that change the session but take it by shared reference.
    async fn session_command(
        &mut self,
        name: &str,
        args: &str,
        live: &Live,
    ) -> Result<String, Error> {
        let scope = &self.factory.scope;
        let core = self.registry.builtin_names();
        match name {
            "skills" => {
                let reply = pmt_skills::activation::run(&live.skills, args, &core)?;
                if reply.rebuild_now {
                    live.agent.rebuild_prompt(Rebuild::Now("skills".to_owned()));
                }
                Ok(reply.text)
            }
            "reload-skills" => Ok(pmt_skills::reload::reload(&live.skills).render()),
            "memory" => {
                let reply =
                    pmt_memory::slash::run(&self.factory.store, &scope.config().memory, args)?;
                if reply.rebuild_now {
                    live.agent.rebuild_prompt(Rebuild::Now("memory".to_owned()));
                }
                Ok(reply.text)
            }
            "personality" => {
                let (text, now) = crate::slash_handlers::personality(scope, args)?;
                if let Some(name) = now {
                    live.agent.apply_personality_now(&name);
                }
                Ok(text)
            }
            "yolo" => Ok(yolo(live, args)),
            "config" => config(scope, args),
            "reasoning" => self.reasoning(live, args).await,
            other => Err(Error::new(
                &codes::CLI_USAGE,
                format!("/{other} is not available in the line interface yet"),
            )),
        }
    }

    fn title(&self, live: &Live, args: &str) -> Result<String, Error> {
        let store = &self.factory.store;
        if args.is_empty() {
            let session = store.session(live.id)?;
            return Ok(session.title.map_or_else(
                || "this session has no title".to_owned(),
                |t| format!("title: {t}"),
            ));
        }
        store.set_title(live.id, Some(args), pmt_store::TitleSource::User)?;
        Ok(format!("title set to {args}"))
    }

    fn budget(&self, live: &Live, args: &str) -> Result<String, Error> {
        let scope = &self.factory.scope;
        let (text, _) = crate::slash_handlers::budget(
            scope,
            &self.factory.store,
            live.id,
            &scope.config().budgets,
            args,
        )?;
        Ok(text)
    }

    fn theme(&mut self, args: &str) -> Result<String, Error> {
        let dir = self.factory.scope.dir();
        let available = theme::available(dir);
        if args.is_empty() || !available.iter().any(|t| t == args) {
            let mut text = String::new();
            if !args.is_empty() {
                let _ = writeln!(text, "no theme '{args}'");
            }
            let _ = write!(text, "themes:");
            for name in &available {
                let mark = if *name == self.display.theme.name {
                    " (current)"
                } else {
                    ""
                };
                let _ = write!(text, "\n  {name}{mark}");
            }
            return Ok(text);
        }
        match Theme::load(dir, args) {
            Ok(theme) => {
                self.display.theme = theme;
                self.factory
                    .scope
                    .profile()
                    .config_file()
                    .set("display.theme", args)?;
                Ok(format!("theme {args}"))
            }
            Err(why) => Ok(why),
        }
    }

    /// `/tools [list] | enable|disable <toolset> [--now]`.
    async fn tools(&mut self, args: &str) -> Result<String, Error> {
        let (args, now) = take_now(args);
        let words: Vec<&str> = args.split_whitespace().collect();
        let (enable, toolset) = match words.as_slice() {
            [] | ["list"] => {
                let live = self.live();
                return crate::tools_cmd::slash_toolsets(&self.factory.registry, &live.selection);
            }
            ["enable", name] => (true, *name),
            ["disable", name] => (false, *name),
            _ => {
                return Err(Error::new(
                    &codes::CLI_USAGE,
                    "usage: /tools [list] | enable|disable <toolset> [--now]",
                ));
            }
        };
        if !self.factory.registry.toolsets().any(|t| t.name == toolset) {
            return Err(crate::tools_cmd::unknown_toolset(
                &self.factory.registry,
                toolset,
            ));
        }
        crate::tools_cmd::configure::persist_toolset(&self.factory.scope, toolset, enable)?;
        let verb = if enable { "enabled" } else { "disabled" };
        if !now {
            return Ok(deferred(&format!("Toolset '{toolset}' is {verb}")));
        }
        self.apply_tools_now(toolset, enable).await?;
        Ok(format!(
            "Toolset '{toolset}' {verb} now; the next request uses the new tool set."
        ))
    }

    /// `/learn [focus]`: the learn instruction as this turn's message.
    async fn learn(&mut self, args: &str) -> Flow {
        let chars = self.factory.scope.config().skills.index_description_chars;
        match pmt_skills::learn::message(args, chars) {
            Ok(message) => self.user_turn(message).await,
            Err(err) => self.error(&err),
        }
        Flow::Continue
    }

    /// A message that starts with skill commands: one user message that
    /// loads the skills, then the instruction (never a system prompt
    /// change).
    pub(super) async fn skill_line(&mut self, line: &str) {
        let core = self.registry.builtin_names();
        let skills = std::sync::Arc::clone(&self.live().skills);
        let expanded = pmt_skills::dispatch::expand(&skills, line, &core).await;
        match expanded {
            Some(Ok(expanded)) => {
                for notice in &expanded.notices {
                    self.notice(notice);
                }
                self.run_input(TurnInput::text(expanded.message)).await;
            }
            Some(Err(message)) => self.notice(&message),
            None => {
                let (token, _) = crate::slash::split_line(line);
                self.notice(&format!("Unknown command: /{token}"));
            }
        }
    }
}

fn approvals(scope: &pmt_config::ProfileScope, live: &Live) -> String {
    let config = &scope.config().approvals;
    let mode = match config.mode {
        pmt_config::schema::ApprovalMode::Manual => "manual",
        pmt_config::schema::ApprovalMode::Smart => "smart",
        pmt_config::schema::ApprovalMode::Off => "off",
    };
    let yolo = if live.gate.yolo() {
        " (YOLO: every call allowed)"
    } else {
        ""
    };
    format!("approvals: {mode}{yolo}; timeout {} s", config.timeout_secs)
}

fn yolo(live: &Live, args: &str) -> String {
    let on = match args.trim() {
        "on" => true,
        "off" => false,
        _ => !live.gate.yolo(),
    };
    live.gate.set_yolo(on);
    if on {
        "YOLO on: approval-gated calls run without asking (deny rules and hardline blocks still apply)".to_owned()
    } else {
        "YOLO off".to_owned()
    }
}

/// `/config` (the file's path), `/config get <key>`, `/config set <key>
/// <value>` (validated like `prometheus config set`).
fn config(scope: &pmt_config::ProfileScope, args: &str) -> Result<String, Error> {
    let file = scope.profile().config_file();
    match args.split_once(' ').map(|(a, b)| (a, b.trim())) {
        None if args.is_empty() => Ok(format!("config: {}", file.path().display())),
        None if args.starts_with("get") => Err(config_usage()),
        Some(("get", key)) => match scope.layers().value(key) {
            Some(value) => Ok(format!("{key} = {value}")),
            None => Err(Error::new(
                &codes::CLI_USAGE,
                format!("unknown key '{key}'"),
            )),
        },
        Some(("set", rest)) => {
            let (key, value) = rest.split_once(' ').ok_or_else(config_usage)?;
            file.set(key, value.trim())?;
            Ok(format!("{key} = {} (saved)", value.trim()))
        }
        _ => Err(config_usage()),
    }
}

fn config_usage() -> Error {
    Error::new(
        &codes::CLI_USAGE,
        "usage: /config [get <key> | set <key> <value>]",
    )
}
