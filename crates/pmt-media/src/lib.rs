//! The media tools (spec: web-and-media-tools): `image_generate`
//! ([`image`]), `video_analyze` ([`video`]), `text_to_speech` and the
//! speech-to-text service of voice input ([`speech`]) and `x_search`
//! ([`x_search`]).
//!
//! Every tool has one fixed contract while providers are selected by the
//! category's config key ([`access`]); results name the provider, the
//! model and the `billing_source`, and metered calls reach the usage
//! ledger through the session's sink. A session attaches its
//! [`MediaSession`] to the process table [`MediaSessions`] the tool
//! handlers look it up in.

pub mod access;
pub mod explain;
pub mod http;
pub mod image;
pub mod plugin;
pub mod probe;
pub mod speech;
pub mod video;
pub mod x_search;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, PoisonError, RwLock};

use pmt_net::fetch::Fetcher;
use pmt_tools::RegisteredTool;
use pmt_tools::descriptor::{CallContext, ToolFailure};
use pmt_tools::usage::UsageSink;

pub use access::{Access, Credentials};

/// Where generated media is kept.
pub trait MediaHost: Send + Sync {
    /// Stores `bytes` (`mime`) in the profile's blob store, referenced by
    /// the session; returns the file's path.
    ///
    /// # Errors
    ///
    /// The store refused the write.
    fn store_media(&self, bytes: &[u8], mime: &str) -> Result<PathBuf, String>;
}

/// A media host writing into a directory (tests and sessions without a
/// store).
pub struct DirHost(pub PathBuf);

impl MediaHost for DirHost {
    fn store_media(&self, bytes: &[u8], mime: &str) -> Result<PathBuf, String> {
        pmt_core::private_fs::create_dir(&self.0).map_err(|e| e.to_string())?;
        let path = self.0.join(format!(
            "{}.{}",
            ulid::Ulid::generate(),
            speech::audio::extension_of(mime)
        ));
        pmt_core::private_fs::write_atomic(&path, bytes).map_err(|e| e.to_string())?;
        Ok(path)
    }
}

/// What a session's media tools are built from.
pub struct MediaSetup {
    /// The profile's configuration and credentials.
    pub credentials: Credentials,
    /// Calls the providers' APIs.
    pub client: reqwest::Client,
    /// The session's URL policy for media the model points at.
    pub fetcher: Arc<Fetcher>,
    /// Where generated media goes.
    pub host: Arc<dyn MediaHost>,
    /// The profile's home directory.
    pub profile_dir: PathBuf,
    /// The session's workspace (relative paths, `output_path`).
    pub workspace: PathBuf,
    /// The session's surface (`cli`, `tui`, a platform name).
    pub surface: String,
    /// Base URL overrides by provider id (mirrors, tests).
    pub endpoints: BTreeMap<String, String>,
    /// Where metered calls are recorded.
    pub usage: Option<UsageSink>,
    /// The `vision` auxiliary route when its model takes video.
    pub video: Option<Arc<dyn video::VideoDescriber>>,
    /// Runs command-template speech providers inside the sandbox.
    pub runner: Arc<dyn speech::command::CommandRunner>,
    /// Starts the local speech engines (`None`: not installed).
    pub local: Option<Arc<dyn speech::local::Launcher>>,
    /// Image, speech and transcription providers of enabled plugins.
    pub plugins: Arc<dyn plugin::MediaPlugins>,
}

impl MediaSetup {
    /// The base URL of `provider`: its override, else `default`.
    #[must_use]
    pub fn endpoint(&self, provider: &str, default: &str) -> String {
        self.endpoints
            .get(provider)
            .map_or(default, String::as_str)
            .trim_end_matches('/')
            .to_owned()
    }

    /// The profile's configuration.
    #[must_use]
    pub fn config(&self) -> &pmt_config::Config {
        &self.credentials.config
    }
}

/// One session's media tools.
pub struct MediaSession {
    setup: MediaSetup,
    image_slots: tokio::sync::Semaphore,
    speech: speech::SpeechState,
}

impl MediaSession {
    /// The session's media tools.
    #[must_use]
    pub fn new(setup: MediaSetup) -> Self {
        let config = setup.config();
        let slots = image::parallel_limit(config);
        let speech = speech::SpeechState::new(config);
        Self {
            setup,
            image_slots: tokio::sync::Semaphore::new(slots),
            speech,
        }
    }

    /// The same session converting audio with `ffmpeg` (default: the one
    /// on the `PATH`).
    #[must_use]
    pub fn with_ffmpeg(mut self, ffmpeg: speech::audio::Ffmpeg) -> Self {
        self.speech.ffmpeg = ffmpeg;
        self
    }

    /// Its setup.
    #[must_use]
    pub fn setup(&self) -> &MediaSetup {
        &self.setup
    }
}

/// The attached media sessions of a process, by session id.
#[derive(Default)]
pub struct MediaSessions {
    sessions: RwLock<HashMap<String, Arc<MediaSession>>>,
}

static PROCESS: LazyLock<Arc<MediaSessions>> = LazyLock::new(Arc::default);

impl MediaSessions {
    /// The process-wide table the media tools use.
    #[must_use]
    pub fn process() -> Arc<Self> {
        Arc::clone(&PROCESS)
    }

    /// Attaches `session` under `session_id` until the returned
    /// [`Attachment`] drops.
    #[must_use]
    pub fn attach(self: &Arc<Self>, session_id: &str, session: Arc<MediaSession>) -> Attachment {
        self.sessions
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(session_id.to_owned(), session);
        Attachment {
            table: Arc::clone(self),
            session_id: session_id.to_owned(),
        }
    }

    /// The media session of the calling session.
    ///
    /// # Errors
    ///
    /// An `unavailable` failure when none is attached.
    pub fn of(&self, context: &CallContext) -> Result<Arc<MediaSession>, ToolFailure> {
        self.session(&context.session_id).ok_or_else(|| {
            ToolFailure::new(
                "unavailable",
                format!(
                    "no media tools are attached to session {}",
                    context.session_id
                ),
            )
        })
    }

    /// The media session attached to `session_id`.
    #[must_use]
    pub fn session(&self, session_id: &str) -> Option<Arc<MediaSession>> {
        self.sessions
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(session_id)
            .cloned()
    }
}

/// Keeps a media session attached; dropping it detaches.
pub struct Attachment {
    table: Arc<MediaSessions>,
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

/// `image_generate`, `video_analyze`, `text_to_speech` and `x_search`,
/// their handlers bound to `sessions`.
#[must_use]
pub fn tools(sessions: &Arc<MediaSessions>) -> Vec<RegisteredTool> {
    vec![
        image::tool::registered(sessions),
        video::registered(sessions),
        speech::tool::registered(sessions),
        x_search::registered(sessions),
    ]
}
