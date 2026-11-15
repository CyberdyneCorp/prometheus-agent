//! Many cards at once (spec: kanban, Swarm topology helper; Orchestrator
//! guidance, specify and decompose). A swarm and a decomposition are each
//! created in one transaction, after every assignee was checked, so a
//! reader sees either nothing or the whole graph.

use std::collections::{BTreeMap, BTreeSet};

use pmt_core::error::Error;
use rusqlite::{Transaction, params};
use serde::Deserialize;
use serde_json::json;

use crate::board::create::{self, NewTask, Profiles, extras, initial_status, insert, task_id};
use crate::board::{Board, event, load, open_parents};
use crate::db::sql;
use crate::model::{Actor, Status, invalid};
use crate::workflow::{self, Verb};

/// The cards of a swarm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Swarm {
    /// The root card (`done`, holding the goal and the blackboard).
    pub root: String,
    /// One card per worker profile, in the order given.
    pub workers: Vec<String>,
    /// Gated on every worker card.
    pub verifier: String,
    /// Gated on the verifier.
    pub synthesizer: String,
}

/// What a swarm is made of.
#[derive(Debug, Clone)]
pub struct SwarmSpec<'a> {
    /// The goal.
    pub goal: &'a str,
    /// Worker profiles.
    pub workers: &'a [String],
    /// The verifier profile.
    pub verifier: &'a str,
    /// The synthesizer profile.
    pub synthesizer: &'a str,
}

fn new_id(tx: &Transaction<'_>) -> String {
    let mut id = task_id();
    while load(tx, &id).is_ok() {
        id = task_id();
    }
    id
}

/// Inserts `new` (already checked) with its links; returns its id and
/// status.
fn add(
    tx: &Transaction<'_>,
    actor: &Actor,
    new: &NewTask,
    now: i64,
) -> Result<(String, Status), Error> {
    let id = new_id(tx);
    let label = actor.label();
    insert(tx, &id, new, &label, actor.run.as_deref(), now)?;
    extras(tx, &id, new, false, None)?;
    let (status, _) = initial_status(tx, &id, new)?;
    event(
        tx,
        Some(&id),
        actor.run.as_deref(),
        &label,
        "created",
        &json!({"title": new.title.trim(), "status": status, "parents": new.parents}),
        now,
    )?;
    Ok((id, status))
}

fn swarm_body(role: &str, goal: &str, root: &str) -> String {
    format!(
        "Swarm goal: {goal}\n\nYour role: {role}. The shared blackboard is the JSON comment thread of \
         task {root}: read it with kanban_show (task_id {root}) and append your findings as one JSON \
         object per kanban_comment on {root} ({{\"from\": \"<you>\", \"kind\": \"finding|decision|question\", \
         \"text\": \"...\"}})."
    )
}

impl Board {
    /// Creates a swarm: the root, the worker cards, the verifier and the
    /// synthesizer.
    ///
    /// # Errors
    ///
    /// `kanban.unknown_profile` (nothing is created), `kanban.invalid_task`.
    pub fn swarm(
        &mut self,
        actor: &Actor,
        spec: &SwarmSpec<'_>,
        profiles: &dyn Profiles,
        now: i64,
    ) -> Result<Swarm, Error> {
        if spec.goal.trim().is_empty() || spec.workers.is_empty() {
            return Err(invalid("a swarm needs a goal and at least one worker"));
        }
        let named = spec
            .workers
            .iter()
            .map(String::as_str)
            .chain([spec.verifier, spec.synthesizer]);
        for profile in named {
            if !profiles.exists(profile) {
                return Err(create::unknown_profile(profile));
            }
        }
        let label = actor.label();
        self.write(|tx| {
            let root = new_id(tx);
            let mut new = NewTask::titled(&format!("Swarm: {}", spec.goal.trim()));
            spec.goal.trim().clone_into(&mut new.body);
            insert(tx, &root, &new, &label, actor.run.as_deref(), now)?;
            tx.execute(
                "UPDATE tasks SET status = 'done', completed_at = ?2, result = ?3 WHERE id = ?1",
                params![root, now, "swarm root: the goal and the blackboard"],
            )
            .map_err(sql)?;
            let blackboard = json!({"swarm": spec.goal.trim(), "workers": spec.workers, "verifier": spec.verifier, "synthesizer": spec.synthesizer, "entries": []});
            tx.execute(
                "INSERT INTO comments(task_id, author, body, at) VALUES (?1, ?2, ?3, ?4)",
                params![root, label, blackboard.to_string(), now],
            )
            .map_err(sql)?;
            event(tx, Some(&root), None, &label, "created", &json!({"title": new.title, "status": "done", "swarm": true}), now)?;
            let mut workers = Vec::new();
            for profile in spec.workers {
                let mut card = NewTask::titled(&format!("{profile}: {}", spec.goal.trim()));
                card.body = swarm_body(profile, spec.goal.trim(), &root);
                card.assignee = Some(profile.clone());
                card.parents = vec![root.clone()];
                workers.push(add(tx, actor, &card, now)?.0);
            }
            let mut verifier = NewTask::titled(&format!("Verify: {}", spec.goal.trim()));
            verifier.body = swarm_body("verifier: check every worker's findings", spec.goal.trim(), &root);
            verifier.assignee = Some(spec.verifier.to_owned());
            verifier.parents.clone_from(&workers);
            let verifier = add(tx, actor, &verifier, now)?.0;
            let mut synth = NewTask::titled(&format!("Synthesize: {}", spec.goal.trim()));
            synth.body = swarm_body("synthesizer: write the final answer", spec.goal.trim(), &root);
            synth.assignee = Some(spec.synthesizer.to_owned());
            synth.parents = vec![verifier.clone()];
            let synthesizer = add(tx, actor, &synth, now)?.0;
            Ok(Swarm {
                root,
                workers,
                verifier,
                synthesizer,
            })
        })
    }
}

/// One child of a decomposition, as the auxiliary model proposes it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Child {
    /// A key other children name in `depends_on`.
    pub key: String,
    /// The title.
    pub title: String,
    /// Self-contained instructions, every shared decision included.
    #[serde(default)]
    pub body: String,
    /// An existing profile (or lane).
    pub assignee: Option<String>,
    /// Keys of the children it waits for.
    #[serde(default)]
    pub depends_on: Vec<String>,
}

/// A proposed child-task graph.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Graph {
    /// The children.
    pub tasks: Vec<Child>,
}

impl Graph {
    /// The graph in a model reply (the first JSON object in it).
    ///
    /// # Errors
    ///
    /// `kanban.invalid_task` when the reply holds no graph.
    pub fn parse(reply: &str) -> Result<Self, Error> {
        let start = reply.find('{');
        let end = reply.rfind('}');
        let text = match (start, end) {
            (Some(s), Some(e)) if s < e => &reply[s..=e],
            _ => return Err(invalid("the decomposition reply holds no JSON graph")),
        };
        serde_json::from_str(text)
            .map_err(|e| invalid(format!("the decomposition reply is not a task graph: {e}")))
    }

    /// Checks keys, assignees and dependencies: every assignee exists,
    /// keys are unique, dependencies name known keys and form no cycle.
    ///
    /// # Errors
    ///
    /// `kanban.invalid_task` naming every problem.
    pub fn validate(&self, profiles: &dyn Profiles) -> Result<(), Error> {
        let mut problems = Vec::new();
        if self.tasks.is_empty() {
            problems.push("the graph has no tasks".to_owned());
        }
        let keys = self.child_problems(profiles, &mut problems);
        for child in &self.tasks {
            for dep in child
                .depends_on
                .iter()
                .filter(|d| !keys.contains(d.as_str()))
            {
                problems.push(format!(
                    "task '{}' depends on unknown key '{dep}'",
                    child.key
                ));
            }
        }
        if problems.is_empty() && self.order().is_none() {
            problems.push("the dependencies form a cycle".to_owned());
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(invalid(format!(
                "decomposition refused: {}",
                problems.join("; ")
            )))
        }
    }

    /// The keys of the children, noting duplicate keys, blank titles and
    /// unknown assignees in `problems`.
    fn child_problems(
        &self,
        profiles: &dyn Profiles,
        problems: &mut Vec<String>,
    ) -> BTreeSet<&str> {
        let mut keys = BTreeSet::new();
        for child in &self.tasks {
            if !keys.insert(child.key.as_str()) {
                problems.push(format!("duplicate key '{}'", child.key));
            }
            if child.title.trim().is_empty() {
                problems.push(format!("task '{}' has no title", child.key));
            }
            if let Some(a) = child.assignee.as_deref()
                && create::assignee_lane(profiles, a).is_err()
            {
                problems.push(format!("unknown assignee '{a}' for task '{}'", child.key));
            }
        }
        keys
    }

    /// The children in dependency order, `None` on a cycle.
    fn order(&self) -> Option<Vec<&Child>> {
        let mut done: BTreeSet<&str> = BTreeSet::new();
        let mut out = Vec::new();
        while out.len() < self.tasks.len() {
            let next = self.tasks.iter().find(|c| {
                !done.contains(c.key.as_str())
                    && c.depends_on.iter().all(|d| done.contains(d.as_str()))
            })?;
            done.insert(&next.key);
            out.push(next);
        }
        Some(out)
    }
}

impl Board {
    /// Creates `graph` under task `id` in one transaction: each child as
    /// a task with its dependencies linked, `id` waiting for the children
    /// nothing else waits for (unless a worker runs it); children without
    /// open parents are promoted to `ready` when `auto_promote`. Returns
    /// child key → task id.
    ///
    /// # Errors
    ///
    /// `kanban.invalid_task` (nothing is created), `kanban.task_not_found`.
    pub fn decompose(
        &mut self,
        actor: &Actor,
        id: &str,
        graph: &Graph,
        auto_promote: bool,
        profiles: &dyn Profiles,
        now: i64,
    ) -> Result<BTreeMap<String, String>, Error> {
        graph.validate(profiles)?;
        let order = graph.order().unwrap_or_default();
        let label = actor.label();
        self.write(|tx| {
            let parent = load(tx, id)?;
            let ids = add_children(tx, actor, &order, profiles, now)?;
            if auto_promote {
                promote_ready(tx, &label, ids.values(), now)?;
            }
            let waits = !matches!(
                parent.status,
                Status::Running | Status::Review | Status::Done | Status::Archived
            );
            if waits {
                wait_for_children(tx, actor, (id, parent.status), graph, &ids)?;
            }
            event(
                tx,
                Some(id),
                actor.run.as_deref(),
                &label,
                "decomposed",
                &json!({"children": ids}),
                now,
            )?;
            Ok(ids)
        })
    }
}

/// Inserts the children of a decomposition in dependency order; returns
/// key → task id.
fn add_children(
    tx: &Transaction<'_>,
    actor: &Actor,
    order: &[&Child],
    profiles: &dyn Profiles,
    now: i64,
) -> Result<BTreeMap<String, String>, Error> {
    let mut ids: BTreeMap<String, String> = BTreeMap::new();
    for child in order {
        let mut new = NewTask::titled(&child.title);
        new.body.clone_from(&child.body);
        new.assignee.clone_from(&child.assignee);
        new.parents = child.depends_on.iter().map(|d| ids[d].clone()).collect();
        new.initial_status = Some(Status::Todo);
        let (task, _) = add(tx, actor, &new, now)?;
        let lane = match &child.assignee {
            Some(a) => create::assignee_lane(profiles, a)?,
            None => false,
        };
        tx.execute(
            "UPDATE tasks SET lane = ?2 WHERE id = ?1",
            params![task, lane],
        )
        .map_err(sql)?;
        ids.insert(child.key.clone(), task);
    }
    Ok(ids)
}

/// Promotes the tasks among `ids` that wait for no open parent.
fn promote_ready<'a>(
    tx: &Transaction<'_>,
    label: &str,
    ids: impl Iterator<Item = &'a String>,
    now: i64,
) -> Result<(), Error> {
    for task in ids {
        if open_parents(tx, task)?.is_empty() {
            tx.execute("UPDATE tasks SET status = 'ready' WHERE id = ?1", [task])
                .map_err(sql)?;
            event(tx, Some(task), None, label, "promoted", &json!({}), now)?;
        }
    }
    Ok(())
}

/// Makes task `id` wait for the children nothing else waits for (a ready
/// task goes back to `todo`).
fn wait_for_children(
    tx: &Transaction<'_>,
    actor: &Actor,
    (id, status): (&str, Status),
    graph: &Graph,
    ids: &BTreeMap<String, String>,
) -> Result<(), Error> {
    let depended: BTreeSet<&str> = graph
        .tasks
        .iter()
        .flat_map(|c| c.depends_on.iter().map(String::as_str))
        .collect();
    for child in graph
        .tasks
        .iter()
        .filter(|c| !depended.contains(c.key.as_str()))
    {
        tx.execute(
            "INSERT OR IGNORE INTO links(parent, child) VALUES (?1, ?2)",
            params![ids[&child.key], id],
        )
        .map_err(sql)?;
    }
    if status == Status::Ready {
        workflow::check(Verb::Demote, actor.kind, Status::Ready, Status::Todo)?;
        tx.execute("UPDATE tasks SET status = 'todo' WHERE id = ?1", [id])
            .map_err(sql)?;
    }
    Ok(())
}
