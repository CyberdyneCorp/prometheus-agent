//! Task validation before any child is built (spec: subagent-delegation,
//! Task validation): a call with one bad task is refused whole, naming
//! the task index and the rule it breaks.

use std::sync::LazyLock;

use pmt_core::error::codes;
use regex::Regex;

use crate::Refusal;
use crate::contract::Task;
use crate::schema_check;

/// Longest goal, in characters.
pub const MAX_GOAL_CHARS: usize = 16_000;
/// Longest context, in characters.
pub const MAX_CONTEXT_CHARS: usize = 32_000;
/// Most images per task.
pub const MAX_IMAGES: usize = 8;

/// `task 3`, `Task-12`.
static TASK_N: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^task[\s_-]*\d+[.!]?$").expect("task pattern"));

/// An unexpanded template marker: two or more plain words inside angle
/// or curly brackets (`<your goal here>`, `{{task description}}`).
/// Code-shaped brackets (`Vec<T>`, `{"key": 1}`, `Map<K, V>`) have one
/// word, quotes or punctuation inside, so they never match.
static MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[<{]{1,2}\s*[A-Za-z][A-Za-z_-]*(?:\s+[A-Za-z][A-Za-z_-]*)+\s*[>}]{1,2}")
        .expect("marker pattern")
});

/// Checks every task of a call against the rules; `known_agents` are the
/// session's frozen agent definitions.
///
/// # Errors
///
/// The first broken rule: `delegation.invalid_task`, or
/// `delegation.unknown_agent` for an undefined agent.
pub fn tasks(tasks: &[Task], known_agents: &[String]) -> Result<(), Refusal> {
    tasks.iter().try_for_each(|task| one(task, known_agents))
}

fn one(task: &Task, known_agents: &[String]) -> Result<(), Refusal> {
    let i = task.index;
    let invalid = |message: String| Err(Refusal::new(&codes::DELEGATION_INVALID_TASK, message));
    let goal = task.goal.trim();
    if goal.is_empty() {
        return invalid(format!("Task {i} has a blank goal."));
    }
    let chars = task.goal.chars().count();
    if chars > MAX_GOAL_CHARS {
        return invalid(format!(
            "Task {i} goal has {chars} characters; the limit is {MAX_GOAL_CHARS}."
        ));
    }
    if let Some(context) = &task.context {
        let chars = context.chars().count();
        if chars > MAX_CONTEXT_CHARS {
            return invalid(format!(
                "Task {i} context has {chars} characters; the limit is {MAX_CONTEXT_CHARS}."
            ));
        }
    }
    if let Some(why) = placeholder(goal) {
        return invalid(format!(
            "Task {i} goal is a placeholder ({why}); describe the actual work."
        ));
    }
    if task.images.len() > MAX_IMAGES {
        return invalid(format!(
            "Task {i} has {} images; the per-task limit is {MAX_IMAGES}.",
            task.images.len()
        ));
    }
    if let Some(schema) = &task.output_schema
        && let Err(why) = schema_check::check(schema)
    {
        return invalid(format!(
            "Task {i} output_schema is not a valid JSON Schema (draft 2020-12): {why}."
        ));
    }
    if let Some(agent) = &task.agent
        && !known_agents.iter().any(|a| a == agent)
    {
        return Err(Refusal::new(
            &codes::DELEGATION_UNKNOWN_AGENT,
            format!("Task {i} names unknown agent '{agent}'."),
        ));
    }
    Ok(())
}

/// Why `goal` is a placeholder, if it is one.
fn placeholder(goal: &str) -> Option<&'static str> {
    let bare = goal.trim_end_matches(['.', '!', ':']).trim();
    if bare.eq_ignore_ascii_case("todo") {
        return Some("'todo'");
    }
    if TASK_N.is_match(goal) {
        return Some("'task N'");
    }
    MARKER
        .is_match(goal)
        .then_some("an unexpanded template marker")
}

/// Checks each task's router parameters against the router's schemas
/// (`params`: name -> JSON Schema).
///
/// # Errors
///
/// `delegation.invalid_task` naming the task, the parameter and why.
pub fn router_params(
    tasks: &[Task],
    params: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), Refusal> {
    for task in tasks {
        for (name, value) in &task.router_params {
            let Some(schema) = params.get(name) else {
                continue;
            };
            let errors = schema_check::validate(schema, value);
            if let Some(first) = errors.first() {
                return Err(Refusal::new(
                    &codes::DELEGATION_INVALID_TASK,
                    format!("Task {} parameter '{name}' is invalid: {first}", task.index),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, json};

    use super::*;

    fn task(goal: &str) -> Task {
        Task {
            index: 0,
            goal: goal.to_owned(),
            context: None,
            agent: None,
            toolsets: None,
            model: None,
            output_schema: None,
            images: Vec::new(),
            group: None,
            weight: None,
            router_params: Map::new(),
        }
    }

    fn message(task: &Task) -> String {
        tasks(std::slice::from_ref(task), &[]).unwrap_err().message
    }

    /// Scenario: Ninth image rejected.
    #[test]
    fn a_ninth_image_is_refused() {
        let mut t = task("Describe these screenshots");
        t.images = (0..9).map(|n| format!("shot-{n}.png")).collect();
        assert_eq!(message(&t), "Task 0 has 9 images; the per-task limit is 8.");
    }

    /// Scenario: Code-shaped brackets accepted.
    #[test]
    fn code_shaped_brackets_are_not_template_markers() {
        for goal in [
            "Implement Display for Vec<T> wrappers",
            "Parse {\"key\": 1} payloads",
            "Use HashMap<K, V> for the cache",
        ] {
            tasks(&[task(goal)], &[]).unwrap();
        }
    }

    #[test]
    fn placeholders_blank_and_oversized_goals_are_refused() {
        assert_eq!(message(&task("  ")), "Task 0 has a blank goal.");
        assert!(message(&task("TODO")).contains("placeholder"));
        assert!(message(&task("task 3")).contains("placeholder"));
        assert!(message(&task("Do <your goal here>")).contains("template marker"));
        assert!(message(&task("{{task description}}")).contains("template marker"));
        assert!(message(&task(&"x".repeat(16_001))).contains("limit is 16000"));
        let mut t = task("ok");
        t.context = Some("c".repeat(32_001));
        assert!(message(&t).contains("limit is 32000"));
    }

    #[test]
    fn bad_schemas_and_unknown_agents_are_refused() {
        let mut t = task("Summarise");
        t.output_schema = Some(json!({"type": "objekt"}));
        assert!(message(&t).contains("output_schema is not a valid JSON Schema"));
        let mut t = task("Review");
        t.agent = Some("reviewer".to_owned());
        let err = tasks(&[t.clone()], &[]).unwrap_err();
        assert_eq!(err.code.code, "delegation.unknown_agent");
        tasks(&[t], &["reviewer".to_owned()]).unwrap();
    }

    #[test]
    fn router_parameters_must_match_their_schema() {
        let mut task = crate::contract::parse(
            &serde_json::json!({"tasks": [{"goal": "Fix the parser", "tier": 3}]}),
            8,
            &["tier".to_owned()],
        )
        .unwrap()
        .tasks;
        let params = serde_json::Map::from_iter([(
            "tier".to_owned(),
            serde_json::json!({"type": "string"}),
        )]);
        let err = router_params(&task, &params).unwrap_err();
        assert!(err.message.contains("'tier'"), "{}", err.message);
        task[0]
            .router_params
            .insert("tier".to_owned(), serde_json::json!("fast"));
        assert!(router_params(&task, &params).is_ok());
    }
}
