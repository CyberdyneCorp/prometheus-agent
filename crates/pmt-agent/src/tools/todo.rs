//! The `todo` tool (spec: agent-turn-loop, todo tool contract).
//!
//! A session's prioritized task list: list position is priority, every
//! write bumps a monotonic revision, every call returns the whole list, and
//! after compaction the turn loop re-injects only the open items
//! ([`TodoList::reinjection`]). The list is session state the store keeps
//! with the session.

use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_store::{SessionTodos, Store};
use pmt_tools::agent_level::{TODO_MAX_CONTENT, TODO_MAX_ITEMS};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// An item's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    /// Not started.
    Pending,
    /// Being worked on.
    InProgress,
    /// Done.
    Completed,
    /// Dropped.
    Cancelled,
}

impl TodoStatus {
    fn open(self) -> bool {
        matches!(self, Self::Pending | Self::InProgress)
    }
}

/// One item of the list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    /// Stable id chosen by the model.
    pub id: String,
    /// What to do.
    pub content: String,
    /// Its state.
    pub status: TodoStatus,
    /// The parent item, for subtasks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

/// An item as the model sends it: every field but `id` optional (a merge
/// changes only the fields given).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Patch {
    id: String,
    content: Option<String>,
    status: Option<TodoStatus>,
    parent: Option<String>,
}

/// The arguments of one call.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    todos: Option<Vec<Patch>>,
    #[serde(default)]
    merge: bool,
}

/// A session's list and its revision.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TodoList {
    revision: u64,
    items: Vec<TodoItem>,
}

impl TodoList {
    /// The list `session` stored (empty when it never wrote one).
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn load(store: &Store, session: SessionId) -> Result<Self, Error> {
        let Some(stored) = store.todos(session)? else {
            return Ok(Self::default());
        };
        let items = serde_json::from_value(stored.items).unwrap_or_default();
        Ok(Self {
            revision: stored.revision,
            items,
        })
    }

    /// Stores the list with `session`.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn save(&self, store: &Store, session: SessionId) -> Result<(), Error> {
        let items = serde_json::to_value(&self.items).unwrap_or_else(|_| json!([]));
        store.save_todos(
            session,
            &SessionTodos {
                revision: self.revision,
                items,
            },
        )
    }

    /// The write counter.
    #[must_use]
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// The items in priority order.
    #[must_use]
    pub fn items(&self) -> &[TodoItem] {
        &self.items
    }

    /// Runs one `todo` call. `Ok` carries whether the list changed (the
    /// caller then persists it) and the reply, the whole list.
    ///
    /// # Errors
    ///
    /// The model-facing message of an invalid call; the list is unchanged.
    pub fn call(&mut self, arguments: Value) -> Result<(bool, Value), String> {
        let args: Args =
            serde_json::from_value(arguments).map_err(|err| format!("invalid todo call: {err}"))?;
        let Some(patches) = args.todos else {
            return Ok((false, self.reply()));
        };
        let items = if args.merge {
            merged(&self.items, patches)?
        } else {
            replaced(patches)?
        };
        check(&items)?;
        self.items = items;
        self.revision += 1;
        Ok((true, self.reply()))
    }

    /// The whole list as returned by every call.
    #[must_use]
    pub fn reply(&self) -> Value {
        json!({"revision": self.revision, "todos": self.items})
    }

    /// The text re-injected after compaction: pending and in-progress items
    /// plus the parents of open subtasks, in list order; `None` when
    /// nothing is open.
    #[must_use]
    pub fn reinjection(&self) -> Option<String> {
        let open_parents: Vec<&str> = self
            .items
            .iter()
            .filter(|item| item.status.open())
            .filter_map(|item| item.parent.as_deref())
            .collect();
        let shown: Vec<String> = self
            .items
            .iter()
            .filter(|item| item.status.open() || open_parents.contains(&item.id.as_str()))
            .map(|item| {
                let status = serde_json::to_value(item.status).unwrap_or_default();
                let indent = if item.parent.is_some() { "  " } else { "" };
                format!(
                    "{indent}- [{}] {} ({})",
                    status.as_str().unwrap_or_default(),
                    item.content,
                    item.id
                )
            })
            .collect();
        if shown.is_empty() {
            return None;
        }
        Some(format!(
            "Your todo list (revision {}; completed and cancelled items hidden):\n{}",
            self.revision,
            shown.join("\n")
        ))
    }
}

/// `patches` as a new list (`merge = false`): each item needs its content.
fn replaced(patches: Vec<Patch>) -> Result<Vec<TodoItem>, String> {
    let mut items: Vec<TodoItem> = Vec::with_capacity(patches.len());
    for patch in patches {
        if items.iter().any(|item| item.id == patch.id) {
            return Err(format!("duplicate todo id '{}'", patch.id));
        }
        items.push(new_item(patch)?);
    }
    Ok(items)
}

/// `current` with each patch applied by id; unknown ids are appended.
fn merged(current: &[TodoItem], patches: Vec<Patch>) -> Result<Vec<TodoItem>, String> {
    let mut items = current.to_vec();
    for patch in patches {
        match items.iter_mut().find(|item| item.id == patch.id) {
            Some(item) => {
                if let Some(content) = patch.content {
                    item.content = content;
                }
                if let Some(status) = patch.status {
                    item.status = status;
                }
                if patch.parent.is_some() {
                    item.parent = patch.parent;
                }
            }
            None => items.push(new_item(patch)?),
        }
    }
    Ok(items)
}

fn new_item(patch: Patch) -> Result<TodoItem, String> {
    let content = patch
        .content
        .ok_or_else(|| format!("todo '{}' needs content", patch.id))?;
    Ok(TodoItem {
        id: patch.id,
        content,
        status: patch.status.unwrap_or(TodoStatus::Pending),
        parent: patch.parent,
    })
}

fn check(items: &[TodoItem]) -> Result<(), String> {
    if items.len() > TODO_MAX_ITEMS {
        return Err(format!("todos supports at most {TODO_MAX_ITEMS} items."));
    }
    if let Some(item) = items
        .iter()
        .find(|item| item.content.chars().count() > TODO_MAX_CONTENT)
    {
        return Err(format!(
            "todo '{}' content exceeds {TODO_MAX_CONTENT} characters.",
            item.id
        ));
    }
    Ok(())
}
