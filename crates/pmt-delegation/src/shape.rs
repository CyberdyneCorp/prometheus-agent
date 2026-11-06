//! The session's form of `delegate_task` (spec: subagent-delegation,
//! Gateway delegation mode and one-shot joining; Independent completions).
//!
//! Two per-session facts change what the model is offered: a session that
//! joins its children inside the call (a messaging session under
//! `delegation.gateway_mode`) is not offered the live-control actions, and
//! a session with `delegation.independent_completions` is offered the
//! per-task `group` field. Both are fixed when the session is created:
//! once its tool schemas are frozen, the stored `delegate_task` schema is
//! what the session's shape is read back from, so a configuration change
//! applies to new sessions only and the schema and the behaviour never
//! disagree.

use serde_json::Value;

/// The `delegate_task` fields only live control uses.
pub const CONTROL_FIELDS: &[&str] = &["action", "subagent_id", "message"];

/// The tool's name.
const TOOL: &str = "delegate_task";

/// What the group field says when it is offered.
const GROUP_DESCRIPTION: &str = "Deliver this task's result together with the other tasks of \
     the same group once they all finish; a task without a group is delivered on its own as \
     soon as it finishes.";

/// A session's form of `delegate_task`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Shape {
    /// Children are joined inside the call (no background dispatch, no
    /// live-control actions).
    pub joining: bool,
    /// Ungrouped tasks and groups complete independently.
    pub groups: bool,
}

impl Shape {
    /// The shape a frozen tool array (JSON array of `{name, parameters}`)
    /// gives `delegate_task`; `None` when it has none.
    #[must_use]
    pub fn of_stored(tools: &Value) -> Option<Self> {
        let schema = tools
            .as_array()?
            .iter()
            .find(|t| t.get("name").and_then(Value::as_str) == Some(TOOL))?
            .get("parameters")?;
        let properties = schema.get("properties")?;
        Some(Self {
            joining: properties.get("action").is_none(),
            groups: properties
                .pointer("/tasks/items/properties/group")
                .is_some(),
        })
    }

    /// Tailors `delegate_task`'s full `schema` to this shape.
    pub fn tailor(self, schema: &mut Value) {
        let Some(properties) = schema.get_mut("properties").and_then(Value::as_object_mut) else {
            return;
        };
        if self.joining {
            for field in CONTROL_FIELDS {
                properties.remove(*field);
            }
        }
        let Some(task) = properties
            .get_mut("tasks")
            .and_then(|t| t.pointer_mut("/items/properties"))
            .and_then(Value::as_object_mut)
        else {
            return;
        };
        if self.groups {
            task.insert(
                "group".to_owned(),
                serde_json::json!({"type": "string", "description": GROUP_DESCRIPTION}),
            );
        } else {
            task.remove("group");
        }
    }

    /// Whether `name`'s schema is shaped by this (only `delegate_task`).
    #[must_use]
    pub fn applies_to(name: &str) -> bool {
        name == TOOL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> Value {
        pmt_tools::agent_level_delegation::delegate_task().parameters
    }

    #[test]
    fn a_joining_session_is_not_offered_live_control() {
        let mut schema = full();
        Shape {
            joining: true,
            groups: false,
        }
        .tailor(&mut schema);
        for field in CONTROL_FIELDS {
            assert!(schema["properties"].get(*field).is_none(), "{field}");
        }
        assert!(
            schema
                .pointer("/properties/tasks/items/properties/group")
                .is_none()
        );
        assert!(
            schema
                .pointer("/properties/tasks/items/properties/goal")
                .is_some()
        );
    }

    #[test]
    fn the_shape_is_read_back_from_the_frozen_schema() {
        for shape in [
            Shape::default(),
            Shape {
                joining: true,
                groups: true,
            },
        ] {
            let mut schema = full();
            shape.tailor(&mut schema);
            let tools = serde_json::json!([
                {"name": "read_file", "parameters": {}},
                {"name": "delegate_task", "parameters": schema}
            ]);
            assert_eq!(Shape::of_stored(&tools), Some(shape));
        }
        assert_eq!(Shape::of_stored(&serde_json::json!([])), None);
    }
}
