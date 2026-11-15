//! `[[kanban_lanes]]` (spec: kanban, Worker lanes; plugin-system, Plugin
//! kinds and manifest): external worker lanes a plugin offers the kanban
//! dispatcher. Each has a `name` (an assignee, never a profile's name), a
//! `command` run under the local sandbox in the task's workspace with the
//! run ticket on stdin, and an optional `description`.

use pmt_core::error::Error;
use toml::{Table, Value};

use super::invalid;

/// One declared lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lane {
    /// The assignee name (`^[a-z0-9][a-z0-9_-]{0,63}$`).
    pub name: String,
    /// The program and its arguments, split without a shell; a relative
    /// program resolves in the plugin directory.
    pub command: Vec<String>,
    /// One line for `kanban assignees`.
    pub description: String,
}

const FIELDS: &[&str] = &["name", "command", "description"];

/// Parses the `[[kanban_lanes]]` tables.
///
/// # Errors
///
/// `plugins.manifest` naming the field.
pub fn parse(tables: &[Table]) -> Result<Vec<Lane>, Error> {
    let mut lanes: Vec<Lane> = Vec::new();
    for table in tables {
        if let Some(key) = table.keys().find(|k| !FIELDS.contains(&k.as_str())) {
            return Err(invalid(format!("unknown kanban_lanes field '{key}'")));
        }
        let name = match table.get("name") {
            Some(Value::String(name)) => name.clone(),
            _ => return Err(invalid("kanban_lanes.name must be a string".to_owned())),
        };
        super::check_id(&name)
            .map_err(|_| invalid(format!("invalid kanban_lanes.name '{name}'")))?;
        let command: Vec<String> = match table.get("command") {
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| v.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
                .unwrap_or_default(),
            Some(Value::String(line)) => line.split_whitespace().map(str::to_owned).collect(),
            _ => Vec::new(),
        };
        if command.is_empty() {
            return Err(invalid(format!(
                "kanban_lanes.command of lane '{name}' must name a program"
            )));
        }
        if lanes.iter().any(|l| l.name == name) {
            return Err(invalid(format!("kanban lane '{name}' is declared twice")));
        }
        let description = table
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        lanes.push(Lane {
            name,
            command,
            description,
        });
    }
    Ok(lanes)
}
