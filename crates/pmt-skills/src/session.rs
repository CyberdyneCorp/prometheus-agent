//! One session's view of the skills (spec: skills, Prompt-cache
//! preservation for skill changes; Profile-scoped skill state).
//!
//! Everything is resolved from the session's own [`ProfileScope`]: the
//! roots, the lockfile, the usage rows, the staged writes. The index is
//! frozen when the session starts and changes only on an explicit `--now`
//! ([`SkillsSession::refresh_now`]); every other change reaches the model
//! as a per-turn note ([`SkillsSession::change_note`]).
//!
//! Tool handlers are process-wide registry entries, so a session attaches
//! its [`SkillsSession`] to a [`SkillSessions`] table under its id and the
//! handlers look it up from the call context.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError, RwLock};

use pmt_config::schema::Config;
use pmt_config::{Launch, Overrides, ProfileScope};
use pmt_core::capability::{Capability, CapabilitySet};
use pmt_store::Store;
use pmt_tools::terminal::SecretSource;
use pmt_tools::{CallContext, ToolFailure};

use crate::bundled::Bundled;
use crate::catalog::{Catalog, Skill};
use crate::gates::{self, SessionView};
use crate::index::{self, Difference, Posture, Snapshot};
use crate::inline::InlineShell;
use crate::lockfile::LockFile;
use crate::review::ReviewState;
use crate::roots::{RootDir, RootInputs, Roots, profile_skills};
use crate::scan::{PatternScanner, Scanner};
use crate::secrets::SecretPrompt;

/// What a session's skills are built from.
#[derive(Clone)]
pub struct SessionSetup {
    /// The profile.
    pub scope: ProfileScope,
    /// Its store (usage rows).
    pub store: Arc<Store>,
    /// The session id (template variable `${PROMETHEUS_SESSION_ID}`).
    pub session_id: String,
    /// The working directory (project skills).
    pub cwd: Option<PathBuf>,
    /// The user's home (`~`, project-root walk limit).
    pub user_home: Option<PathBuf>,
    /// The surface and the tools on offer.
    pub view: SessionView,
    /// How the index is rendered.
    pub posture: Posture,
    /// The bundled tree.
    pub bundled: Arc<Bundled>,
    /// Plugin skill directories (M2).
    pub plugins: Vec<(String, PathBuf)>,
    /// The skills guard.
    pub scanner: Arc<dyn Scanner>,
    /// Runs inline shell snippets in the session sandbox; `None` renders
    /// them as an error.
    pub inline_shell: Option<Arc<dyn InlineShell>>,
    /// Asks the user for a missing skill secret (interactive surfaces).
    pub secret_prompt: Option<Arc<dyn SecretPrompt>>,
    /// A background skill review (restricted `skill_manage`).
    pub review: bool,
}

impl SessionSetup {
    /// A setup with the built-in bundled tree and scanner, no cwd, no
    /// inline shell and no secret prompt.
    #[must_use]
    pub fn new(scope: ProfileScope, store: Arc<Store>, session_id: impl Into<String>) -> Self {
        Self {
            scope,
            store,
            session_id: session_id.into(),
            cwd: None,
            user_home: None,
            view: SessionView::default(),
            posture: Posture::Default,
            bundled: Bundled::builtin(),
            plugins: Vec::new(),
            scanner: Arc::new(PatternScanner),
            inline_shell: None,
            secret_prompt: None,
            review: false,
        }
    }
}

struct Frozen {
    text: Option<String>,
    snapshot: Snapshot,
}

/// One session's skills.
pub struct SkillsSession {
    setup: SessionSetup,
    frozen: Mutex<Frozen>,
    last_note: Mutex<Option<Difference>>,
    loaded: Mutex<BTreeMap<String, Vec<String>>>,
    review: Option<ReviewState>,
}

impl SkillsSession {
    /// Scans the roots and freezes the session's index.
    #[must_use]
    pub fn new(setup: SessionSetup) -> Self {
        let review = setup.review.then(ReviewState::default);
        let session = Self {
            setup,
            frozen: Mutex::new(Frozen {
                text: None,
                snapshot: Snapshot::new(),
            }),
            last_note: Mutex::new(None),
            loaded: Mutex::new(BTreeMap::new()),
            review,
        };
        session.refresh_now();
        session
    }

    /// The setup.
    #[must_use]
    pub fn setup(&self) -> &SessionSetup {
        &self.setup
    }

    /// The profile scope.
    #[must_use]
    pub fn scope(&self) -> &ProfileScope {
        &self.setup.scope
    }

    /// The profile's store.
    #[must_use]
    pub fn store(&self) -> &Store {
        &self.setup.store
    }

    /// `<profile>/skills`.
    #[must_use]
    pub fn skills_dir(&self) -> PathBuf {
        profile_skills(self.setup.scope.dir())
    }

    /// The review state, in a background review.
    #[must_use]
    pub fn review(&self) -> Option<&ReviewState> {
        self.review.as_ref()
    }

    /// The profile config as on disk now (the session's snapshot when the
    /// file no longer loads): skill changes made during the session are
    /// visible to the tools at once.
    #[must_use]
    pub fn config(&self) -> Config {
        let scope = &self.setup.scope;
        Overrides::parse::<&str>(&[])
            .and_then(|o| ProfileScope::open(scope.profile().clone(), &o, Launch::Other))
            .map_or_else(|_| scope.config().clone(), |s| s.config().clone())
    }

    /// The roots under `config`.
    #[must_use]
    pub fn roots(&self, config: &Config) -> Roots {
        Roots::resolve(&RootInputs {
            config: &config.skills,
            profile_dir: self.setup.scope.dir(),
            cwd: self.setup.cwd.as_deref(),
            user_home: self.setup.user_home.as_deref(),
            plugins: &self.setup.plugins,
            bundled: Arc::clone(&self.setup.bundled),
        })
    }

    /// The skills as on disk now, with the config they were read under.
    #[must_use]
    pub fn catalog(&self) -> (Catalog, Config) {
        let config = self.config();
        let lock = LockFile::in_dir(&self.skills_dir()).read();
        let catalog = Catalog::load(&self.roots(&config), &lock, self.setup.scanner.as_ref());
        (catalog, config)
    }

    /// The skills offered to the model at all (not disabled, right OS).
    #[must_use]
    pub fn visible<'a>(&self, catalog: &'a Catalog, config: &Config) -> Vec<&'a Skill> {
        catalog
            .skills
            .iter()
            .filter(|s| gates::hidden(&config.skills, &self.setup.view, s).is_none())
            .map(AsRef::as_ref)
            .collect()
    }

    /// The skills the prompt index lists.
    #[must_use]
    pub fn indexed<'a>(&self, catalog: &'a Catalog, config: &Config) -> Vec<&'a Skill> {
        self.visible(catalog, config)
            .into_iter()
            .filter(|s| gates::conditions_ok(&self.setup.view, s))
            .collect()
    }

    /// The frozen index text (the prompt's skills layer).
    #[must_use]
    pub fn index_text(&self) -> Option<String> {
        lock(&self.frozen).text.clone()
    }

    /// Rebuilds the frozen index from the current state (session start and
    /// an explicit `--now`); returns the notices of the scan.
    pub fn refresh_now(&self) -> Vec<String> {
        let (catalog, config) = self.catalog();
        let indexed = self.indexed(&catalog, &config);
        let chars = usize::try_from(config.skills.index_description_chars).unwrap_or(usize::MAX);
        *lock(&self.frozen) = Frozen {
            text: index::render(&indexed, chars, self.setup.posture),
            snapshot: if self.setup.posture == Posture::SafeWorker {
                Snapshot::new()
            } else {
                index::snapshot(&indexed)
            },
        };
        *lock(&self.last_note) = None;
        catalog.notices
    }

    /// The note for the next user message when the indexable skills differ
    /// from the frozen index and the difference changed since the last
    /// note; `None` otherwise.
    #[must_use]
    pub fn change_note(&self) -> Option<String> {
        if self.setup.posture == Posture::SafeWorker {
            return None;
        }
        let (catalog, config) = self.catalog();
        let current = index::snapshot(&self.indexed(&catalog, &config));
        let diff = Difference::between(&lock(&self.frozen).snapshot, &current);
        let mut last = lock(&self.last_note);
        let unchanged = match last.as_ref() {
            Some(previous) => *previous == diff,
            None => diff.is_empty(),
        };
        if unchanged {
            return None;
        }
        let chars = usize::try_from(config.skills.index_description_chars).unwrap_or(usize::MAX);
        let text = index::note(&diff, &current, chars);
        *last = Some(diff);
        Some(text)
    }

    /// Records that `skill` was loaded with its declared secret names.
    pub fn mark_loaded(&self, skill: &str, secrets: Vec<String>) {
        lock(&self.loaded).insert(skill.to_owned(), secrets);
    }

    /// The secret names the loaded skills declared: only these may be
    /// passed into the session's terminal and code-execution sandboxes.
    #[must_use]
    pub fn granted_secrets(&self) -> BTreeSet<String> {
        lock(&self.loaded).values().flatten().cloned().collect()
    }

    /// The terminal's secret source: the declared secrets of the loaded
    /// skills, resolved from the profile at each command (unset ones are
    /// skipped). Holds the session weakly.
    #[must_use]
    pub fn secret_source(self: &Arc<Self>) -> SecretSource {
        let session = Arc::downgrade(self);
        Arc::new(move || {
            let Some(session) = session.upgrade() else {
                return Vec::new();
            };
            let secrets = session.scope().secrets();
            session
                .granted_secrets()
                .into_iter()
                .filter_map(|name| secrets.resolve(&name).ok().map(|value| (name, value)))
                .collect()
        })
    }

    /// Read grants the session sandbox needs for skill directories on
    /// disk (inline shell runs in the skill directory; scripts are read
    /// from it): the profile skills directory and every other disk root.
    /// Session start adds them to the effective capability set, as it
    /// does for its tool output directories.
    #[must_use]
    pub fn sandbox_reads(&self) -> CapabilitySet {
        let config = self.config();
        self.roots(&config)
            .roots
            .iter()
            .filter_map(|root| match &root.dir {
                RootDir::Disk(dir) => Some(Capability::read_under(dir)),
                RootDir::Bundled(_) => None,
            })
            .chain([Capability::read_under(&self.skills_dir())])
            .collect()
    }

    /// The working directory.
    #[must_use]
    pub fn cwd(&self) -> Option<&Path> {
        self.setup.cwd.as_deref()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The attached skill sessions of a process, by session id.
#[derive(Default)]
pub struct SkillSessions {
    sessions: RwLock<HashMap<String, Arc<SkillsSession>>>,
}

static PROCESS: LazyLock<Arc<SkillSessions>> = LazyLock::new(Arc::default);

impl SkillSessions {
    /// The process-wide table the built-in skill tools use.
    #[must_use]
    pub fn process() -> Arc<Self> {
        Arc::clone(&PROCESS)
    }

    /// Attaches `session` under `session_id` until the returned
    /// [`Attachment`] drops.
    #[must_use]
    pub fn attach(self: &Arc<Self>, session_id: &str, session: Arc<SkillsSession>) -> Attachment {
        self.sessions
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(session_id.to_owned(), session);
        Attachment {
            table: Arc::clone(self),
            session_id: session_id.to_owned(),
        }
    }

    /// The skills of the calling session.
    ///
    /// # Errors
    ///
    /// An `unavailable` failure when none is attached.
    pub fn of(&self, context: &CallContext) -> Result<Arc<SkillsSession>, ToolFailure> {
        self.sessions
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&context.session_id)
            .cloned()
            .ok_or_else(|| {
                ToolFailure::new(
                    "unavailable",
                    format!("no skills are attached to session {}", context.session_id),
                )
            })
    }
}

/// Keeps a session's skills attached; dropping it detaches.
pub struct Attachment {
    table: Arc<SkillSessions>,
    session_id: String,
}

impl Drop for Attachment {
    fn drop(&mut self) {
        self.table
            .sessions
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.session_id);
    }
}
