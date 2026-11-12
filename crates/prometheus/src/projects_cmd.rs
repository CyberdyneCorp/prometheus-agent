//! `prometheus projects` (spec: projects, Project records; Project
//! commands; Active project and path ownership; Archive and restore).
//!
//! Projects are records of the profile's `state.db` (`pmt_store::projects`);
//! this module parses the commands and renders them. `<project>` is an id
//! or a slug; an unknown one exits 66.

use std::fmt::Write as _;
use std::path::PathBuf;

use clap::{Args, Subcommand};
use pmt_core::error::Error;
use pmt_store::{NewProject, Project, Store};
use serde_json::{Value, json};

use crate::context::Context;
use crate::output::{document, json as print_json, print_raw};

/// `prometheus projects` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum ProjectsCommand {
    /// Create a project; the first folder is primary unless `--primary`.
    Create(CreateArgs),
    /// List projects, the active one marked `*`.
    #[command(visible_alias = "ls")]
    List {
        /// Include archived projects.
        #[arg(long)]
        all: bool,
    },
    /// Show a project with its folders (missing ones flagged).
    Show {
        /// Project id or slug.
        project: String,
    },
    /// Add a folder to a project.
    AddFolder {
        /// Project id or slug.
        project: String,
        /// The folder (an existing directory).
        path: PathBuf,
        /// A label shown next to it.
        #[arg(long)]
        label: Option<String>,
        /// Make it the primary folder.
        #[arg(long)]
        primary: bool,
    },
    /// Remove a folder from a project.
    RemoveFolder {
        /// Project id or slug.
        project: String,
        /// One of its folders.
        path: PathBuf,
    },
    /// Rename a project (its slug stays).
    Rename {
        /// Project id or slug.
        project: String,
        /// The new display name.
        name: String,
    },
    /// Make one of a project's folders its primary.
    SetPrimary {
        /// Project id or slug.
        project: String,
        /// One of its folders.
        path: PathBuf,
    },
    /// Set the active project (omit it to clear).
    Use {
        /// Project id or slug.
        project: Option<String>,
    },
    /// Archive a project: hidden from listings, its folders released.
    Archive {
        /// Project id or slug.
        project: String,
    },
    /// Restore an archived project.
    Restore {
        /// Project id or slug.
        project: String,
    },
    /// Bind a project to a kanban board (omit the board to unbind).
    BindBoard {
        /// Project id or slug.
        project: String,
        /// The board's slug.
        board: Option<String>,
    },
}

/// `projects create` arguments.
#[derive(Debug, Clone, Args)]
pub struct CreateArgs {
    /// Display name (the slug derives from it unless `--slug`).
    pub name: String,
    /// Its folders (existing directories).
    pub folders: Vec<PathBuf>,
    /// Explicit slug, `^[a-z0-9][a-z0-9_-]{0,63}$`.
    #[arg(long)]
    pub slug: Option<String>,
    /// The primary folder (added when not among the folders).
    #[arg(long, value_name = "PATH")]
    pub primary: Option<PathBuf>,
    /// A description.
    #[arg(long, value_name = "TEXT")]
    pub description: Option<String>,
    /// An icon.
    #[arg(long)]
    pub icon: Option<String>,
    /// A color.
    #[arg(long)]
    pub color: Option<String>,
    /// Bind it to this kanban board.
    #[arg(long, value_name = "SLUG")]
    pub board: Option<String>,
    /// Make it the active project.
    #[arg(long = "use")]
    pub activate: bool,
}

/// Runs one `projects` subcommand on the selected profile's store.
///
/// # Errors
///
/// `project.not_found` (66), `project.folder_claimed` (1) and the other
/// `project.*` errors, store errors.
pub fn run(command: &ProjectsCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let store = Store::open(&ctx.scope()?)?;
    let out = Out { ctx, store: &store };
    match command {
        ProjectsCommand::Create(args) => create(&out, args),
        ProjectsCommand::List { all } => list(&out, *all),
        ProjectsCommand::Show { project } => out.show(&store.project(project)?),
        ProjectsCommand::AddFolder {
            project,
            path,
            label,
            primary,
        } => out.changed(
            &store.add_project_folder(project, path, label.as_deref(), *primary)?,
            "added a folder to",
        ),
        ProjectsCommand::RemoveFolder { project, path } => out.changed(
            &store.remove_project_folder(project, path)?,
            "removed a folder from",
        ),
        ProjectsCommand::Rename { project, name } => {
            out.changed(&store.rename_project(project, name)?, "renamed")
        }
        ProjectsCommand::SetPrimary { project, path } => out.changed(
            &store.set_project_primary(project, path)?,
            "set the primary folder of",
        ),
        ProjectsCommand::Use { project } => match store.use_project(project.as_deref())? {
            Some(project) => out.changed(&project, "activated"),
            None => out.cleared(),
        },
        ProjectsCommand::Archive { project } => {
            out.changed(&store.archive_project(project)?, "archived")
        }
        ProjectsCommand::Restore { project } => {
            out.changed(&store.restore_project(project)?, "restored")
        }
        ProjectsCommand::BindBoard { project, board } => {
            let project = store.bind_project_board(project, board.as_deref())?;
            let verb = if board.is_some() { "bound" } else { "unbound" };
            out.changed(&project, verb)
        }
    }
}

fn create(out: &Out<'_>, args: &CreateArgs) -> Result<u8, Error> {
    let new = NewProject {
        name: args.name.clone(),
        slug: args.slug.clone(),
        folders: args.folders.clone(),
        primary: args.primary.clone(),
        description: args.description.clone(),
        icon: args.icon.clone(),
        color: args.color.clone(),
        board: args.board.clone(),
    };
    let project = out.store.create_project(&new, args.activate)?;
    if out.ctx.json {
        return out.show(&project);
    }
    eprintln!("created project '{}'", project.slug);
    print_raw(&format!("{}\n", project.slug))
}

fn list(out: &Out<'_>, all: bool) -> Result<u8, Error> {
    let projects = out.store.projects(all)?;
    let active = out.active_id()?;
    if out.ctx.json {
        let rows: Vec<Value> = projects
            .iter()
            .map(|p| project_json(p, active.as_deref()))
            .collect();
        return document("projects list", &json!({ "projects": rows }));
    }
    if projects.is_empty() {
        eprintln!("no projects");
        return Ok(0);
    }
    let width = projects.iter().map(|p| p.slug.len()).max().unwrap_or(0);
    let mut text = String::new();
    for project in &projects {
        let mark = if is_active(project, active.as_deref()) {
            '*'
        } else {
            ' '
        };
        let archived = if project.archived { "  (archived)" } else { "" };
        let _ = writeln!(
            text,
            "{mark} {:<width$}  {}  {}{archived}",
            project.slug,
            project.name,
            project.primary_path().unwrap_or("-"),
        );
    }
    print_raw(&text)
}

/// Rendering shared by the subcommands.
struct Out<'a> {
    ctx: &'a Context<'a>,
    store: &'a Store,
}

impl Out<'_> {
    fn active_id(&self) -> Result<Option<String>, Error> {
        Ok(self.store.active_project()?.map(|p| p.id.to_string()))
    }

    fn show(&self, project: &Project) -> Result<u8, Error> {
        let active = self.active_id()?;
        if self.ctx.json {
            let body = json!({ "project": project_json(project, active.as_deref()) });
            return document("projects show", &body);
        }
        print_raw(&render(project, is_active(project, active.as_deref())))
    }

    fn changed(&self, project: &Project, verb: &str) -> Result<u8, Error> {
        if self.ctx.json {
            return self.show(project);
        }
        eprintln!("{verb} project '{}'", project.slug);
        Ok(0)
    }

    fn cleared(&self) -> Result<u8, Error> {
        if self.ctx.json {
            return print_json(&json!({ "active": null }));
        }
        eprintln!("no project is active");
        Ok(0)
    }
}

fn is_active(project: &Project, active: Option<&str>) -> bool {
    active == Some(project.id.to_string().as_str())
}

fn project_json(project: &Project, active: Option<&str>) -> Value {
    let folders: Vec<Value> = project
        .folders
        .iter()
        .map(|f| {
            json!({
                "path": f.path,
                "label": f.label,
                "primary": f.is_primary,
                "missing": f.missing(),
            })
        })
        .collect();
    json!({
        "id": project.id.to_string(),
        "slug": project.slug,
        "name": project.name,
        "description": project.description,
        "icon": project.icon,
        "color": project.color,
        "board": project.board,
        "primary_path": project.primary_path(),
        "created_at": project.created_at.to_string(),
        "archived": project.archived,
        "active": is_active(project, active),
        "folders": folders,
    })
}

fn render(project: &Project, active: bool) -> String {
    let mut text = String::new();
    let mut field = |name: &str, value: &str| {
        let _ = writeln!(text, "{name:<13}{value}");
    };
    field("slug", &project.slug);
    field("name", &project.name);
    field("id", &project.id.to_string());
    field("active", if active { "yes" } else { "no" });
    field("archived", if project.archived { "yes" } else { "no" });
    for (name, value) in [
        ("description", &project.description),
        ("icon", &project.icon),
        ("color", &project.color),
        ("board", &project.board),
    ] {
        field(name, value.as_deref().unwrap_or("-"));
    }
    field("created", &project.created_at.to_string());
    if project.folders.is_empty() {
        field("folders", "-");
    }
    for (n, folder) in project.folders.iter().enumerate() {
        field(if n == 0 { "folders" } else { "" }, &folder_line(folder));
    }
    text
}

/// `<path>  (primary)  [label]  missing`, the flags only when they apply.
fn folder_line(folder: &pmt_store::Folder) -> String {
    let mut line = folder.path.clone();
    if folder.is_primary {
        line.push_str("  (primary)");
    }
    if let Some(label) = &folder.label {
        let _ = write!(line, "  [{label}]");
    }
    if folder.missing() {
        line.push_str("  missing");
    }
    line
}
