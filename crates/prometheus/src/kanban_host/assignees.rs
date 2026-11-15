//! Who a task may name (spec: kanban, Task creation; Per-task model
//! override and pinned skills; Worker lanes; projects, Kanban board
//! binding): the home's profiles, the worker lanes the invoking profile's
//! enabled plugins declare, the model override checked against the
//! assignee profile's provider registry, pinned skills against its
//! installed skills, and the project of the invoking profile bound to the
//! board.

use std::path::{Path, PathBuf};

use pmt_config::{Launch, Overrides, ProfileName, ProfileScope};
use pmt_core::error::{Error, codes};
use pmt_kanban::{NewTask, Profiles};

use super::profile_exists;

/// The assignees of a home, seen from one profile.
pub struct Assignees {
    home: PathBuf,
    scope: Option<ProfileScope>,
    lanes: Vec<String>,
}

impl Assignees {
    /// The home's profiles and the lanes `scope`'s enabled plugins declare.
    #[must_use]
    pub fn of(scope: &ProfileScope) -> Self {
        Self {
            home: scope.home().to_path_buf(),
            lanes: super::lanes::declared(scope)
                .into_iter()
                .map(|l| l.lane.name)
                .collect(),
            scope: Some(scope.clone()),
        }
    }

    /// Only the home's profiles (no lanes, no checks beyond existence).
    #[must_use]
    pub fn home(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            scope: None,
            lanes: Vec::new(),
        }
    }

    /// The assignee profile's scope (its own configuration only).
    fn assignee_scope(&self, name: &str) -> Result<ProfileScope, Error> {
        let home = pmt_config::layout::Home::open(&self.home)?;
        let dir = home.existing_profile(&ProfileName::parse(name)?)?;
        ProfileScope::open(dir, &Overrides::default(), Launch::Other)
    }
}

fn invalid(message: String) -> Error {
    Error::new(&codes::KANBAN_INVALID_TASK, message)
}

/// Checks a model override against `scope`'s provider registry.
fn check_model(scope: &ProfileScope, model: &str, provider: Option<&str>) -> Result<(), Error> {
    let config = scope.config();
    let registry = pmt_providers::ProviderRegistry::for_profile(config, Vec::new())?;
    let reference = match provider {
        Some(p) => {
            registry
                .lookup(p)
                .map_err(|e| invalid(format!("--provider: {}", e.message())))?;
            format!("{p}/{model}")
        }
        None => model.to_owned(),
    };
    pmt_providers::target::parse(&reference, &config.model, &registry, None)
        .map(|_| ())
        .map_err(|e| invalid(format!("--model: {}", e.message())))
}

/// The installed, enabled skills of `scope`.
#[must_use]
pub fn installed_skills(scope: &ProfileScope) -> Vec<String> {
    let catalog = pmt_skills::eject::profile_catalog(
        scope,
        None,
        None,
        pmt_skills::bundled::Bundled::builtin(),
    );
    let disabled = &scope.config().skills.disabled;
    catalog
        .skills
        .iter()
        .filter(|s| !disabled.contains(&s.name))
        .map(|s| s.name.clone())
        .collect()
}

impl Profiles for Assignees {
    fn exists(&self, name: &str) -> bool {
        profile_exists(&self.home, name)
    }

    fn lane(&self, name: &str) -> bool {
        self.lanes.iter().any(|l| l == name)
    }

    fn validate(&self, new: &NewTask) -> Result<(), Error> {
        let wants_checks = new.model.is_some() || !new.skills.is_empty();
        let Some(assignee) = new.assignee.as_deref().filter(|_| wants_checks) else {
            return Ok(());
        };
        if self.lane(assignee) && !self.exists(assignee) {
            return Err(invalid(format!(
                "a model override or pinned skills need a profile assignee ('{assignee}' is a worker lane)"
            )));
        }
        let scope = self.assignee_scope(assignee)?;
        if let Some(model) = new.model.as_deref() {
            check_model(&scope, model, new.provider.as_deref())?;
        }
        let installed = installed_skills(&scope);
        if let Some(missing) = new.skills.iter().find(|s| !installed.contains(s)) {
            return Err(invalid(format!(
                "unknown skill '{missing}' for profile '{assignee}'"
            )));
        }
        Ok(())
    }

    fn project(&self, board: &str) -> Option<(String, PathBuf)> {
        let scope = self.scope.as_ref()?;
        let store = pmt_store::Store::open(scope).ok()?;
        let project = store
            .projects(false)
            .ok()?
            .into_iter()
            .find(|p| p.board.as_deref() == Some(board))?;
        let root = PathBuf::from(project.primary_path()?);
        Some((project.slug, root))
    }
}
