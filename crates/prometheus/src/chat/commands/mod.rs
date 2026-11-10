//! Session slash commands every surface runs the same way (spec: cli,
//! Built-in slash command set; Daemon attachment with in-process fallback;
//! tui, Slash command flow). The line interface runs them on the session
//! it holds; the daemon runs them for `command.dispatch` on the session a
//! client drives. What only changes a client's display or which session
//! it drives stays with that client.

mod account;
mod agents;
mod background;
mod blueprint;
mod browser;
mod context;
pub mod cron;
mod delegation;
mod fast;
pub mod files;
pub mod info;
mod local;
mod mcp;
mod moa;
pub(crate) mod plugins;
mod provider;
pub(crate) mod quick;
mod refine;
mod review;
pub(crate) mod session;
mod side;
mod suggestions;
pub mod worktree;

use std::sync::Arc;

use pmt_agent::prompt::frozen::Rebuild;
use pmt_agent::turn::TurnInput;
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};

pub(crate) use self::refine::skill_review;
use super::session::{Factory, Live};
use crate::slash::{self, Registry, Surface};

/// Where background work posts what it finds later (`/refine`).
pub type Post = Arc<dyn Fn(String) + Send + Sync>;

/// What a command runs against.
pub struct Ctx<'a> {
    /// The session machinery.
    pub factory: &'a Factory,
    /// The session.
    pub live: &'a Live,
    /// The profile's configuration as it is now (read and written by
    /// `/config`, `/budget`, `/personality`...).
    pub scope: &'a ProfileScope,
    /// The session's commands.
    pub registry: &'a Registry,
    /// The surface that asked.
    pub surface: Surface,
    /// `display.timestamps` of that surface (`/history`).
    pub timestamps: bool,
    /// A turn runs (`/status`).
    pub running: bool,
    /// Where `/refine` posts its summary.
    pub post: Post,
    /// The messaging platform's message length (gateway surfaces).
    pub message_limit: Option<usize>,
}

/// What a command produced.
#[derive(Debug)]
pub enum Reply {
    /// Text to show.
    Text(String),
    /// A message to send as the next prompt.
    Send(TurnInput),
}

/// Runs session command `name` with `args`; `None` when `name` is not a
/// session command (it belongs to the client: leaving, display state,
/// switching sessions).
pub async fn run(ctx: &Ctx<'_>, name: &str, args: &str) -> Option<Result<Reply, Error>> {
    let text = match name {
        "learn" => return Some(learn(ctx, args)),
        "init" => return Some(Ok(Reply::Send(pmt_agent::init::input(args)))),
        "retry" => return Some(retry(ctx)),
        "moa" => return Some(moa::moa(ctx, args).await),
        "help" => Ok(ctx.registry.help(ctx.surface).trim_end().to_owned()),
        "btw" => side::btw(ctx, args),
        "autoprune" => crate::chat::autoprune::command(ctx.scope, &ctx.factory.store, args),
        "usage" => account::usage(ctx, args).await,
        "login" => account::login(ctx, args),
        "subscription" => account::subscription(ctx).await,
        "topup" => account::topup(ctx).await,
        "context" => context::context(ctx, args).await,
        "budget" => budget(ctx, args),
        "title" => title(ctx, args),
        "save" => crate::slash_handlers::save(
            ctx.scope,
            &ctx.factory.store,
            ctx.live.id,
            args,
            &ctx.factory.options.cwd,
        ),
        "sessions" => info::sessions(ctx, args),
        "history" => info::history(ctx, args),
        "status" => info::status(ctx),
        "platforms" => info::platforms(ctx).await,
        "version" => Ok(format!("prometheus {}", crate::build_info::identity())),
        "toolsets" => crate::tools_cmd::slash_toolsets(&ctx.live.registry(), &ctx.live.selection()),
        "bundles" => Ok(pmt_skills::bundles::listing(&pmt_skills::bundles::load(
            ctx.scope.dir(),
        ))),
        "approvals" => Ok(approvals(ctx.live)),
        "reload-secrets" => crate::slash_handlers::reload_secrets(ctx.scope),
        "reload-skills" => Ok(pmt_skills::reload::reload(&ctx.live.skills).render()),
        "refine" => refine::refine(ctx, args).await,
        "background" => background::background(ctx, args).await,
        "agents" => agents::agents(ctx, args).await,
        "review" => review::review(ctx, args).await,
        "delegation" => delegation::delegation(ctx, args).await,
        "profile" => Ok(format!(
            "profile {} ({})",
            ctx.scope.name(),
            ctx.scope.dir().display()
        )),
        "undo" => files::undo(ctx, args),
        "rollback" => files::rollback(ctx, args),
        "diff" => Ok(files::diff(ctx, args)),
        "sync" => Ok(ctx.live.sync_back().await.unwrap_or_else(|| {
            "nothing to sync: the terminal backend has no remote workspace or terminal.sync_back is off"
                .to_owned()
        })),
        "rewind" => session::rewind(ctx, args),
        "compress" => session::compress(ctx, args).await,
        "pin" => session::pin(ctx, args),
        "unpin" => session::unpin(ctx, args),
        "model" => session::model(ctx, args).await,
        "provider" => provider::provider(ctx, args).await,
        "reasoning" => session::reasoning(ctx, args).await,
        "fast" => fast::fast(ctx, args).await,
        "local" => local::local(ctx, args).await,
        "tools" => session::tools(ctx, args).await,
        "plugins" => plugins::plugins(ctx, args).await,
        "mcp" => mcp::mcp(ctx, args).await,
        "browser" => browser::browser(ctx, args).await,
        "worktree" => worktree::worktree(ctx, args).await,
        "cron" => cron::cron(ctx, args),
        "suggestions" => suggestions::suggestions(ctx, args),
        "blueprint" => blueprint::blueprint(ctx, args),
        "goal" => crate::automation::commands::goal(ctx, args).await,
        "subgoal" => crate::automation::commands::subgoal(ctx, args),
        "loop" => crate::automation::commands::looping(ctx, args),
        "heartbeat" => crate::automation::commands::heartbeat(ctx, args),
        other => return state_command(ctx, other, args).map(|r| r.map(Reply::Text)),
    };
    Some(text.map(Reply::Text))
}

/// Commands that change the session's skills, memory, personality,
/// approvals or the profile's configuration.
fn state_command(ctx: &Ctx<'_>, name: &str, args: &str) -> Option<Result<String, Error>> {
    let live = ctx.live;
    let core = ctx.registry.builtin_names();
    Some(match name {
        "skills" => pmt_skills::activation::run(&live.skills, args, &core).map(|reply| {
            if reply.rebuild_now {
                live.agent.rebuild_prompt(Rebuild::Now("skills".to_owned()));
            }
            reply.text
        }),
        "memory" => pmt_memory::slash::run(
            &ctx.factory.store,
            &ctx.scope.config().memory,
            Some(&live.id.to_string()),
            args,
        )
        .map(|reply| {
            if reply.rebuild_now {
                live.agent.rebuild_prompt(Rebuild::Now("memory".to_owned()));
            }
            reply.text
        }),
        "journey" => crate::journey_cmd::slash::run(
            ctx.scope,
            &ctx.factory.store,
            ctx.factory
                .launch()
                .user_home()
                .map(std::path::PathBuf::as_path),
            args,
        ),
        "personality" => crate::slash_handlers::personality(ctx.scope, args).map(|(text, now)| {
            if let Some(name) = now {
                live.agent.apply_personality_now(&name);
            }
            text
        }),
        "yolo" => Ok(yolo(live, args)),
        "egress" => egress(live, args),
        "config" => config(ctx.scope, args),
        _ => return None,
    })
}

/// The commands of the profile without a loaded session: the built-ins
/// and its quick commands (a session adds its skills).
///
/// # Panics
///
/// Never: the built-in table's names are unique (a unit test checks it).
#[must_use]
pub fn profile_registry(scope: &ProfileScope) -> Registry {
    let mut registry = Registry::new().expect("the built-in commands are unique");
    quick::register(&mut registry, &scope.config().quick_commands);
    registry
}

/// The commands of `live`: the built-ins, the profile's quick commands,
/// the commands of the profile's current plugins and the session's skill
/// commands; load warnings are logged.
#[must_use]
pub fn registry(scope: &ProfileScope, live: &Live) -> Registry {
    let mut registry = profile_registry(scope);
    plugins::register(&mut registry, live);
    let core = registry.builtin_names();
    let table = pmt_skills::slash::table(&live.skills, &core);
    for (slug, skill) in &table.commands {
        let declaration = slash::Declaration {
            name: slug.clone(),
            description: format!("skill {skill}"),
            ..slash::Declaration::default()
        };
        if let Err(err) = registry.register(&declaration, slash::Source::Skill) {
            tracing::warn!(error = %err, "skill command not loaded");
        }
    }
    for warning in &registry.warnings {
        tracing::warn!("{warning}");
    }
    registry
}

/// A message that starts with skill commands, as one user message that
/// loads the skills then gives the instruction (never a system prompt
/// change): the notices to show and the message.
///
/// # Errors
///
/// What to tell the person when the skills cannot load or the first
/// token is no command.
pub async fn skill_line(
    live: &Live,
    registry: &Registry,
    text: &str,
) -> Result<(Vec<String>, String), String> {
    let core = registry.builtin_names();
    match pmt_skills::dispatch::expand(&live.skills, text, &core).await {
        Some(Ok(expanded)) => Ok((expanded.notices, expanded.message)),
        Some(Err(message)) => Err(message),
        None => {
            let (token, _) = slash::split_line(text);
            Err(format!("Unknown command: /{token}"))
        }
    }
}

/// `/learn [focus]`: the learn instruction as the next message.
fn learn(ctx: &Ctx<'_>, args: &str) -> Result<Reply, Error> {
    let chars = ctx.scope.config().skills.index_description_chars;
    pmt_skills::learn::message(args, chars).map(|message| Reply::Send(TurnInput::text(message)))
}

/// `/retry`: undoes the last exchange and sends its message again.
fn retry(ctx: &Ctx<'_>) -> Result<Reply, Error> {
    Ok(match session::retry(ctx.factory, ctx.live.id)? {
        Some(input) => Reply::Send(input),
        None => Reply::Text("nothing to retry".to_owned()),
    })
}

fn title(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let store = &ctx.factory.store;
    if args.is_empty() {
        let session = store.session(ctx.live.id)?;
        return Ok(session.title.map_or_else(
            || "this session has no title".to_owned(),
            |t| format!("title: {t}"),
        ));
    }
    store.set_title(ctx.live.id, Some(args), pmt_store::TitleSource::User)?;
    Ok(format!("title set to {args}"))
}

fn budget(ctx: &Ctx<'_>, args: &str) -> Result<String, Error> {
    let current = ctx.live.config.get();
    let (text, _) = crate::slash_handlers::budget(
        ctx.scope,
        &ctx.factory.store,
        ctx.live.id,
        &current.config().budgets,
        args,
    )?;
    Ok(text)
}

fn approvals(live: &Live) -> String {
    let scope = live.config.get();
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

/// `/egress [status] | allow <host> | deny <host> | mode <m>`: changes the
/// session's egress policy in place (the system prompt and tool array stay
/// as they are, so the prompt cache holds).
fn egress(live: &Live, args: &str) -> Result<String, Error> {
    let (action, value) = args.trim().split_once(' ').unwrap_or((args.trim(), ""));
    if !matches!(action, "" | "status") {
        crate::egress::Change::parse(action, value)?.apply(&live.egress.hosts);
    }
    Ok(crate::egress::describe(&live.egress.hosts))
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
fn config(scope: &ProfileScope, args: &str) -> Result<String, Error> {
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
