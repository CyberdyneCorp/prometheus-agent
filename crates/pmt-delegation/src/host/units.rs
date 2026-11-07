//! Delivery units and live transcripts of a dispatch (spec:
//! subagent-delegation, Independent completions; Agents monitor and live
//! transcripts).
//!
//! A unit is what one completion delivers. Without independent
//! completions a call is one unit under its own delegation id. With them,
//! each ungrouped task is a unit and each `group` is one, with ids
//! `<delegation_id>-1`, `-2`, … in the order their first task appears.
//! Units change delivery only: every child of the call is admitted and run
//! as before.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Value, json};

use super::Delegator;
use super::spawn::Child;
use crate::contract::Call;
use crate::transcript::{self, TaskEntry};

/// One completion's tasks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Unit {
    /// Its delegation id (the call's, or `<call>-<n>`).
    pub(super) id: String,
    /// Its group, if any.
    pub(super) group: Option<String>,
    /// Positions of its tasks in the call.
    pub(super) tasks: Vec<usize>,
}

impl Unit {
    /// The whole call `delegation_id` of `tasks` tasks as one unit.
    pub(super) fn whole(delegation_id: &str, tasks: usize) -> Self {
        Self {
            id: delegation_id.to_owned(),
            group: None,
            tasks: (0..tasks).collect(),
        }
    }

    /// The units of `call`.
    pub(super) fn of(call: &Call, delegation_id: &str, independent: bool) -> Vec<Self> {
        if !independent {
            return vec![Self::whole(delegation_id, call.tasks.len())];
        }
        let mut units: Vec<Self> = Vec::new();
        for (position, task) in call.tasks.iter().enumerate() {
            let grouped = task
                .group
                .as_ref()
                .and_then(|g| units.iter_mut().find(|u| u.group.as_ref() == Some(g)));
            match grouped {
                Some(unit) => unit.tasks.push(position),
                None => units.push(Self {
                    id: format!("{delegation_id}-{}", units.len() + 1),
                    group: task.group.clone(),
                    tasks: vec![position],
                }),
            }
        }
        units
    }

    /// The id of the unit holding task `position`.
    pub(super) fn id_of(units: &[Self], position: usize) -> String {
        units
            .iter()
            .find(|u| u.tasks.contains(&position))
            .map_or_else(String::new, |u| u.id.clone())
    }

    /// `children` (in task order) by unit id.
    pub(super) fn split(units: &[Self], children: Vec<Child>) -> HashMap<String, Vec<Child>> {
        let mut out: HashMap<String, Vec<Child>> = HashMap::new();
        for (position, child) in children.into_iter().enumerate() {
            out.entry(Self::id_of(units, position))
                .or_default()
                .push(child);
        }
        out
    }

    /// The unit as the dispatch handle lists it.
    pub(super) fn view(&self) -> Value {
        json!({
            "delegation_id": self.id,
            "group": self.group,
            "task_indexes": self.tasks,
        })
    }
}

impl Delegator {
    /// Creates the live transcripts of `children` and hands each its
    /// own; their paths (empty when they could not be written).
    pub(super) fn transcripts(&self, delegation_id: &str, children: &mut [Child]) -> Vec<String> {
        let tasks: Vec<TaskEntry> = children
            .iter()
            .map(|c| TaskEntry {
                task_index: c.entry.task_index,
                subagent_id: c.entry.subagent_id.clone(),
                goal: c.entry.goal.clone(),
            })
            .collect();
        let opened = transcript::open(
            self.ctx.scope.dir(),
            delegation_id,
            &self.ctx.session.to_string(),
            &tasks,
        );
        match opened {
            Ok(logs) => children
                .iter_mut()
                .zip(logs)
                .map(|(child, log)| {
                    let path = log.path().to_string_lossy().into_owned();
                    child.entry.set_transcript(log.path().to_path_buf());
                    child.transcript = Some(Arc::new(log));
                    path
                })
                .collect(),
            Err(err) => {
                tracing::warn!(error = %err, delegation_id, "live transcripts not written");
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ungrouped_tasks_and_each_group_are_units_in_order() {
        let call = crate::contract::parse(
            &json!({"tasks": [
                {"goal": "a"},
                {"goal": "b", "group": "bench"},
                {"goal": "c"},
                {"goal": "d", "group": "bench"}
            ]}),
            8,
            &[],
        )
        .unwrap();
        let units = Unit::of(&call, "dg-x", true);
        let shape: Vec<(String, Option<String>, Vec<usize>)> = units
            .iter()
            .map(|u| (u.id.clone(), u.group.clone(), u.tasks.clone()))
            .collect();
        assert_eq!(
            shape,
            [
                ("dg-x-1".to_owned(), None, vec![0]),
                ("dg-x-2".to_owned(), Some("bench".to_owned()), vec![1, 3]),
                ("dg-x-3".to_owned(), None, vec![2]),
            ]
        );
        assert_eq!(Unit::of(&call, "dg-x", false), [Unit::whole("dg-x", 4)]);
    }
}
