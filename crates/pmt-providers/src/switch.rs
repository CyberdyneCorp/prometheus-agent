//! The `/model` switch (spec: model-providers, /model switch command):
//! `/model [<provider>/]<model> [--provider ID] [--reasoning LEVEL]
//! [--once|--session|--global]`.
//!
//! The target is validated (route resolution with its own credential, tool
//! support, minimum context window) before anything changes, so a failed
//! switch leaves the session's route as it was. A successful switch takes
//! effect at the next turn: `--once` for that turn only, `--session` (the
//! default once a model is configured) until the session ends, `--global`
//! also writing `model.provider`/`model.default` (the default on a fresh
//! profile). A switch to another route restarts the provider's prompt
//! cache, which the notice says.

use pmt_config::schema::ReasoningLevel;
use pmt_core::error::{Error, codes};

use crate::capability;
use crate::metadata::Catalog;
use crate::resolve::{Resolver, Route, Target, Trace};
use crate::target;

/// How long a switch lasts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The next turn only.
    Once,
    /// Until the session ends.
    Session,
    /// This session and `config.toml`.
    Global,
}

/// Parsed `/model` arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Args {
    /// `[<provider>/]<model>` or an alias.
    pub reference: Option<String>,
    /// `--provider ID`.
    pub provider: Option<String>,
    /// `--reasoning LEVEL`.
    pub reasoning: Option<ReasoningLevel>,
    /// `--once`, `--session` or `--global`.
    pub scope: Option<Scope>,
}

/// Usage of `/model`.
pub const USAGE: &str = "/model [<provider>/]<model> [--provider ID] [--reasoning none|minimal|low|medium|high|xhigh|max] [--once|--session|--global]";

impl Args {
    /// Parses the text after `/model`.
    ///
    /// # Errors
    ///
    /// `cli.usage` for unknown flags, missing flag values, two scopes, or
    /// `--once` with `--global` (`/model --once cannot be combined with
    /// --global`).
    pub fn parse(text: &str) -> Result<Self, Error> {
        let mut args = Self::default();
        let mut scopes = Vec::new();
        let mut words = text.split_whitespace();
        while let Some(word) = words.next() {
            match word {
                "--once" => scopes.push(Scope::Once),
                "--session" => scopes.push(Scope::Session),
                "--global" => scopes.push(Scope::Global),
                "--provider" => args.provider = Some(value(words.next(), word)?.to_owned()),
                "--reasoning" => {
                    let level = value(words.next(), word)?;
                    args.reasoning = Some(
                        ReasoningLevel::parse(level)
                            .ok_or_else(|| usage(&format!("unknown reasoning level '{level}'")))?,
                    );
                }
                flag if flag.starts_with("--") => {
                    return Err(usage(&format!("unknown option '{flag}'")));
                }
                reference if args.reference.is_none() => {
                    args.reference = Some(reference.to_owned());
                }
                extra => return Err(usage(&format!("unexpected argument '{extra}'"))),
            }
        }
        if scopes.contains(&Scope::Once) && scopes.contains(&Scope::Global) {
            return Err(Error::new(
                &codes::CLI_USAGE,
                "/model --once cannot be combined with --global",
            ));
        }
        if scopes.len() > 1 {
            return Err(usage("give at most one of --once, --session, --global"));
        }
        args.scope = scopes.first().copied();
        Ok(args)
    }
}

fn value<'a>(word: Option<&'a str>, flag: &str) -> Result<&'a str, Error> {
    word.filter(|w| !w.starts_with("--"))
        .ok_or_else(|| usage(&format!("{flag} needs a value")))
}

fn usage(message: &str) -> Error {
    Error::new(&codes::CLI_USAGE, format!("{message}; usage: {USAGE}"))
}

/// A target chosen for later turns.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Choice {
    target: Target,
    reasoning: Option<ReasoningLevel>,
}

/// A session's model overrides above the profile config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionModel {
    session: Option<Choice>,
    once: Option<Choice>,
    reasoning: Option<ReasoningLevel>,
}

impl SessionModel {
    /// The target of the next turn (`None`: the session's resolved
    /// primary) and its reasoning level; a `--once` choice is used up.
    pub fn next_turn(&mut self) -> (Option<Target>, Option<ReasoningLevel>) {
        let choice = self.once.take().or_else(|| self.session.clone());
        let reasoning = choice.as_ref().and_then(|c| c.reasoning).or(self.reasoning);
        (choice.map(|c| c.target), reasoning)
    }

    /// The session's reasoning level (`/reasoning <level>`).
    #[must_use]
    pub fn reasoning(&self) -> Option<ReasoningLevel> {
        self.reasoning
    }

    /// Sets the session's reasoning level from the next request.
    pub fn set_reasoning(&mut self, level: Option<ReasoningLevel>) {
        self.reasoning = level;
    }
}

/// What the session needs from a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Requirements {
    /// The session offers tools.
    pub tools: bool,
    /// `model.min_context_window`.
    pub min_context_window: u64,
}

/// A validated switch.
#[derive(Debug, Clone, PartialEq)]
pub struct Switched {
    /// The new route (used from the next turn).
    pub route: Route,
    /// How long it lasts.
    pub scope: Scope,
    /// The text to show.
    pub notice: String,
}

impl Switched {
    /// Whether `model.provider`/`model.default` must be written.
    #[must_use]
    pub fn persists(&self) -> bool {
        self.scope == Scope::Global
    }
}

/// The session's environment for a switch.
pub struct Context<'a> {
    /// The profile's resolver.
    pub resolver: &'a Resolver<'a>,
    /// The profile's catalog.
    pub catalog: &'a Catalog,
    /// The route the session uses now.
    pub current: &'a Route,
    /// What the session needs.
    pub needs: Requirements,
}

impl std::fmt::Debug for Context<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context").finish_non_exhaustive()
    }
}

/// Validates `args` and, on success only, records the choice in `state`.
///
/// # Errors
///
/// `model.invalid_ref`, `cli.usage` (no model named), the resolution error
/// of the target (`provider.unknown`, `provider.missing_credential`...),
/// `model.capability_missing` (tools), `model.context_too_small`.
pub async fn switch(
    args: &Args,
    state: &mut SessionModel,
    ctx: &Context<'_>,
) -> Result<Switched, Error> {
    let reference = args
        .reference
        .as_deref()
        .ok_or_else(|| usage("name a model"))?;
    let config = ctx.resolver.config;
    let target = match &args.provider {
        // `--provider` takes the reference literally as its model id.
        Some(provider) => Target {
            provider: provider.clone(),
            model: Some(reference.to_owned()),
            ..Target::default()
        },
        None => target::parse(
            reference,
            &config.model,
            ctx.resolver.registry,
            Some(&ctx.current.provider),
        )?,
    };
    let route = ctx
        .resolver
        .resolve_target(&target, &mut Trace::default())
        .await?;
    let (meta, _) = ctx
        .catalog
        .metadata_at(&route.provider, &route.model, Some(&route.base_url));
    let needs = capability::Needs {
        tools: ctx.needs.tools,
        ..capability::Needs::default()
    };
    capability::check(&route.label(), &meta, &needs)?;
    capability::check_min_context(&route.label(), &meta, ctx.needs.min_context_window)?;
    let configured = config.model.provider.is_some() || config.model.default.is_some();
    let scope = args.scope.unwrap_or(if configured {
        Scope::Session
    } else {
        Scope::Global
    });
    let choice = Choice {
        target,
        reasoning: args.reasoning,
    };
    match scope {
        Scope::Once => state.once = Some(choice),
        Scope::Session | Scope::Global => {
            state.once = None;
            state.session = Some(choice);
        }
    }
    let notice = notice(ctx.current, &route, scope);
    Ok(Switched {
        route,
        scope,
        notice,
    })
}

fn notice(current: &Route, next: &Route, scope: Scope) -> String {
    let lasting = match scope {
        Scope::Once => "for the next turn only",
        Scope::Session => "from the next turn until the session ends",
        Scope::Global => "from the next turn, saved as the profile default",
    };
    let text = format!("model: {} {lasting}", next.label());
    if current.identity() == next.identity() {
        return text;
    }
    format!(
        "{text}; the prompt cache starts over (cache break: {} cannot reuse the prefix cached for {})",
        next.label(),
        current.label()
    )
}

#[cfg(test)]
mod tests;
