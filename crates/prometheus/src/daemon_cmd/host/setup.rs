//! How a client asked a daemon session to run (spec: cli, Daemon
//! attachment with in-process fallback): the session-shaping chat flags
//! of `session.create` and `session.resume` become the session's own
//! factory (its overrides, options and route), its effective configuration
//! after reloads, and the images and run budget of `prompt.submit`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_agent::turn::TurnInput;
use pmt_agent::turn::transcript::{ImageRef, images_of};
use pmt_config::{Launch, Overrides, ProfileScope};
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_core::secret::SecretString;
use pmt_rpc::contracts::{PromptImage, SessionCreateParams, SessionSetup, SubmitParams};
use pmt_store::ephemeral::TAG as EPHEMERAL;
use pmt_store::{NewSession, Visibility};

use super::{DaemonHost, Profile, busy};
use crate::chat::attach;
use crate::chat::session::model::RouteFlags;
use crate::chat::session::{Factory, Live, Options};
use crate::chat::usage_file::RunRecord;

/// The route flags of `setup`.
pub(super) fn flags(setup: &SessionSetup) -> RouteFlags<'_> {
    RouteFlags {
        provider: setup.provider.as_deref(),
        model: setup.model.as_deref(),
        mock_script: setup.mock_script.as_deref().map(Path::new),
        unrecorded: setup.ephemeral,
    }
}

/// Whether a person attends the session at a client (TUI, desktop,
/// dashboard, editor, CLI): its plugins include those `plugins dev`
/// serves (spec: plugin-system, Plugin development loop). Gateway, cron,
/// API, subagent and other automated sessions are never attended.
pub(super) fn attended(source: pmt_store::Source, origin: &pmt_store::Origin) -> bool {
    use pmt_store::Source;
    matches!(origin, pmt_store::Origin::None)
        && matches!(
            source,
            Source::Cli | Source::Tui | Source::Desktop | Source::Dashboard | Source::Acp
        )
}

/// Tags a new session: the client's source tag, or [`EPHEMERAL`] (hidden
/// from listings) for an ephemeral session. The onboarding and ephemeral
/// tags are reserved.
pub(super) fn tag(params: &SessionCreateParams, new: &mut NewSession) -> Result<(), Error> {
    let reserved = |tag: &str, why: &str| {
        Error::new(
            &codes::SESSION_INVALID_SOURCE_TAG,
            format!("the source tag '{tag}' is reserved {why}"),
        )
    };
    if let Some(tag) = &params.source_tag {
        pmt_store::sessions::check_source_tag(tag)?;
        // Only `onboarding.ensure` makes the onboarding chat, the one
        // session with `setup_beat`.
        if tag == crate::chat::session::tools::ONBOARDING_TAG {
            return Err(reserved(
                tag,
                "for the desktop's onboarding chat (onboarding.ensure)",
            ));
        }
        if tag == EPHEMERAL || params.setup.ephemeral {
            return Err(reserved(
                EPHEMERAL,
                "for ephemeral sessions, which take no other tag",
            ));
        }
        new.source_tag = Some(tag.clone());
    }
    if params.setup.ephemeral {
        new.source_tag = Some(EPHEMERAL.to_owned());
        new.hidden = true;
    }
    Ok(())
}

/// Readies a loaded session for its clients: they render tool progress
/// from `session.event`, and an embedding program's overlay rides with
/// every model call.
pub(super) fn client_rendered(live: &mut Live, setup: &SessionSetup) {
    drop(live.progress.take());
    live.agent
        .set_ephemeral_system(setup.system_prompt_overlay.clone());
}

/// Removes an ephemeral session's transcript once it leaves memory.
pub(super) fn forget_if_ephemeral(store: &pmt_store::Store, id: SessionId, setup: &SessionSetup) {
    if setup.ephemeral
        && let Err(err) = store.forget_transcript(id)
    {
        tracing::error!(session_id = %id, "an ephemeral transcript was not removed: {err}");
    }
}

/// Whether a session with `setup` reads anything but the profile's
/// snapshot.
fn own_config(setup: &SessionSetup) -> bool {
    !setup.set.is_empty() || setup.ignore_user_config
}

impl DaemonHost {
    /// Whether profile `name` sees the launch environment's secrets.
    pub(super) fn launch_of(&self, name: &str) -> Launch<'_> {
        if name == self.launch_profile {
            Launch::Profile(&self.launch)
        } else {
            Launch::Other
        }
    }

    /// The machinery of unattended sessions of `profile` (gateway, cron,
    /// API...) working in `cwd` with `setup`.
    pub(super) fn factory(
        &self,
        profile: &Profile,
        cwd: PathBuf,
        setup: &SessionSetup,
    ) -> Result<Arc<Factory>, Error> {
        self.factory_for(profile, cwd, setup, false)
    }

    /// The machinery of the stored session `row`.
    pub(super) fn factory_of(
        &self,
        profile: &Profile,
        cwd: PathBuf,
        setup: &SessionSetup,
        row: &pmt_store::Session,
    ) -> Result<Arc<Factory>, Error> {
        self.factory_for(profile, cwd, setup, attended(row.source, &row.origin))
    }

    /// The machinery of sessions of `profile` working in `cwd` with
    /// `setup`, `attended` by a person at a client or not (built once per
    /// triple, rebuilt after a reload).
    pub(super) fn factory_for(
        &self,
        profile: &Profile,
        cwd: PathBuf,
        setup: &SessionSetup,
        attended: bool,
    ) -> Result<Arc<Factory>, Error> {
        let key = (cwd, setup.clone(), attended);
        let mut factories = profile.factories.lock().expect("factories poisoned");
        if let Some(factory) = factories.get(&key) {
            return Ok(Arc::clone(factory));
        }
        let options = Options {
            interactive: true,
            yolo: setup.yolo,
            cwd: key.0.clone(),
            // No recorded or given working directory: the profile's own.
            cwd_fallback: key.0 == profile.scope().dir().join("workspace"),
            skills: setup.skills.clone(),
            pass_session_id: setup.pass_session_id,
            no_context_files: setup.no_context_files,
            source_tag: None,
            ignore_user_config: setup.ignore_user_config,
            ignore_rules: setup.ignore_rules,
            // A client narrows or extends it with `egress.set`.
            egress: crate::egress::Overrides::default(),
            cron: None,
            kanban: None,
            replay: None,
            rollout: None,
            harness_tree: None,
            memory_store: None,
            domain: setup.domain.clone(),
            dev_plugins: attended,
        };
        let ad_hoc = setup
            .base_url
            .as_deref()
            .map(|url| {
                pmt_providers::resolve::AdHoc::new(
                    url,
                    setup.api_key.clone().map(SecretString::new),
                )
            })
            .transpose()?;
        let scope = self.session_scope(profile, &profile.scope(), setup)?;
        let ports = super::send_tool::GatewayPorts {
            gateway: Arc::clone(&self.gateway),
            profile: profile.scope().name().as_str().to_owned(),
        };
        let daemon = crate::daemon_cmd::self_guard::daemon(
            scope.home(),
            self.launch.user_home().map(PathBuf::as_path),
            true,
        );
        let desktop = super::desktop_ui::UiPorts::new(scope.clone(), Arc::clone(&profile.store));
        let factory = Factory::new(scope, &self.launch, &flags(setup), ad_hoc, options)?
            .with_current(profile.current.clone())
            .with_messaging(Arc::new(ports))
            .with_desktop(Arc::new(desktop))
            .hosted_by(daemon);
        let factory = Arc::new(factory);
        factories.insert(key, Arc::clone(&factory));
        Ok(factory)
    }

    /// `current` with `setup`'s overrides (or the schema defaults for
    /// `--ignore-user-config`), keeping the daemon's own overrides and the
    /// restart keys a reload held back.
    fn session_scope(
        &self,
        profile: &Profile,
        current: &ProfileScope,
        setup: &SessionSetup,
    ) -> Result<ProfileScope, Error> {
        if !own_config(setup) {
            return Ok(current.clone());
        }
        let mut set = self.set.clone();
        set.extend(profile.config.lock().expect("config state poisoned").pins());
        set.extend(setup.set.iter().cloned());
        let overrides = Overrides::parse(&set)?;
        let dir = current.profile().clone();
        let launch = self.launch_of(current.name().as_str());
        if setup.ignore_user_config {
            ProfileScope::open_defaults(dir, &overrides, launch)
        } else {
            ProfileScope::open(dir, &overrides, launch)
        }
    }

    /// What a session that started with `start` runs with now: `start`
    /// with the profile's current `live` keys (through the session's own
    /// overrides); `None` when nothing differs or the session ignores the
    /// profile's file.
    pub(super) fn effective(
        &self,
        profile: &Profile,
        setup: &SessionSetup,
        start: &ProfileScope,
    ) -> Option<ProfileScope> {
        if setup.ignore_user_config {
            return None;
        }
        let candidate = self
            .session_scope(profile, &profile.scope(), setup)
            .inspect_err(|err| tracing::warn!(error = %err, "session overrides no longer apply"))
            .ok()?;
        start
            .with_live(&candidate)
            .inspect_err(|err| tracing::error!(error = %err, "live keys not applied"))
            .ok()
    }

    /// Makes `setup` apply when session `id` next loads; an idle loaded
    /// session with another setup is unloaded for it.
    pub(super) async fn set_up(
        profile: &Profile,
        id: SessionId,
        setup: SessionSetup,
    ) -> Result<(), Error> {
        let mut loaded = profile.loaded.lock().await;
        if let Some(running) = loaded.get(&id) {
            if running.setup == setup {
                return Ok(());
            }
            if running.busy.load(std::sync::atomic::Ordering::Acquire) {
                return Err(busy(id));
            }
            loaded.remove(&id);
        }
        profile
            .setups
            .lock()
            .expect("setups poisoned")
            .insert(id, setup);
        Ok(())
    }
}

/// The turn input of `params`: its text, the files it attaches (checked
/// and stored like `chat --image`) and the session's own images it names.
pub(super) async fn input(
    factory: &Factory,
    live: &Live,
    params: &SubmitParams,
) -> Result<TurnInput, Error> {
    let mut files = Vec::new();
    let mut held = Vec::new();
    for (n, image) in params.images.iter().enumerate() {
        match image {
            PromptImage::Path(path) => files.push(attach::load(Path::new(path))?),
            PromptImage::Data(url) => {
                files.push(attach::from_data_url(
                    url,
                    &format!("snapshot-{}.png", n + 1),
                )?);
            }
            PromptImage::Blob(blob) => held.push(held_image(factory, live.id, blob)?),
        }
    }
    held.extend(attach::prepare(factory, live, &files).await?);
    let mut text = params.text.clone();
    for path in &params.files {
        if let Some(copy) = attach::keep_file(factory, live, Path::new(path))? {
            // The note names the workspace copy tools should read.
            text = text.replace(path.as_str(), &copy.to_string_lossy());
        }
    }
    Ok(TurnInput {
        images: held,
        ..TurnInput::text(text)
    })
}

/// An image of one of the session's user messages, as it was stored.
fn held_image(factory: &Factory, id: SessionId, blob: &str) -> Result<ImageRef, Error> {
    factory
        .store
        .messages(id, Visibility::All)?
        .iter()
        .flat_map(images_of)
        .find(|image| image.blob == blob)
        .ok_or_else(|| {
            Error::new(
                &codes::CHAT_IMAGE,
                format!("session {id} holds no image {blob}"),
            )
        })
}

/// A usage record of one turn of `live` (its `--usage-file` report).
pub(super) fn record(factory: &Factory, live: &Live) -> Result<RunRecord, Error> {
    let record = RunRecord::default();
    record.store(&factory.store);
    let root = factory.store.session(live.id)?.root_session_id;
    record.session(live.id, root, &live.model.target());
    record.turn();
    Ok(record)
}

/// The setup of the chat policy frozen with a gateway session, which keeps
/// shaping it after a daemon restart (spec: messaging-gateway, Per-chat
/// policies).
pub(super) fn frozen_policy(
    profile: &super::Profile,
    row: &pmt_store::Session,
) -> Option<pmt_rpc::contracts::SessionSetup> {
    let pmt_store::Origin::Gateway { platform, .. } = &row.origin else {
        return None;
    };
    let text = profile.store.session_policy(row.id).ok().flatten()?;
    let policy: pmt_config::schema::ChatPolicy = serde_json::from_str(&text).ok()?;
    Some(pmt_gateway::messaging::policy::session_setup(
        &policy, platform,
    ))
}
