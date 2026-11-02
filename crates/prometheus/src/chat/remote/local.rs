//! The slash commands an attached chat runs itself (spec: cli, Daemon
//! attachment with in-process fallback; tui, Slash command flow): leaving,
//! what this terminal shows (`/theme`, `/verbose`, `/focus`,
//! `/timestamps`), what the next prompt carries (`/image`, `/queue`,
//! `/prompt`) and which session it drives (`/new`, `/clear`, `/fork`,
//! `/resume`, `/stop` through the session methods). Every other command,
//! and anything the built-in table does not name (skill and quick
//! commands live in the daemon's session), goes to `command.dispatch`.

use std::fmt::Write as _;

use pmt_core::error::{Error, codes};
use pmt_rpc::client::error_of;
use pmt_rpc::contracts::{
    ForkParams, ResolveKind, ResolveParams, SessionFork, SessionInterrupt, SessionRef,
    SessionResolve,
};

use super::line::{Chat, Flow, Typed};
use super::open;
use super::render::Out as _;
use crate::chat::attach;
use crate::chat::repl::queue;
use crate::slash::{Registry, Resolution, Surface, split_line};

/// Commands that ask `once / always / cancel` first.
const DESTRUCTIVE: &[&str] = &["clear", "new", "undo", "rewind"];

/// `args` without `--yes`/`-y`, and whether one was there.
fn take_yes(args: &str) -> (String, bool) {
    let kept: Vec<&str> = args
        .split_whitespace()
        .filter(|w| !matches!(*w, "--yes" | "-y"))
        .collect();
    let yes = kept.len() != args.split_whitespace().count();
    (kept.join(" "), yes)
}

impl Chat<'_, '_> {
    /// Runs a slash line typed at the idle prompt.
    pub(super) async fn slash(&mut self, line: String) -> Result<Flow, Error> {
        let registry = Registry::new()?;
        let Resolution::Builtin { command, args } = registry.resolve(&line, Surface::Cli) else {
            let (token, args) = split_line(&line);
            self.dispatch(&token, &args).await?;
            return Ok(Flow::Continue);
        };
        let (args, yes) = take_yes(&args);
        if DESTRUCTIVE.contains(&command.name) && !yes && !self.confirmed(command.name).await? {
            return Ok(Flow::Continue);
        }
        if let Some(flow) = self.local(command.name, &args).await? {
            return Ok(flow);
        }
        self.dispatch(command.name, &args).await?;
        Ok(Flow::Continue)
    }

    /// Asks before a destructive command (`cli.confirm_destructive`);
    /// `always` stops asking for this profile.
    async fn confirmed(&mut self, name: &str) -> Result<bool, Error> {
        if !self.confirm_destructive {
            return Ok(true);
        }
        self.screen.ask(&format!(
            "/{name} changes this session. Run it [o]nce, [a]lways (stop asking), or [c]ancel?"
        ));
        let answer = match self.lines.next().await {
            Typed::Line(line) => line.trim().to_lowercase(),
            _ => String::new(),
        };
        Ok(match answer.as_str() {
            "o" | "once" | "y" | "yes" => true,
            "a" | "always" => {
                self.confirm_destructive = false;
                self.start
                    .scope
                    .profile()
                    .config_file()
                    .set("cli.confirm_destructive", "false")?;
                true
            }
            _ => {
                self.screen.notice("cancelled");
                false
            }
        })
    }

    /// Runs `name` here when it is a client command.
    async fn local(&mut self, name: &str, args: &str) -> Result<Option<Flow>, Error> {
        let text = match name {
            "exit" => return Ok(Some(Flow::Exit)),
            "clear" => {
                self.screen.raw("\x1b[2J\x1b[H");
                let created = open::claim(&self.attached.client, None, self.start).await;
                self.switch(created).await;
                return Ok(Some(Flow::Continue));
            }
            "new" => {
                let created = open::claim(&self.attached.client, None, self.start).await;
                self.switch(created).await;
                return Ok(Some(Flow::Continue));
            }
            "fork" => {
                let forked = self.fork(args).await;
                self.switch(forked).await;
                return Ok(Some(Flow::Continue));
            }
            "resume" if args.is_empty() => {
                self.dispatch("sessions", "all").await?;
                return Ok(Some(Flow::Continue));
            }
            "resume" => {
                let resumed = self.resume(args).await;
                self.switch(resumed).await;
                return Ok(Some(Flow::Continue));
            }
            "stop" => {
                self.interrupt().await;
                Ok("nothing is running".to_owned())
            }
            "prompt" => return self.compose(args).await.map(Some),
            other => match self.display_command(other, args) {
                Some(result) => result,
                None => return Ok(None),
            },
        };
        match text {
            Ok(text) => self.screen.notice(&text),
            Err(err) => self.screen.error(&err),
        }
        Ok(Some(Flow::Continue))
    }

    /// What this terminal shows and carries with the next prompt.
    fn display_command(&mut self, name: &str, args: &str) -> Option<Result<String, Error>> {
        let scope = self.start.scope;
        let display = &mut self.screen.display;
        Some(match name {
            "theme" => display.set_theme(scope, args),
            "verbose" => Ok(display.verbose()),
            "focus" => Ok(display.focus(args)),
            "timestamps" => display.set_timestamps(scope, args),
            "queue" => {
                queue::parse(args).and_then(|action| queue::apply(&mut self.lines.queue, action))
            }
            "image" => self.image(args),
            "redraw" => {
                self.lines.redraw();
                Ok(String::new())
            }
            "statusbar" => self.status_bar.statusbar(scope, args),
            "battery" => self.status_bar.battery_command(scope, args),
            _ => return None,
        })
    }

    /// `/image <path>` attaches, bare `/image` lists, `/image clear` drops.
    fn image(&mut self, args: &str) -> Result<String, Error> {
        match args.trim() {
            "" if self.images.is_empty() => Ok("no pending images".to_owned()),
            "" => {
                let mut text = String::from("pending images (sent with the next prompt):");
                for path in &self.images {
                    let _ = write!(text, "\n  {}", path.display());
                }
                Ok(text)
            }
            "clear" => {
                let n = self.images.len();
                self.images.clear();
                Ok(format!("dropped {n} pending image(s)"))
            }
            path => {
                let path = self.start.cwd.join(path);
                let image = attach::load(&path)?;
                self.images.push(path);
                Ok(format!(
                    "attached {} ({} pending; sent with the next prompt)",
                    image.name,
                    self.images.len()
                ))
            }
        }
    }

    /// `/prompt [text]`: the external editor's result is the next prompt.
    async fn compose(&mut self, initial: &str) -> Result<Flow, Error> {
        let editor = self
            .start
            .launch
            .editor()
            .unwrap_or(crate::config_cmd::edit::DEFAULT_EDITOR);
        self.lines.flush();
        let dir = self.start.scope.path("cache/compose");
        match crate::chat::repl::compose::edit_message(&dir, editor, initial) {
            Ok(Some(text)) => self.submit(text).await?,
            Ok(None) => self.screen.notice(crate::chat::repl::compose::EMPTY),
            Err(err) => self.screen.error(&err),
        }
        Ok(Flow::Continue)
    }

    /// `/fork [title]`: a copy of this session, opened as its owner.
    async fn fork(&self, title: &str) -> Result<String, Error> {
        let params = ForkParams {
            session_id: self.attached.session.clone(),
            at_message_id: None,
            title: (!title.is_empty()).then(|| title.to_owned()),
        };
        let client = &self.attached.client;
        let forked = client
            .call_typed::<SessionFork>(&params)
            .await
            .map_err(|e| error_of(&e))?;
        open::resume(client, &forked.session_id, &self.start.setup).await?;
        self.screen.notice(&format!(
            "forked {} into {}",
            self.attached.session, forked.session_id
        ));
        Ok(forked.session_id)
    }

    /// `/resume <ref>`: the session the reference names, as its owner.
    async fn resume(&self, reference: &str) -> Result<String, Error> {
        let params = ResolveParams {
            profile: None,
            kind: ResolveKind::Resume,
            reference: reference.to_owned(),
            cwd: Some(self.start.cwd.to_string_lossy().into_owned()),
            terminal: self.start.terminal.clone(),
        };
        let client = &self.attached.client;
        let found = client
            .call_typed::<SessionResolve>(&params)
            .await
            .map_err(|e| error_of(&e))?;
        if found.session_id == self.attached.session {
            return Err(Error::new(&codes::CLI_USAGE, "that is this session"));
        }
        open::resume(client, &found.session_id, &self.start.setup).await?;
        Ok(found.session_id)
    }

    async fn interrupt(&self) {
        let params = SessionRef {
            session_id: self.attached.session.clone(),
        };
        let _ = self
            .attached
            .client
            .call_typed::<SessionInterrupt>(&params)
            .await;
    }
}
