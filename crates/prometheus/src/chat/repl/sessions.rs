//! Session commands of the line interface: `/new`, `/clear`, `/undo`,
//! `/retry`, `/rewind`, `/fork`, `/resume`, `/sessions`, `/compress`,
//! `/model`, `/reasoning`, `/snapshot`, `/usage` and the `--now` path of
//! `/tools` (spec: cli, Built-in slash command set; Usage display;
//! Deferred effects with `--now`).

use std::fmt::Write as _;
use std::sync::Arc;

use pmt_agent::prompt::frozen::Rebuild;
use pmt_config::ProfileScope;
use pmt_core::error::{Error, codes};
use pmt_core::id::{MessageId, SessionId};
use pmt_store::{EndReason, ListOptions, NewMessage, Role, UsageScope, UsageTotals, Visibility};
use pmt_tools::Selection;

use super::{Flow, Repl};
use crate::chat::session::model::target_of;
use crate::chat::session::{Factory, Live};

impl Repl {
    /// Ends the current session and opens `next` (a new row when `None`).
    pub(super) async fn switch_to(
        &mut self,
        next: Option<SessionId>,
        reason: EndReason,
    ) -> Result<SessionId, Error> {
        let target = self.live().model.target();
        let id = match next {
            Some(id) => id,
            None => self.factory.create_session(None)?,
        };
        self.finish(reason).await;
        let live = self.factory.open(id, &target).await?;
        self.attach(live);
        self.load_registry();
        Ok(id)
    }

    /// `/new`, `/clear`: a fresh session.
    pub(super) async fn new_session(
        &mut self,
        _parent: Option<SessionId>,
        _title: Option<&str>,
    ) -> Result<String, Error> {
        let id = self.switch_to(None, EndReason::NewSession).await?;
        Ok(format!("new session {id}"))
    }

    /// `/undo`: the last exchange leaves the active transcript.
    pub(super) fn undo(&self) -> Result<String, Error> {
        let rewound = self.factory.store.undo_last_turn(self.live().id, None)?;
        Ok(format!(
            "undid the last exchange ({} messages)",
            rewound.messages.len()
        ))
    }

    /// `/retry`: the last user message again, its answer replaced.
    pub(super) async fn retry(&mut self) -> Flow {
        let id = self.live().id;
        let last = match last_user_text(&self.factory, id) {
            Ok(Some(text)) => text,
            Ok(None) => {
                self.notice("nothing to retry");
                return Flow::Continue;
            }
            Err(err) => {
                self.error(&err);
                return Flow::Continue;
            }
        };
        if let Err(err) = self.factory.store.undo_last_turn(id, None) {
            self.error(&err);
            return Flow::Continue;
        }
        self.user_turn(last).await;
        Flow::Continue
    }

    /// `/rewind` lists the recent user messages; `/rewind N` goes back to
    /// before message N.
    pub(super) fn rewind(&self, args: &str) -> Result<String, Error> {
        let users = user_messages(&self.factory, self.live().id)?;
        if args.is_empty() {
            let mut text = String::from("recent messages (/rewind N goes back to before N):");
            let start = users.len().saturating_sub(10);
            for (n, (_, text_of)) in users.iter().enumerate().skip(start) {
                let first = text_of.lines().next().unwrap_or_default();
                let _ = write!(text, "\n  {}. {first}", n + 1);
            }
            return Ok(text);
        }
        let n: usize = args
            .parse()
            .ok()
            .filter(|n| (1..=users.len()).contains(n))
            .ok_or_else(|| {
                Error::new(
                    &codes::CLI_USAGE,
                    "usage: /rewind [N] (a number from the list)",
                )
            })?;
        let (target, _) = &users[n - 1];
        let rewound = self
            .factory
            .store
            .rewind(self.live().id, *target, None, None)?;
        Ok(format!("rewound {} messages", rewound.messages.len()))
    }

    /// `/fork [title]`: continues in a copy of this session.
    pub(super) async fn fork(&mut self, title: &str) -> Result<String, Error> {
        let parent = self.live().id;
        let store = &self.factory.store;
        let child = self.factory.create_session(Some(parent))?;
        let copied: Vec<NewMessage> = store
            .messages(parent, Visibility::Active)?
            .into_iter()
            .map(|m| NewMessage {
                id: MessageId::new(),
                role: m.role,
                content: m.content,
                tool_calls: m.tool_calls,
                reasoning: m.reasoning,
                provider_payload: m.provider_payload,
                tool_call_id: m.tool_call_id,
                tool_name: m.tool_name,
                metadata: m.metadata,
            })
            .collect();
        store.append_messages(child, &copied)?;
        if !title.is_empty() {
            store.set_title(child, Some(title), pmt_store::TitleSource::User)?;
        }
        self.switch_to(Some(child), EndReason::NewSession).await?;
        Ok(format!("forked {parent} into {child}"))
    }

    /// `/resume <ref>`: switches to another session and shows its recap.
    pub(super) async fn resume(&mut self, reference: &str) -> Result<String, Error> {
        if reference.is_empty() {
            return list(&self.factory);
        }
        let store = &self.factory.store;
        let id = store.resolve_session(reference)?;
        store.check_resumable(id, self.factory.scope.config().sessions.max_resume_messages)?;
        store.reopen_session(id)?;
        self.switch_to(Some(id), EndReason::UserExit).await?;
        self.recap();
        Ok(String::new())
    }

    /// `/compress [--preview] [here [N]] [focus]`.
    pub(super) async fn compress(&mut self, args: &str) -> Result<String, Error> {
        let manual = pmt_agent::compress::manual::parse(args)?;
        let live = self.live();
        let outcome = live.agent.compress(&live.scope, &manual).await?;
        Ok(format!(
            "compress: {:?}, context {} -> {} tokens",
            outcome.status, outcome.tokens_before, outcome.tokens_after
        )
        .to_lowercase())
    }

    /// `/model [<ref>] [...]`: shows the route or switches it from the
    /// next turn (`--global` also saves it).
    pub(super) async fn model(&mut self, args: &str) -> Result<String, Error> {
        let live = self.live();
        let target = live.model.target();
        let models = &self.factory.models;
        let resolver = models.resolver();
        let current = resolver
            .resolve_target(&target, &mut pmt_providers::resolve::Trace::default())
            .await?;
        let needs = pmt_providers::switch::Requirements {
            tools: true,
            min_context_window: self.factory.scope.config().model.min_context_window,
        };
        let ctx = pmt_providers::switch::Context {
            resolver: &resolver,
            catalog: models.catalog(),
            current: &current,
            needs,
        };
        let mut session = pmt_providers::switch::SessionModel::default();
        let text = crate::model_slash::model(&self.factory.scope, &mut session, &ctx, args).await?;
        if let (Some(next), _) = session.next_turn() {
            let route = resolver
                .resolve_target(&next, &mut pmt_providers::resolve::Trace::default())
                .await?;
            let next = target_of(&route);
            let port = models.port(&next).await?;
            live.model.switch(next, port);
        }
        Ok(text)
    }

    /// `/reasoning [...]` on the session's route.
    pub(super) async fn reasoning(&self, live: &Live, args: &str) -> Result<String, Error> {
        let models = &self.factory.models;
        let route = models
            .resolver()
            .resolve_target(
                &live.model.target(),
                &mut pmt_providers::resolve::Trace::default(),
            )
            .await?;
        let (meta, _) = models.catalog().metadata(&route.provider, &route.model);
        let mut session = pmt_providers::switch::SessionModel::default();
        crate::model_slash::reasoning(&self.factory.scope, &mut session, &route, &meta, args)
    }

    /// `/snapshot ...`; a restore replaces `state.db`, so the session's
    /// store handles are opened again.
    pub(super) async fn snapshot(&mut self, args: &str) -> Result<String, Error> {
        let text = crate::slash_handlers::snapshot(&self.factory.scope, args)?;
        if args.split_whitespace().next() == Some("restore") {
            self.factory.store = Arc::new(pmt_store::Store::open(&self.factory.scope)?);
            let id = self.live().id;
            let next = self.factory.store.session(id).ok().map(|_| id);
            self.switch_to(next, EndReason::UserExit).await?;
        }
        Ok(text)
    }

    /// `/tools enable|disable <toolset> --now`: the same session's next
    /// request carries the new tool set; one `prompt.rebuilt` with reason
    /// `tools --now` is recorded.
    pub(super) async fn apply_tools_now(
        &mut self,
        toolset: &str,
        enable: bool,
    ) -> Result<(), Error> {
        let live = self
            .live
            .take()
            .ok_or_else(|| Error::new(&codes::CLI_USAGE, "no session"))?;
        let mut enabled = live.selection.toolsets.clone();
        enabled.retain(|t| t != toolset);
        if enable {
            enabled.push(toolset.to_owned());
        }
        let disabled: Vec<String> = if enable {
            Vec::new()
        } else {
            vec![toolset.to_owned()]
        };
        let result = async {
            let selection = Selection::resolve(&self.factory.registry, &enabled, &disabled)?;
            self.factory.rebuild_tools(&live, &selection).await?;
            Ok::<Selection, Error>(selection)
        }
        .await;
        let mut live = live;
        let outcome = result.map(|selection| {
            live.selection = selection;
            live.agent.rebuild_prompt(Rebuild::Now("tools".to_owned()));
        });
        self.live = Some(live);
        outcome
    }
}

/// Saves a toolset change for the next sessions: `tools.disabled_toolsets`
/// gains or loses the toolset (and `tools.enabled_toolsets` gains it when
/// enabling).
///
/// # Errors
///
/// Config write errors.
pub(super) fn persist_toolset(
    scope: &ProfileScope,
    toolset: &str,
    enable: bool,
) -> Result<(), Error> {
    let file = scope.profile().config_file();
    let (text, _) = file.read()?;
    let saved: pmt_config::Config = toml::from_str(&text).unwrap_or_default();
    let tools = &saved.tools;
    let mut disabled = tools.disabled_toolsets.clone();
    let mut enabled = tools.enabled_toolsets.clone();
    disabled.retain(|t| t != toolset);
    if enable {
        if !enabled.iter().any(|t| t == toolset) {
            enabled.push(toolset.to_owned());
        }
    } else {
        disabled.push(toolset.to_owned());
    }
    let list = |names: &[String]| {
        let quoted: Vec<String> = names.iter().map(|n| format!("{n:?}")).collect();
        format!("[{}]", quoted.join(", "))
    };
    file.set_all(&[
        ("tools.enabled_toolsets", &list(&enabled)),
        ("tools.disabled_toolsets", &list(&disabled)),
    ])
}

/// `/usage`: this session and its whole lineage (compression
/// continuations, forks, delegated children), then the budgets.
///
/// # Errors
///
/// Store errors.
pub(super) fn usage(factory: &Factory, live: &Live) -> Result<String, Error> {
    let store = &factory.store;
    let session = store.usage_totals(UsageScope::Session(live.id))?;
    let lineage = store.usage_totals(UsageScope::Lineage(live.id))?;
    let mut text = format!(
        "{}\n{}",
        totals_line("session", &session),
        totals_line("lineage", &lineage)
    );
    let scope = &factory.scope;
    let (budgets, _) =
        crate::slash_handlers::budget(scope, store, live.id, &scope.config().budgets, "")?;
    if budgets != "no budgets set" {
        let _ = write!(text, "\n{budgets}");
    }
    Ok(text)
}

fn totals_line(label: &str, totals: &UsageTotals) -> String {
    let t = &totals.tokens;
    format!(
        "{label:<8} calls {}  input {}  output {}  cache read {}  cache write {}  reasoning {}  cost {}",
        totals.calls,
        t.input,
        t.output,
        t.cache_read,
        t.cache_write,
        t.reasoning,
        pmt_obs::usage::format_total_cost(totals)
    )
}

/// `/sessions`: the ten most recent sessions.
///
/// # Errors
///
/// Store errors.
pub(super) fn list(factory: &Factory) -> Result<String, Error> {
    let sessions = factory.store.list_sessions(&ListOptions {
        limit: Some(10),
        ..ListOptions::default()
    })?;
    let mut text = String::from("recent sessions (/resume <id or title>):");
    for session in sessions {
        let _ = write!(
            text,
            "\n  {}  {}  {}",
            session.id,
            session.last_activity_at.strftime("%Y-%m-%d %H:%M"),
            session.title.as_deref().unwrap_or("(untitled)")
        );
    }
    Ok(text)
}

/// The user messages of `id` (what the user typed), oldest first.
fn user_messages(factory: &Factory, id: SessionId) -> Result<Vec<(MessageId, String)>, Error> {
    Ok(factory
        .store
        .messages(id, Visibility::Active)?
        .into_iter()
        .filter(|m| m.role == Role::User && !authored(m))
        .map(|m| {
            let text = m
                .metadata
                .as_ref()
                .and_then(|meta| meta.get("user_text"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .or(m.content)
                .unwrap_or_default();
            (m.id, text)
        })
        .collect())
}

fn authored(message: &pmt_store::Message) -> bool {
    message
        .metadata
        .as_ref()
        .and_then(|m| m.get(pmt_agent::turn::transcript::AUTHORED))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

fn last_user_text(factory: &Factory, id: SessionId) -> Result<Option<String>, Error> {
    Ok(user_messages(factory, id)?.pop().map(|(_, text)| text))
}
