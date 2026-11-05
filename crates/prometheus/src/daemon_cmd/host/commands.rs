//! `command.dispatch` in the daemon (spec: tui, Slash command flow; cli,
//! Daemon attachment with in-process fallback): a slash command a client
//! does not handle itself runs on the daemon's session with the same
//! implementation as in the line interface ([`crate::chat::commands`]),
//! following the command's busy policy while a turn runs, and answers
//! `done` (its output), `send` (a message the client submits as a prompt)
//! or `refused` (why it did not run).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use pmt_core::error::Error;
use pmt_gateway::Link;
use pmt_rpc::contracts::{COMMAND_NOTICE, DispatchParams, Dispatched, PromptImage, SessionEvent};
use serde_json::json;

use super::{DaemonHost, Loaded, Profile, session_id};
use crate::chat::commands::quick::{MAX_DEPTH, alias_line, exec};
use crate::chat::commands::{self, Ctx, Reply};
use crate::slash::{BusyPolicy, Command, Registry, Resolution, Source, Surface};

/// Commands that change what a client shows or which session it drives:
/// the client runs them (or uses the session methods for them).
pub(super) const CLIENT_COMMANDS: &[&str] = &[
    "exit",
    "clear",
    "new",
    "fork",
    "resume",
    "image",
    "queue",
    "theme",
    "verbose",
    "focus",
    "timestamps",
    "prompt",
    "redraw",
    "statusbar",
    "battery",
];

fn refused(reason: impl Into<String>) -> Dispatched {
    Dispatched::Refused {
        reason: reason.into(),
    }
}

fn line_of(params: &DispatchParams) -> String {
    let name = params.name.trim_start_matches('/');
    if params.args.is_empty() {
        format!("/{name}")
    } else {
        format!("/{name} {}", params.args)
    }
}

/// What one dispatch runs against.
struct Run<'a> {
    session: &'a Loaded,
    profile: &'a Profile,
    registry: &'a Registry,
    link: &'a Link,
    /// Where the command was typed: a client (`Cli`) or a messaging
    /// platform (`Gateway`).
    surface: Surface,
    /// The platform's message length limit (gateway commands).
    message_limit: Option<usize>,
    /// The surface plugin commands see in `session.info`: `gateway`, or
    /// for a client the surface that created the session (`cli`, `tui`,
    /// `desktop`...).
    caller: &'a str,
}

impl DaemonHost {
    pub(super) async fn dispatch_command(
        &self,
        profile: &Profile,
        params: DispatchParams,
        link: Link,
    ) -> Result<Dispatched, Error> {
        let id = session_id(&params.session_id)?;
        let session = self.load(profile, id, &link).await?;
        let registry = commands::registry(&profile.scope(), &session.live);
        let run = Run {
            session: &session,
            profile,
            registry: &registry,
            link: &link,
            surface: Surface::Cli,
            message_limit: None,
            caller: session.live.surface(),
        };
        Ok(run.line(line_of(&params), 0).await)
    }

    /// A registry command a messaging user sent (spec: messaging-gateway,
    /// Gateway slash commands, quick commands and hooks): resolved for the
    /// gateway surface; an `exec` quick command is killed with its process
    /// tree after `gateway.quick_commands.timeout_secs`.
    pub(super) async fn gateway_command(
        &self,
        profile: &Profile,
        session_id: &str,
        input: &str,
        link: Link,
    ) -> Result<Dispatched, Error> {
        let id = super::session_id(session_id)?;
        let session = self.load(profile, id, &link).await?;
        let registry = commands::registry(&profile.scope(), &session.live);
        let run = Run {
            session: &session,
            profile,
            registry: &registry,
            link: &link,
            surface: Surface::Gateway,
            message_limit: self.message_limit(profile, id),
            caller: "gateway",
        };
        Ok(run.line(input.to_owned(), 0).await)
    }
}

impl DaemonHost {
    /// The message limit of the platform a gateway session talks on.
    fn message_limit(&self, profile: &Profile, id: pmt_core::id::SessionId) -> Option<usize> {
        let session = profile.store.session(id).ok()?;
        let pmt_store::Origin::Gateway { platform, .. } = session.origin else {
            return None;
        };
        self.gateway
            .get()?
            .max_message_length(&profile.scope().name().to_string(), &platform)
    }
}

impl Run<'_> {
    async fn line(&self, line: String, depth: u8) -> Dispatched {
        match self.registry.resolve(&line, self.surface) {
            Resolution::Builtin { command, args } => self.builtin(command, &args).await,
            Resolution::External { command, args } => match command.source {
                Source::Quick => Box::pin(self.quick(&command.name, &args, depth)).await,
                Source::Plugin => {
                    match commands::plugins::run_line(&self.session.live, &line, self.caller).await
                    {
                        Ok(notice) => Dispatched::Done { notice },
                        Err(reason) => refused(reason),
                    }
                }
                Source::Skill => {
                    match commands::skill_line(&self.session.live, self.registry, &line).await {
                        Ok((notices, message)) => {
                            for notice in notices {
                                self.notice(&notice);
                            }
                            Dispatched::Send {
                                message,
                                images: Vec::new(),
                            }
                        }
                        Err(reason) => refused(reason),
                    }
                }
            },
            other => refused(other.message().unwrap_or_default()),
        }
    }

    async fn quick(&self, name: &str, args: &str, depth: u8) -> Dispatched {
        let scope = self.profile.scope();
        let Some(command) = scope.config().quick_commands.get(name).cloned() else {
            return refused(format!("Unknown command: /{name}"));
        };
        match command.kind {
            pmt_config::schema::QuickCommandKind::Alias if depth >= MAX_DEPTH => {
                refused(format!("quick command '{name}' nests too deeply"))
            }
            pmt_config::schema::QuickCommandKind::Alias => {
                self.line(alias_line(&command, args), depth + 1).await
            }
            pmt_config::schema::QuickCommandKind::Exec => {
                let mut command = command;
                if self.surface == Surface::Gateway {
                    command.timeout_secs = scope.config().gateway.quick_commands.timeout_secs;
                }
                let notice = exec(&self.ctx(&scope), name, &command, args).await;
                Dispatched::Done { notice }
            }
        }
    }

    async fn builtin(&self, command: &'static Command, args: &str) -> Dispatched {
        let name = command.name;
        if self.surface == Surface::Cli && CLIENT_COMMANDS.contains(&name) {
            return refused(format!("/{name} is handled by the client"));
        }
        let session = self.session;
        if session.busy.load(Ordering::Acquire) {
            match command.busy {
                BusyPolicy::Reject => {
                    return refused(format!("/{name} is unavailable while the agent is working"));
                }
                BusyPolicy::Queue => session.until_idle().await,
                BusyPolicy::InterruptThenDispatch => {
                    session.stop();
                    session.until_idle().await;
                }
                BusyPolicy::Immediate => {}
            }
        }
        match name {
            "stop" => Dispatched::Done {
                notice: "nothing is running".to_owned(),
            },
            "snapshot" if args.split_whitespace().next() == Some("restore") => refused(
                "restoring a snapshot replaces the store the gateway daemon serves; stop it with 'prometheus daemon stop' and restore from the line interface",
            ),
            "snapshot" => done(crate::slash_handlers::snapshot(&self.profile.scope(), args)),
            _ => self.shared(name, args).await,
        }
    }

    async fn shared(&self, name: &str, args: &str) -> Dispatched {
        let scope = self.profile.scope();
        match commands::run(&self.ctx(&scope), name, args).await {
            Some(Ok(Reply::Text(notice))) => Dispatched::Done { notice },
            Some(Ok(Reply::Send(input))) => Dispatched::Send {
                message: input.text,
                images: input
                    .images
                    .into_iter()
                    .map(|image| PromptImage::Blob(image.blob))
                    .collect(),
            },
            Some(Err(err)) => refused(reason(&err)),
            None => refused(format!("/{name} is not available over the gateway daemon")),
        }
    }

    fn ctx<'a>(&'a self, scope: &'a pmt_config::ProfileScope) -> Ctx<'a> {
        let link = self.link.clone();
        Ctx {
            factory: &self.session.factory,
            live: &self.session.live,
            scope,
            registry: self.registry,
            surface: self.surface,
            timestamps: scope.config().display.timestamps,
            running: self.session.busy.load(Ordering::Acquire),
            post: Arc::new(move |text| notice(&link, &text)),
            message_limit: self.message_limit,
        }
    }

    fn notice(&self, text: &str) {
        notice(self.link, text);
    }
}

/// Shows `text` on the session's clients as a `command.notice` event.
fn notice(link: &Link, text: &str) {
    link.emit::<SessionEvent>(&json!({
        "type": COMMAND_NOTICE,
        "ts": jiff::Timestamp::now().to_string(),
        "turn_id": null,
        "span_id": null,
        "parent_span_id": null,
        "payload": {"text": text},
    }));
}

fn done(result: Result<String, Error>) -> Dispatched {
    match result {
        Ok(notice) => Dispatched::Done { notice },
        Err(err) => refused(reason(&err)),
    }
}

/// An error as the person reads it (message and hint).
fn reason(err: &Error) -> String {
    crate::output::render_error(err, false)
        .trim_end()
        .to_owned()
}
