//! The `delegate_task` input contract (spec: subagent-delegation,
//! `delegate_task` tool contract; Fan-out budget split and roll-up).
//!
//! The model's arguments are parsed here into a typed [`Call`]. Task items
//! are checked field by field instead of by the registry schema, so an
//! unknown field is refused with the spec's message naming the task, and
//! a delegation router's own typed task parameters can join the set of
//! accepted fields for the session.

use pmt_core::error::{ErrorCode, codes};
use serde_json::{Map, Value};

use crate::Refusal;

/// The built-in task fields.
pub const TASK_FIELDS: &[&str] = &[
    "goal",
    "context",
    "agent",
    "toolsets",
    "model",
    "output_schema",
    "images",
    "group",
    "weight",
];

/// What a call asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Start children (the default).
    Spawn,
    /// The caller's live and recent children.
    List,
    /// One child's state.
    Status,
    /// Queue a message for a child's next iteration.
    Steer,
    /// Ask a child to stop.
    Stop,
    /// Wait for a child (or a background delegation) to finish.
    Wait,
}

impl Action {
    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "spawn" => Self::Spawn,
            "list" => Self::List,
            "status" => Self::Status,
            "steer" => Self::Steer,
            "stop" => Self::Stop,
            "wait" => Self::Wait,
            _ => return None,
        })
    }

    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::List => "list",
            Self::Status => "status",
            Self::Steer => "steer",
            Self::Stop => "stop",
            Self::Wait => "wait",
        }
    }
}

/// How a call's budget is divided across its tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Split {
    /// Equal shares.
    #[default]
    Even,
    /// Shares proportional to each task's `weight` (default 1).
    Weighted,
}

/// A call-level budget (`budget`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct CallBudget {
    /// USD for the whole call.
    pub max_cost_usd: Option<f64>,
    /// Tokens for the whole call.
    pub max_tokens: Option<u64>,
    /// Model calls for the whole call.
    pub max_iterations: Option<u32>,
    /// How the budget is divided.
    pub split: Split,
}

/// One task.
#[derive(Debug, Clone, PartialEq)]
pub struct Task {
    /// Its index in the call.
    pub index: usize,
    /// What the child must achieve.
    pub goal: String,
    /// Background the child needs.
    pub context: Option<String>,
    /// A named agent definition.
    pub agent: Option<String>,
    /// Narrowing of the child's toolsets.
    pub toolsets: Option<Vec<String>>,
    /// The child's model (`provider/model`).
    pub model: Option<String>,
    /// A JSON Schema the child's answer must match.
    pub output_schema: Option<Value>,
    /// Image references (paths or URLs).
    pub images: Vec<String>,
    /// Delivery group (accepted; grouping is delivery-only).
    pub group: Option<String>,
    /// Weight under `budget.split = "weighted"`.
    pub weight: Option<f64>,
    /// Router-defined parameters.
    pub router_params: Map<String, Value>,
}

impl Task {
    /// The task as a router sees it: its fields and its router
    /// parameters, as one JSON object.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut object = self.router_params.clone();
        object.insert("index".to_owned(), self.index.into());
        object.insert("goal".to_owned(), self.goal.clone().into());
        let optional = [
            ("context", self.context.clone().map(Value::from)),
            ("agent", self.agent.clone().map(Value::from)),
            ("toolsets", self.toolsets.clone().map(Value::from)),
            ("model", self.model.clone().map(Value::from)),
            ("output_schema", self.output_schema.clone()),
            ("group", self.group.clone().map(Value::from)),
            ("weight", self.weight.map(Value::from)),
        ];
        for (key, value) in optional {
            if let Some(value) = value {
                object.insert(key.to_owned(), value);
            }
        }
        if !self.images.is_empty() {
            object.insert("images".to_owned(), self.images.clone().into());
        }
        Value::Object(object)
    }
}

/// A parsed call.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    /// What it asks for.
    pub action: Action,
    /// The child a control action targets.
    pub subagent_id: Option<String>,
    /// A steer's message.
    pub message: Option<String>,
    /// The call budget.
    pub budget: Option<CallBudget>,
    /// The tasks (spawn).
    pub tasks: Vec<Task>,
}

fn refuse(code: &'static ErrorCode, message: impl Into<String>) -> Refusal {
    Refusal::new(code, message)
}

fn invalid(message: impl Into<String>) -> Refusal {
    refuse(&codes::DELEGATION_INVALID_TASK, message)
}

/// Parses the model's arguments; `router_params` are the extra task
/// fields the session's router declares.
///
/// # Errors
///
/// `delegation.invalid_task` naming the offending field.
pub fn parse(
    arguments: &Value,
    max_tasks: usize,
    router_params: &[String],
) -> Result<Call, Refusal> {
    let Some(object) = arguments.as_object() else {
        return Err(invalid("delegate_task arguments must be an object."));
    };
    let action = match object.get("action") {
        None | Some(Value::Null) => Action::Spawn,
        Some(Value::String(text)) => Action::parse(text).ok_or_else(|| {
            invalid(format!(
                "Unknown action '{text}' (use spawn, list, status, steer, stop or wait)."
            ))
        })?,
        Some(_) => return Err(invalid("'action' must be a string.")),
    };
    check_join(object.get("join"))?;
    let tasks = match action {
        Action::Spawn => parse_tasks(object.get("tasks"), max_tasks, router_params)?,
        _ => Vec::new(),
    };
    Ok(Call {
        action,
        subagent_id: string(object, "subagent_id")?,
        message: string(object, "message")?,
        budget: object.get("budget").map(parse_budget).transpose()?,
        tasks,
    })
}

/// `join`: only `all` runs in this release (the other fan-in modes are
/// not offered yet, so they are refused rather than ignored).
fn check_join(join: Option<&Value>) -> Result<(), Refusal> {
    match join {
        None | Some(Value::Null) => Ok(()),
        Some(Value::String(mode)) if mode == "all" => Ok(()),
        Some(other) => Err(invalid(format!(
            "join {other} is not supported; children are always joined with \"all\"."
        ))),
    }
}

fn string(object: &Map<String, Value>, key: &str) -> Result<Option<String>, Refusal> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(invalid(format!("'{key}' must be a string."))),
    }
}

fn parse_tasks(
    tasks: Option<&Value>,
    max_tasks: usize,
    router_params: &[String],
) -> Result<Vec<Task>, Refusal> {
    let Some(Value::Array(items)) = tasks else {
        return Err(invalid(
            "A spawn needs 'tasks': an array of 1 or more tasks.",
        ));
    };
    if items.is_empty() || items.len() > max_tasks {
        return Err(invalid(format!(
            "A call carries 1 to {max_tasks} tasks (delegation.max_tasks_per_call); got {}.",
            items.len()
        )));
    }
    items
        .iter()
        .enumerate()
        .map(|(index, item)| parse_task(index, item, router_params))
        .collect()
}

fn parse_task(index: usize, item: &Value, router_params: &[String]) -> Result<Task, Refusal> {
    let Some(object) = item.as_object() else {
        return Err(invalid(format!("Task {index} must be an object.")));
    };
    let mut router = Map::new();
    for (key, value) in object {
        if TASK_FIELDS.contains(&key.as_str()) {
            continue;
        }
        if router_params.iter().any(|p| p == key) {
            router.insert(key.clone(), value.clone());
            continue;
        }
        return Err(invalid(format!("Task {index} has unknown field '{key}'.")));
    }
    let text = |key: &str| task_string(index, object, key);
    Ok(Task {
        index,
        goal: text("goal")?.unwrap_or_default(),
        context: text("context")?,
        agent: text("agent")?,
        toolsets: string_list(index, object, "toolsets")?,
        model: text("model")?,
        output_schema: object
            .get("output_schema")
            .filter(|v| !v.is_null())
            .cloned(),
        images: string_list(index, object, "images")?.unwrap_or_default(),
        group: text("group")?,
        weight: weight(index, object)?,
        router_params: router,
    })
}

fn task_string(
    index: usize,
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<String>, Refusal> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(invalid(format!(
            "Task {index} field '{key}' must be a string."
        ))),
    }
}

fn string_list(
    index: usize,
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<Vec<String>>, Refusal> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| v.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .map(Some)
            .ok_or_else(|| invalid(format!("Task {index} field '{key}' must list strings."))),
        Some(_) => Err(invalid(format!(
            "Task {index} field '{key}' must be an array."
        ))),
    }
}

fn weight(index: usize, object: &Map<String, Value>) -> Result<Option<f64>, Refusal> {
    match object.get("weight") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_f64()
            .filter(|w| w.is_finite() && *w > 0.0)
            .map(Some)
            .ok_or_else(|| invalid(format!("Task {index} weight must be a positive number."))),
    }
}

fn parse_budget(value: &Value) -> Result<CallBudget, Refusal> {
    let Some(object) = value.as_object() else {
        return Err(invalid("'budget' must be an object."));
    };
    for key in object.keys() {
        if !["max_cost_usd", "max_tokens", "max_iterations", "split"].contains(&key.as_str()) {
            return Err(invalid(format!("budget has unknown field '{key}'.")));
        }
    }
    let positive = |key: &str| -> Result<Option<f64>, Refusal> {
        match object.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_f64()
                .filter(|n| n.is_finite() && *n > 0.0)
                .map(Some)
                .ok_or_else(|| invalid(format!("budget.{key} must be a positive number."))),
        }
    };
    let split = match object.get("split").and_then(Value::as_str) {
        None | Some("even") => Split::Even,
        Some("weighted") => Split::Weighted,
        Some(other) => {
            return Err(invalid(format!(
                "budget.split '{other}' is not even or weighted."
            )));
        }
    };
    Ok(CallBudget {
        max_cost_usd: positive("max_cost_usd")?,
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // positive, finite
        max_tokens: positive("max_tokens")?.map(|n| n as u64),
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // positive, finite
        max_iterations: positive("max_iterations")?.map(|n| n.min(f64::from(u32::MAX)) as u32),
        split,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Scenario: Unknown task field rejected.
    #[test]
    fn an_unknown_task_field_is_refused_naming_it() {
        let err = parse(
            &json!({"tasks": [{"goal": "x", "acp_command": "run"}]}),
            8,
            &[],
        )
        .unwrap_err();
        assert_eq!(err.message, "Task 0 has unknown field 'acp_command'.");
        assert_eq!(err.code.code, "delegation.invalid_task");
    }

    #[test]
    fn spawn_is_the_default_action_and_router_params_are_accepted() {
        let call = parse(
            &json!({"tasks": [{"goal": "Fix the failing tests in crates/store", "tier": "fast"}]}),
            8,
            &["tier".to_owned()],
        )
        .unwrap();
        assert_eq!(call.action, Action::Spawn);
        assert_eq!(call.tasks.len(), 1);
        assert_eq!(call.tasks[0].router_params["tier"], "fast");
    }

    #[test]
    fn too_many_tasks_and_other_join_modes_are_refused() {
        let tasks: Vec<Value> = (0..3).map(|_| json!({"goal": "g"})).collect();
        assert!(parse(&json!({"tasks": tasks}), 2, &[]).is_err());
        assert!(
            parse(
                &json!({"tasks": [{"goal": "g"}], "join": "first_success"}),
                2,
                &[]
            )
            .is_err()
        );
        let control = parse(&json!({"action": "list"}), 2, &[]).unwrap();
        assert_eq!(control.action, Action::List);
    }

    #[test]
    fn budgets_parse_with_their_split() {
        let call = parse(
            &json!({"tasks": [{"goal": "g", "weight": 3}], "budget": {"max_cost_usd": 1.0, "split": "weighted"}}),
            8,
            &[],
        )
        .unwrap();
        let budget = call.budget.unwrap();
        assert_eq!(budget.max_cost_usd, Some(1.0));
        assert_eq!(budget.split, Split::Weighted);
        assert_eq!(call.tasks[0].weight, Some(3.0));
    }
}
