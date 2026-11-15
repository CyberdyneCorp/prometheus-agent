//! Dependency links (spec: kanban, Dependency links): within one board,
//! no self-links, no cycles, and a running child only with its current
//! run id. Linking a `ready` child to an open parent demotes it.

use std::collections::{HashSet, VecDeque};

use pmt_core::error::{Error, codes};
use rusqlite::{Connection, params};
use serde_json::json;

use super::{Board, event, load, open_run, set_status};
use crate::db::sql;
use crate::model::{Actor, Status};
use crate::workflow::{self, Verb};

fn refused(message: impl Into<String>) -> Error {
    Error::new(&codes::KANBAN_LINK_REFUSED, message)
}

/// Whether `to` is reachable from `from` following parent → child links.
fn reaches(conn: &Connection, from: &str, to: &str) -> Result<bool, Error> {
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([from.to_owned()]);
    let mut stmt = conn
        .prepare("SELECT child FROM links WHERE parent = ?1")
        .map_err(sql)?;
    while let Some(node) = queue.pop_front() {
        if node == to {
            return Ok(true);
        }
        if !seen.insert(node.clone()) {
            continue;
        }
        let children = stmt
            .query_map([&node], |r| r.get::<_, String>(0))
            .map_err(sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql)?;
        queue.extend(children);
    }
    Ok(false)
}

impl Board {
    /// Makes `child` depend on `parent`. `child_run` must name the
    /// child's open run when the child is running.
    ///
    /// # Errors
    ///
    /// `kanban.link_refused`, `kanban.task_not_found`.
    pub fn link(
        &mut self,
        actor: &Actor,
        parent: &str,
        child: &str,
        child_run: Option<&str>,
        now: i64,
    ) -> Result<(), Error> {
        if parent == child {
            return Err(refused("a task cannot depend on itself"));
        }
        let label = actor.label();
        self.write(|tx| {
            let p = load(tx, parent)?;
            let c = load(tx, child)?;
            if reaches(tx, child, parent)? {
                return Err(refused(format!(
                    "linking {parent} → {child} would create a cycle"
                )));
            }
            if c.status == Status::Running {
                let run = open_run(tx, child)?.map(|r| r.id);
                if run.is_none() || run.as_deref() != child_run {
                    return Err(refused(format!(
                        "task {child} is running: pass its current run id to link it"
                    )));
                }
            }
            tx.execute(
                "INSERT OR IGNORE INTO links(parent, child) VALUES (?1, ?2)",
                params![parent, child],
            )
            .map_err(sql)?;
            event(
                tx,
                Some(child),
                None,
                &label,
                "linked",
                &json!({"parent": parent}),
                now,
            )?;
            if c.status == Status::Ready && !p.status.settles_parent() {
                workflow::check(Verb::Demote, actor.kind, Status::Ready, Status::Todo)?;
                set_status(tx, child, Status::Todo, now)?;
                event(
                    tx,
                    Some(child),
                    None,
                    &label,
                    "dependency_wait",
                    &json!({"parents": [parent]}),
                    now,
                )?;
            }
            Ok(())
        })
    }

    /// Removes a link.
    ///
    /// # Errors
    ///
    /// `kanban.task_not_found`, `kanban.link_refused` when no such link.
    pub fn unlink(
        &mut self,
        actor: &Actor,
        parent: &str,
        child: &str,
        now: i64,
    ) -> Result<(), Error> {
        let label = actor.label();
        self.write(|tx| {
            load(tx, child)?;
            let removed = tx
                .execute(
                    "DELETE FROM links WHERE parent = ?1 AND child = ?2",
                    params![parent, child],
                )
                .map_err(sql)?;
            if removed == 0 {
                return Err(refused(format!("{child} does not depend on {parent}")));
            }
            event(
                tx,
                Some(child),
                None,
                &label,
                "unlinked",
                &json!({"parent": parent}),
                now,
            )?;
            Ok(())
        })
    }
}
