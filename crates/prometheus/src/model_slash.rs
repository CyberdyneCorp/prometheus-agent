//! Bodies of the `/model` and `/reasoning` slash commands (spec:
//! model-providers, /model switch command; Reasoning effort defaults,
//! overrides and display). The slash registry (cli group) registers them;
//! the session passes its [`SessionModel`], its current route and the
//! route's metadata, and shows the returned text. Neither changes the
//! system prompt: a model or effort change applies from the next request.

use pmt_config::ProfileScope;
use pmt_config::schema::ReasoningLevel;
use pmt_core::error::{Error, codes};
use pmt_providers::metadata::ModelMetadata;
use pmt_providers::reasoning;
use pmt_providers::resolve::Route;
use pmt_providers::switch::{self, Args, SessionModel};

/// Usage of `/model` (surfaces cli, tui, desktop, messaging).
pub const MODEL_USAGE: &str = switch::USAGE;

/// Usage of `/reasoning`.
pub const REASONING_USAGE: &str = "/reasoning [<level>|show|hide|full|clamp] [--global]";

/// `/model [<provider>/]<model> [--provider ID] [--reasoning L]
/// [--once|--session|--global]`: validates the target and, only when it
/// is usable, records it for the next turn (and writes
/// `model.provider`/`model.default` for `--global` or a first pick).
/// Bare `/model` shows the current route.
///
/// # Errors
///
/// `cli.usage`, `model.invalid_ref`, the target's resolution or capability
/// error, config write errors; the session's route is then unchanged.
pub async fn model(
    scope: &ProfileScope,
    session: &mut SessionModel,
    ctx: &switch::Context<'_>,
    args: &str,
) -> Result<String, Error> {
    if args.trim().is_empty() {
        return Ok(pmt_i18n::t!(
            strings(scope),
            "slash.model.current",
            route = ctx.current.label(),
            billing = ctx.current.billing_source(),
            usage = MODEL_USAGE
        ));
    }
    let args = Args::parse(args)?;
    let mut next = session.clone();
    let switched = switch::switch(&args, &mut next, ctx).await?;
    if switched.persists() {
        let reference = args.reference.as_deref().unwrap_or_default();
        let alias = pmt_providers::target::alias(&scope.config().model, reference).is_some();
        let default = if alias {
            reference
        } else {
            switched.route.model.as_str()
        };
        scope.profile().config_file().set_all(&[
            ("model.provider", switched.route.provider.as_str()),
            ("model.default", default),
        ])?;
    }
    *session = next;
    Ok(switched.notice)
}

fn strings(scope: &ProfileScope) -> std::sync::Arc<pmt_i18n::Strings> {
    pmt_config::language::strings(scope.dir(), pmt_i18n::Surface::Core)
}

/// `/reasoning [<level>|show|hide|full|clamp] [--global]` on the session's
/// current route (`meta` is its metadata): a level sets the session's
/// effort from the next request (`--global` also writes
/// `agent.reasoning_effort`) and reports a clamp to what the model
/// supports; `show`/`hide` set `display.show_reasoning`; `full`/`clamp`
/// set `display.reasoning_full`; no argument shows the effort.
///
/// # Errors
///
/// `cli.usage` for other arguments; config write errors.
pub fn reasoning(
    scope: &ProfileScope,
    session: &mut SessionModel,
    route: &Route,
    meta: &ModelMetadata,
    args: &str,
) -> Result<String, Error> {
    let words: Vec<&str> = args.split_whitespace().collect();
    let (global, rest): (Vec<&str>, Vec<&str>) = words.iter().partition(|w| **w == "--global");
    let global = !global.is_empty();
    let agent = &scope.config().agent;
    let strings = strings(scope);
    let display = |key: &str, value: &str, text: &str| -> Result<String, Error> {
        scope.profile().config_file().set(key, value)?;
        Ok(strings.get(text).to_owned())
    };
    match rest.as_slice() {
        [] => {
            let effort = reasoning::effort(agent, session.reasoning(), &route.model, meta)?;
            Ok(pmt_i18n::t!(
                strings,
                "slash.reasoning.current",
                effort = effort.describe(),
                route = route.label()
            ))
        }
        ["show"] => display("display.show_reasoning", "true", "slash.reasoning.shown"),
        ["hide"] => display("display.show_reasoning", "false", "slash.reasoning.hidden"),
        ["full"] => display("display.reasoning_full", "true", "slash.reasoning.full"),
        ["clamp"] => display(
            "display.reasoning_full",
            "false",
            "slash.reasoning.clamped_display",
        ),
        [level] => {
            let level = ReasoningLevel::parse(level).ok_or_else(usage)?;
            let effort = reasoning::effort(agent, Some(level), &route.model, meta)?;
            if global {
                scope
                    .profile()
                    .config_file()
                    .set("agent.reasoning_effort", level.as_str())?;
            }
            session.set_reasoning(Some(level));
            let (level, route) = (level.as_str(), route.label());
            Ok(match effort.sent {
                None => pmt_i18n::t!(
                    strings,
                    "slash.reasoning.set_unsupported",
                    level = level,
                    route = route
                ),
                Some(sent) if effort.clamped => pmt_i18n::t!(
                    strings,
                    "slash.reasoning.set_clamped",
                    level = level,
                    sent = sent.as_str(),
                    route = route
                ),
                Some(_) => pmt_i18n::t!(strings, "slash.reasoning.set", level = level),
            })
        }
        _ => Err(usage()),
    }
}

fn usage() -> Error {
    Error::new(&codes::CLI_USAGE, format!("usage: {REASONING_USAGE}"))
}
