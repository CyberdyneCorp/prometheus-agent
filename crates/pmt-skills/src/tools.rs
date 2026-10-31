//! The `skills` toolset: `skills_list`, `skill_view`, `skill_manage`
//! (spec: skills, `skills_list` and `skill_view` tools; `skill_manage` tool
//! validation; Skill usage records).
//!
//! The handlers are process-wide registry entries; each call finds its
//! session's [`SkillsSession`] in the [`SkillSessions`] table. No
//! skill-loading tool takes `offset` or `limit`: a skill is loaded whole,
//! and long material belongs in supporting files.

use std::sync::Arc;

use pmt_store::{CreatedBy, SkillActivity};
use pmt_tools::{
    CallContext, Effect, RegisteredTool, ResourceClaims, ToolDescriptor, ToolFailure, ToolHandler,
    ToolResult, descriptor::HandlerFuture,
};
use serde_json::{Value, json};

use crate::catalog::Missing;
use crate::gates;
use crate::hints;
use crate::layout::SKILL_FILE;
use crate::load;
use crate::manage::{self, Request};
use crate::session::{SkillSessions, SkillsSession};
use crate::source::ReadError;

/// The toolset.
pub const TOOLSET: &str = "skills";

/// The three tools over `sessions`.
#[must_use]
pub fn tools(sessions: &Arc<SkillSessions>) -> Vec<RegisteredTool> {
    let handler = |kind| -> Arc<dyn ToolHandler> {
        Arc::new(Handler {
            kind,
            sessions: Arc::clone(sessions),
        })
    };
    vec![
        RegisteredTool::new(
            list_descriptor(),
            pmt_tools::Owner::Builtin,
            handler(Kind::List),
        ),
        RegisteredTool::new(
            view_descriptor(),
            pmt_tools::Owner::Builtin,
            handler(Kind::View),
        ),
        RegisteredTool::new(
            manage_descriptor(),
            pmt_tools::Owner::Builtin,
            handler(Kind::Manage),
        ),
    ]
}

fn descriptor(name: &str, description: &str, schema: Value, effect: Effect) -> ToolDescriptor {
    let mut d = ToolDescriptor::new(name, TOOLSET, description, schema);
    d.effect = effect;
    // One skills tree: calls of one batch apply in order.
    d.claims = Some(ResourceClaims::exclusive("skills"));
    d
}

fn list_descriptor() -> ToolDescriptor {
    descriptor(
        "skills_list",
        "List the skills available in this session: name, description, category and source. \
         Load one with skill_view.",
        json!({
            "type": "object",
            "properties": {"category": {"type": "string", "description": "Only this category."}},
            "additionalProperties": false
        }),
        Effect::None,
    )
}

fn view_descriptor() -> ToolDescriptor {
    descriptor(
        "skill_view",
        "Load a skill: its instructions and the list of its supporting files. Pass file_path \
         (for example references/api.md) to read one supporting file. name is the skill name, \
         its category/name path or <plugin>:<skill>.",
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "file_path": {"type": "string", "description": "A supporting file, relative to the skill."}
            },
            "required": ["name"],
            "additionalProperties": false
        }),
        Effect::None,
    )
}

fn manage_descriptor() -> ToolDescriptor {
    descriptor(
        "skill_manage",
        "Create and maintain your own skills (procedures worth reusing). Actions: create \
         (name, content = full SKILL.md with name and description frontmatter, optional \
         category), edit (name, content), patch (name, old_string, new_string, optional \
         file_path, replace_all), delete (name), write_file (name, file_path under \
         references/, templates/, scripts/, assets/ or evals/, file_content), remove_file \
         (name, file_path). Keep descriptions short; they are shown in every session's index.",
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "description": "create, edit, patch, delete, write_file or remove_file."},
                "name": {"type": "string"},
                "category": {"type": "string"},
                "content": {"type": "string"},
                "old_string": {"type": "string"},
                "new_string": {"type": "string"},
                "replace_all": {"type": "boolean"},
                "file_path": {"type": "string"},
                "file_content": {"type": "string"}
            },
            "required": ["action", "name"],
            "additionalProperties": false
        }),
        Effect::Workspace,
    )
}

#[derive(Clone, Copy)]
enum Kind {
    List,
    View,
    Manage,
}

struct Handler {
    kind: Kind,
    sessions: Arc<SkillSessions>,
}

impl ToolHandler for Handler {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let session = self.sessions.of(context)?;
            match self.kind {
                Kind::List => Ok(list(&session, &arguments)),
                Kind::View => Ok(view(&session, &arguments).await),
                Kind::Manage => manage(session, arguments).await,
            }
        })
    }
}

fn json_result(value: &Value) -> ToolResult {
    ToolResult::ok(value.to_string())
}

/// `skills_list`.
#[must_use]
pub fn list(session: &SkillsSession, args: &Value) -> ToolResult {
    let category = args.get("category").and_then(Value::as_str);
    let (catalog, config) = session.catalog();
    let mut visible = session.visible(&catalog, &config);
    visible.sort_by(|a, b| (&a.category, &a.name).cmp(&(&b.category, &b.name)));
    let mut categories: Vec<&str> = visible.iter().map(|s| s.category.as_str()).collect();
    categories.dedup();
    let rows: Vec<Value> = visible
        .iter()
        .filter(|s| category.is_none_or(|c| s.category == c))
        .map(|s| {
            json!({
                "name": s.name,
                "description": s.description,
                "category": s.category,
                "source": s.tier.as_str(),
            })
        })
        .collect();
    json_result(&json!({"skills": rows, "categories": categories, "count": rows.len()}))
}

/// `skill_view`.
pub async fn view(session: &SkillsSession, args: &Value) -> ToolResult {
    let name = args.get("name").and_then(Value::as_str).unwrap_or_default();
    let file = args
        .get("file_path")
        .and_then(Value::as_str)
        .filter(|f| !f.is_empty());
    let (catalog, config) = session.catalog();
    let skill = match catalog.lookup(name) {
        Ok(skill) => skill,
        Err(Missing::Ambiguous(paths)) => {
            return ToolResult::error(Missing::ambiguous_message(name, &paths));
        }
        Err(Missing::NotFound) => {
            let scope = session.scope();
            return ToolResult::error(hints::not_found(scope.home(), scope.name().as_str(), name));
        }
    };
    if let Some(hidden) = gates::hidden(&config.skills, &session.setup().view, &skill) {
        return ToolResult::error(hidden.message(&skill.name));
    }
    if let Some(file) = file.filter(|f| *f != SKILL_FILE) {
        return view_file(session, &skill.name, &skill.dir, file);
    }
    let loaded = load::load(session, &skill, &config).await;
    record(session, &skill.name, SkillActivity::Viewed);
    if let Some(review) = session.review() {
        review.mark_viewed(&skill.name, SKILL_FILE);
    }
    json_result(&json!({
        "name": skill.name,
        "description": skill.description,
        "category": skill.category,
        "source": skill.tier.as_str(),
        "content": loaded.body,
        "linked_files": loaded.linked_files,
    }))
}

fn view_file(
    session: &SkillsSession,
    name: &str,
    dir: &crate::source::SkillDir,
    file: &str,
) -> ToolResult {
    let bytes = match dir.read(file) {
        Ok(bytes) => bytes,
        Err(ReadError::Outside) => return ToolResult::error(crate::source::OUTSIDE),
        Err(e) => return ToolResult::error(format!("{file}: {e}")),
    };
    let Ok(text) = String::from_utf8(bytes) else {
        return ToolResult::error(format!("{file} is a binary file"));
    };
    if let Some(review) = session.review() {
        review.mark_viewed(name, file);
    }
    json_result(&json!({"name": name, "file_path": file, "content": text}))
}

fn record(session: &SkillsSession, name: &str, activity: SkillActivity) {
    if let Err(err) = session
        .store()
        .record_skill_activity(name, activity, CreatedBy::User)
    {
        tracing::warn!(skill = %name, error = %err, "skill usage not recorded");
    }
}

async fn manage(session: Arc<SkillsSession>, args: Value) -> Result<ToolResult, ToolFailure> {
    let request = match Request::parse(&args) {
        Ok(request) => request,
        Err(why) => return Ok(ToolResult::error(why)),
    };
    let outcome = tokio::task::spawn_blocking(move || manage::run(&session, &request, false))
        .await
        .map_err(|e| ToolFailure::new("internal", format!("skill_manage: {e}")))?;
    Ok(match outcome {
        Ok(outcome) => json_result(&serde_json::to_value(&outcome).unwrap_or(Value::Null)),
        Err(why) => ToolResult::error(why),
    })
}
