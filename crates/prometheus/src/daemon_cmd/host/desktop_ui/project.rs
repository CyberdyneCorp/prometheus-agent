//! The `project` tool (spec: projects, Project tool for GUI sessions;
//! desktop-app, Project tool): lists the profile's projects, creates one
//! and switches the calling session to one. A switch moves only this
//! session's working directory, from its next tool call (the prompt and
//! the tool array stay as they were, so the prompt cache survives), and
//! never the profile's active project.

use std::path::{Path, PathBuf};

use pmt_rpc::contracts::{SessionProject, SessionProjectChanged};
use pmt_store::{NewProject, Project};
use pmt_tools::ToolResult;
use serde_json::{Value, json};

use super::{UiPort, answer, fail};

/// Runs one `project` call.
pub(super) fn call(port: &UiPort, args: &Value) -> ToolResult {
    let text = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    let outcome = match text("action").unwrap_or_default() {
        "list" => list(port),
        "create" => match text("name") {
            Some(name) => create(port, name, text("path")).and_then(|p| switch_to(port, &p)),
            None => Err("project create needs a name".to_owned()),
        },
        "switch" => match text("project") {
            Some(reference) => port
                .store
                .project(reference)
                .map_err(|e| e.message().to_owned())
                .and_then(|p| switch_to(port, &p)),
            None => Err("project switch needs the project's id or slug".to_owned()),
        },
        other => Err(format!(
            "unknown action '{other}'; use list, create or switch"
        )),
    };
    match outcome {
        Ok(value) => answer(&value),
        Err(refused) => fail(refused),
    }
}

fn row(project: &Project, current: bool) -> Value {
    json!({
        "id": project.id.to_string(),
        "slug": project.slug,
        "name": project.name,
        "path": project.primary_path(),
        "current": current,
    })
}

fn list(port: &UiPort) -> Result<Value, String> {
    let session = port
        .store
        .session(port.session)
        .map_err(|e| e.message().to_owned())?;
    let projects = port
        .store
        .projects(false)
        .map_err(|e| e.message().to_owned())?;
    let rows: Vec<Value> = projects
        .iter()
        .map(|p| row(p, session.project_id == Some(p.id)))
        .collect();
    Ok(json!({"success": true, "projects": rows}))
}

/// The project owning `path` (the session's directory when absent), else
/// a new one named `name` with that folder.
fn create(port: &UiPort, name: &str, path: Option<&str>) -> Result<Project, String> {
    let folder = if let Some(path) = path {
        PathBuf::from(path)
    } else {
        let session = port
            .store
            .session(port.session)
            .map_err(|e| e.message().to_owned())?;
        PathBuf::from(session.cwd.unwrap_or_default())
    };
    let folder = folder
        .canonicalize()
        .map_err(|e| format!("project folder {} is not usable: {e}", folder.display()))?;
    if let Some(owner) = owner_of(port, &folder)? {
        return Ok(owner);
    }
    let new = NewProject {
        name: name.to_owned(),
        folders: vec![folder],
        ..NewProject::default()
    };
    port.store
        .create_project(&new, false)
        .map_err(|e| e.message().to_owned())
}

fn owner_of(port: &UiPort, folder: &Path) -> Result<Option<Project>, String> {
    port.store
        .project_owning(folder)
        .map_err(|e| e.message().to_owned())
}

/// Records `project` on the session, moves its working directory there
/// from the next tool call and tells its clients.
fn switch_to(port: &UiPort, project: &Project) -> Result<Value, String> {
    let primary = project
        .primary_path()
        .ok_or_else(|| format!("project {} has no primary folder", project.slug))?
        .to_owned();
    let tools = port
        .tools()
        .filter(|t| t.can_move())
        .ok_or("this session's working directory cannot move")?;
    port.store
        .set_session_project(port.session, &project.id)
        .map_err(|e| e.message().to_owned())?;
    tools.move_before_next_call(PathBuf::from(&primary));
    if let Some(link) = port.link() {
        link.emit::<SessionProjectChanged>(&SessionProject {
            session_id: port.session.to_string(),
            project_id: project.id.to_string(),
            slug: project.slug.clone(),
            cwd: primary.clone(),
        });
    }
    Ok(json!({
        "success": true,
        "project": row(project, true),
        "note": format!("your working directory becomes {primary} from your next tool call"),
    }))
}
