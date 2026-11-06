//! A spawn: preconditions, validation, planning, admission, recording and
//! dispatch (spec: subagent-delegation, Spawn preconditions and refusals;
//! Background and synchronous dispatch).

use std::sync::Arc;
use std::time::Duration;

use pmt_agent::turn::Interrupt;
use pmt_config::schema::BackgroundMode;
use pmt_core::cancel::CancelToken;
use pmt_core::error::codes;
use pmt_store::delegations::{Delivery, SubagentRow};
use pmt_tools::ToolResult;
use serde_json::{Value, json};

use super::{ApprovalPlan, ChildPlan, Delegator, Dispatch};
use crate::Refusal;
use crate::budget::{self, ChildBudget, Remaining};
use crate::contract::{Call, Task};
use crate::grant::{self, Narrowing, ParentGrant};
use crate::limiter::{Admission, Limiter};
use crate::result::SubagentResult;
use crate::router::Router;
use crate::routing;
use crate::tree::{Entry, NewEntry, Registry};

/// A task ready to run, or refused before running.
pub(super) enum Planned {
    /// It runs with this plan and goal.
    Run(Box<ChildPlan>, String),
    /// A router refused it.
    Refused(String),
}

/// How a spawn's children run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    /// Inside the calling turn (synchronous).
    Turn,
    /// In the background, completion owed to the session.
    Background,
    /// In the background for a plugin, which collects the results.
    Plugin,
}

/// One child of a call: its entry and what it runs.
pub(super) struct Child {
    pub entry: Arc<Entry>,
    pub planned: Planned,
    pub admission: Admission,
    pub output_schema: Option<Value>,
}

impl Delegator {
    pub(super) async fn spawn(
        self: &Arc<Self>,
        call: Call,
        cancel: &CancelToken,
    ) -> Result<ToolResult, Refusal> {
        let (planned, admissions) = self.admit(&call).await?;
        let background = self.runs_in_background(&call);
        let mode = if background.is_ok() {
            Mode::Background
        } else {
            Mode::Turn
        };
        let delegation_id = crate::ids::delegation_id();
        let children = self.register(&delegation_id, &call, planned, admissions, mode, cancel)?;
        match background {
            Ok(sink) => Ok(self.dispatch_background(&delegation_id, children, sink)),
            Err(note) => Ok(self
                .run_synchronously(&delegation_id, children, call.tasks.len(), &note)
                .await),
        }
    }

    /// Everything before a spawn is recorded: the preconditions, task
    /// validation, the root budget, a plan per task (route, grant, model)
    /// and admission under the concurrency limit and the one-shot budget.
    pub(super) async fn admit(
        &self,
        call: &Call,
    ) -> Result<(Vec<Planned>, Vec<Admission>), Refusal> {
        self.preconditions()?;
        let router = self.router()?;
        crate::validate::tasks(&call.tasks, &self.ctx.agents.names())?;
        crate::validate::router_params(&call.tasks, &router.params())?;
        let remaining = self.root_remaining()?;
        let parent = self.parent();
        let planned = self.plan(call, &parent, router.as_ref(), remaining).await?;
        let config = &self.ctx.config;
        let limiter = Limiter::for_profile(
            self.ctx.scope.dir(),
            config.max_concurrent_children,
            config.max_queued,
        );
        let admissions = limiter.admit(&self.ctx.root.to_string(), planned.len())?;
        if let Some(oneshot) = &self.ctx.oneshot {
            // A task the router refused spawns nothing and costs nothing.
            let children = planned
                .iter()
                .filter(|p| matches!(p, Planned::Run(..)))
                .count();
            oneshot.charge(u32::try_from(children).unwrap_or(u32::MAX))?;
        }
        Ok((planned, admissions))
    }

    /// Refusals that build nothing: a closed session, a paused profile,
    /// the depth limit.
    fn preconditions(&self) -> Result<(), Refusal> {
        self.check_open()?;
        if (self.ctx.paused)() {
            return Err(Refusal::new(
                &codes::DELEGATION_PAUSED,
                "Delegation spawning is paused. Resume with /delegation resume.",
            ));
        }
        let (depth, max) = (self.ctx.depth, self.ctx.config.max_spawn_depth);
        if depth >= max {
            return Err(Refusal::new(
                &codes::DELEGATION_DEPTH_LIMIT,
                format!("Delegation depth limit reached (depth={depth}, max_spawn_depth={max})."),
            ));
        }
        Ok(())
    }

    /// What the root lineage has left under `[budgets]`; nothing left
    /// refuses the spawn.
    fn root_remaining(&self) -> Result<Remaining, Refusal> {
        let budgets = &self.ctx.scope.config().budgets;
        if budgets.root_usd <= 0.0 && budgets.root_tokens == 0 {
            return Ok(Remaining::default());
        }
        let spent = self
            .ctx
            .store
            .usage_sum(&pmt_store::UsageFilter {
                root: Some(self.ctx.root),
                ..pmt_store::UsageFilter::default()
            })
            .map_err(|e| Refusal::from(&e))?;
        let tokens = &spent.tokens;
        let used = tokens.input + tokens.output + tokens.cache_read + tokens.cache_write;
        let left_usd = (budgets.root_usd > 0.0).then_some(budgets.root_usd - spent.cost_usd);
        let left_tokens =
            (budgets.root_tokens > 0).then(|| budgets.root_tokens.saturating_sub(used));
        if left_usd.is_some_and(|usd| usd <= 0.0) || left_tokens == Some(0) {
            return Err(Refusal::new(
                &codes::DELEGATION_BUDGET_EXHAUSTED,
                "The root session's budget is spent; no subagent can be given a share of it.",
            ));
        }
        Ok(Remaining {
            usd: left_usd,
            tokens: left_tokens,
        })
    }

    async fn plan(
        &self,
        call: &Call,
        parent: &ParentGrant,
        router: &dyn Router,
        remaining: Remaining,
    ) -> Result<Vec<Planned>, Refusal> {
        let config = &self.ctx.config;
        let iterations: Vec<u32> = call
            .tasks
            .iter()
            .map(|t| {
                self.agent_of(t)
                    .and_then(|a| a.max_iterations)
                    .unwrap_or(config.max_iterations)
            })
            .collect();
        let budgets = budget::split(call.budget.as_ref(), &call.tasks, &iterations, remaining);
        let mut out = Vec::with_capacity(call.tasks.len());
        for (task, budget) in call.tasks.iter().zip(budgets) {
            out.push(self.plan_task(task, parent, router, budget).await?);
        }
        Ok(out)
    }

    fn agent_of(&self, task: &Task) -> Option<&crate::agents::AgentDefinition> {
        task.agent
            .as_deref()
            .and_then(|name| self.ctx.agents.get(name))
    }

    async fn plan_task(
        &self,
        task: &Task,
        parent: &ParentGrant,
        router: &dyn Router,
        budget: ChildBudget,
    ) -> Result<Planned, Refusal> {
        let route = match router.route(task).await {
            Ok(route) => route,
            Err(message) => return Ok(Planned::Refused(message)),
        };
        let agent = self.agent_of(task);
        let depth = self.ctx.depth + 1;
        let grant = grant::derive(
            &self.ctx.registry,
            parent,
            &Narrowing {
                task_toolsets: task.toolsets.as_deref(),
                agent_toolsets: agent.and_then(|a| a.toolsets.as_deref()),
                capabilities: None,
                profile: None,
            },
            depth,
            self.ctx.config.max_spawn_depth,
        )?;
        let model = routing::choose(
            task.model.as_deref().or(route.model.as_deref()),
            agent.and_then(|a| a.model.as_deref()),
            self.ctx.config.model.as_deref(),
        );
        let system = system_prompt(task, agent, self.output_schema(task).as_ref());
        Ok(Planned::Run(
            Box::new(ChildPlan {
                subagent_id: crate::ids::subagent_id(),
                parent: self.ctx.session,
                depth,
                grant,
                model,
                profile: route.profile,
                backend: route.backend,
                system,
                memory: self.ctx.memory.clone(),
                budget,
                approvals: ApprovalPlan {
                    policy: self.ctx.config.child_approval,
                    timeout: Duration::from_secs(self.ctx.config.escalation_timeout_secs),
                },
                images: task.images.clone(),
                events: Arc::clone(&self.ctx.events),
            }),
            goal_input(task),
        ))
    }

    /// The task's schema, or its agent's.
    pub(super) fn output_schema(&self, task: &Task) -> Option<Value> {
        task.output_schema
            .clone()
            .or_else(|| self.agent_of(task).and_then(|a| a.output_schema.clone()))
    }

    /// The completion sink when this call runs in the background, else
    /// why it waits.
    fn runs_in_background(&self, call: &Call) -> Result<Arc<dyn super::Completions>, String> {
        if self.ctx.depth > 0 {
            return Err("Nested spawns run synchronously.".to_owned());
        }
        if self.ctx.config.background == BackgroundMode::Never {
            return Err(
                "delegation.background = \"never\": the call waited for its children.".to_owned(),
            );
        }
        if call
            .tasks
            .iter()
            .any(|t| self.agent_of(t).and_then(|a| a.background) == Some(false))
        {
            return Err("An agent definition asks to run in the foreground.".to_owned());
        }
        match &self.ctx.dispatch {
            Dispatch::Background(sink) => Ok(Arc::clone(sink)),
            Dispatch::Synchronous(why) => Err(why.clone()),
        }
    }

    /// Records the call and its children and creates their live entries.
    pub(super) fn register(
        &self,
        delegation_id: &str,
        call: &Call,
        planned: Vec<Planned>,
        admissions: Vec<Admission>,
        mode: Mode,
        cancel: &CancelToken,
    ) -> Result<Vec<Child>, Refusal> {
        let conversation = self.conversation();
        // Background and plugin children outlive the turn: they stop with
        // the session; synchronous ones stop with the turn too.
        let background = mode != Mode::Turn;
        let token = if background {
            self.lifetime().child()
        } else {
            cancel.child()
        };
        let mut children = Vec::with_capacity(planned.len());
        let mut rows = Vec::with_capacity(planned.len());
        for ((task, planned), admission) in call.tasks.iter().zip(planned).zip(admissions) {
            let subagent_id = match &planned {
                Planned::Run(plan, _) => plan.subagent_id.clone(),
                Planned::Refused(_) => crate::ids::subagent_id(),
            };
            let entry = Registry::process().add(
                NewEntry {
                    subagent_id: subagent_id.clone(),
                    delegation_id: delegation_id.to_owned(),
                    task_index: task.index,
                    goal: task.goal.clone(),
                    depth: self.ctx.depth + 1,
                    parent_session: self.ctx.session.to_string(),
                    parent_conversation: conversation.clone(),
                    root_session: self.ctx.root.to_string(),
                    background,
                    queued: admission.is_queued(),
                },
                Interrupt::child_of(&token),
            );
            rows.push(SubagentRow {
                id: subagent_id,
                delegation_id: delegation_id.to_owned(),
                task_index: i64::try_from(task.index).unwrap_or(i64::MAX),
                session_id: None,
                parent_session_id: self.ctx.session.to_string(),
                root_session_id: self.ctx.root.to_string(),
                depth: i64::from(self.ctx.depth + 1),
                goal: task.goal.clone(),
                status: if admission.is_queued() {
                    "queued"
                } else {
                    "running"
                }
                .to_owned(),
                result: None,
            });
            children.push(Child {
                entry,
                planned,
                admission,
                output_schema: self.output_schema(task),
            });
        }
        // Only a background call owes the session a completion; a plugin's
        // children report to the plugin.
        let delivery = if mode == Mode::Background {
            Delivery::Pending
        } else {
            Delivery::Inline
        };
        self.ctx
            .store
            .record_delegation(delegation_id, self.ctx.session, delivery, &rows)
            .map_err(|e| Refusal::from(&e))?;
        Ok(children)
    }

    /// Starts the children and answers at once with the handle.
    fn dispatch_background(
        self: &Arc<Self>,
        delegation_id: &str,
        children: Vec<Child>,
        sink: Arc<dyn super::Completions>,
    ) -> ToolResult {
        let ids: Vec<String> = children
            .iter()
            .map(|c| c.entry.subagent_id.clone())
            .collect();
        let queued: Vec<String> = children
            .iter()
            .filter(|c| c.admission.is_queued())
            .map(|c| c.entry.subagent_id.clone())
            .collect();
        let me = Arc::clone(self);
        let id = delegation_id.to_owned();
        tokio::spawn(async move {
            let tasks = children.len();
            let results = me.run_all(children).await;
            me.complete(&id, &results, tasks, &sink).await;
        });
        ToolResult::ok(
            json!({
                "status": "dispatched",
                "delegation_id": delegation_id,
                "subagent_ids": ids,
                "queued": queued,
                "note": "The children run in the background; their results arrive later as a delegation_completed input. Use action=list/status/steer/stop/wait to follow them.",
            })
            .to_string(),
        )
    }

    async fn run_synchronously(
        self: &Arc<Self>,
        delegation_id: &str,
        children: Vec<Child>,
        tasks: usize,
        note: &str,
    ) -> ToolResult {
        let results = self.run_all(children).await;
        let inserted = self.for_context(&results, tasks);
        ToolResult::ok(
            json!({
                "status": "completed",
                "delegation_id": delegation_id,
                "results": inserted,
                "note": note,
            })
            .to_string(),
        )
    }

    /// Runs every child concurrently; results in task order.
    pub(super) async fn run_all(self: &Arc<Self>, children: Vec<Child>) -> Vec<SubagentResult> {
        let handles: Vec<_> = children
            .into_iter()
            .map(|child| {
                let me = Arc::clone(self);
                tokio::spawn(async move { me.run_child(child).await })
            })
            .collect();
        let mut results = Vec::with_capacity(handles.len());
        for handle in handles {
            if let Ok(result) = handle.await {
                results.push(result);
            }
        }
        results
    }
}

/// The child's ephemeral system prompt: its role, the goal, the context,
/// its agent definition and the answer format.
fn system_prompt(
    task: &Task,
    agent: Option<&crate::agents::AgentDefinition>,
    schema: Option<&Value>,
) -> String {
    let mut parts = vec![
        "You are a subagent working on one delegated task for a parent agent. Work \
         autonomously: nobody can answer questions. Finish with a concise final answer that \
         reports what you found or did; the parent sees only that answer."
            .to_owned(),
    ];
    if let Some(agent) = agent.filter(|a| !a.prompt.is_empty()) {
        parts.push(agent.prompt.clone());
    }
    parts.push(format!("Your task:\n{}", task.goal.trim()));
    if let Some(context) = task.context.as_deref().filter(|c| !c.trim().is_empty()) {
        parts.push(format!("Context from the parent:\n{}", context.trim()));
    }
    if let Some(schema) = schema {
        parts.push(crate::output::instructions(schema));
    }
    parts.join("\n\n")
}

/// The child's first message.
fn goal_input(task: &Task) -> String {
    task.goal.trim().to_owned()
}
