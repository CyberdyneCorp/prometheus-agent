//! A session's media tools (spec: web-and-media-tools): the
//! [`pmt_media::MediaSession`] attached for its lifetime, with the
//! profile's credentials, the session's blob store and usage ledger, its
//! sandboxed command runner, the managed local speech engines and the
//! `vision` auxiliary route when that route's model takes video.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_config::ProfileScope;
use pmt_core::id::SessionId;
use pmt_media::speech::command::{CommandRunner, ShellSyntax};
use pmt_media::video::{DescribeFuture, VideoDescriber};
use pmt_media::{Credentials, MediaHost, MediaSession, MediaSessions, MediaSetup};
use pmt_providers::auxiliary::{AuxCaller, AuxRoute};
use pmt_providers::cost::CallContext;
use pmt_providers::request::{Message, Part, Request, Role};
use pmt_store::Store;
use pmt_tools::usage::{ToolUsage, UsageSink};

/// The `vision` auxiliary route of a model that takes video.
pub struct VideoAux {
    /// Calls auxiliary routes.
    pub caller: Arc<AuxCaller>,
    /// The resolved route.
    pub route: AuxRoute,
    /// The session (the purpose is set by the caller).
    pub context: CallContext,
}

impl VideoDescriber for VideoAux {
    fn route(&self) -> (String, String) {
        (
            self.route.route.provider.clone(),
            self.route.route.model.clone(),
        )
    }

    fn billing_source(&self) -> String {
        self.route.route.billing_source()
    }

    fn describe<'a>(
        &'a self,
        video: &'a [u8],
        mime: &'a str,
        question: &'a str,
    ) -> DescribeFuture<'a> {
        Box::pin(async move {
            let request = Request {
                model: self.route.route.model.clone(),
                messages: vec![Message {
                    role: Role::User,
                    parts: vec![
                        Part::Image {
                            media_type: mime.to_owned(),
                            data: pmt_media::http::b64(video),
                            is_url: false,
                        },
                        Part::Text {
                            text: question.to_owned(),
                        },
                    ],
                    cache_breakpoint: false,
                }],
                ..Request::default()
            };
            self.caller
                .call(&self.route, &request, &self.context)
                .await
                .map(|reply| reply.text)
                .map_err(|err| err.message().to_owned())
        })
    }
}

/// Generated media in the profile's blob store, referenced by the session.
struct BlobHost {
    store: Arc<Store>,
    session: String,
}

impl MediaHost for BlobHost {
    fn store_media(&self, bytes: &[u8], _mime: &str) -> Result<PathBuf, String> {
        let blob = self
            .store
            .put_blob(bytes)
            .map_err(|e| e.message().to_owned())?;
        self.store
            .reference_blob(&blob, "tool_media", &self.session)
            .map_err(|e| e.message().to_owned())?;
        Ok(self.store.blob_path(&blob))
    }
}

/// The usage ledger of `session` as a tool usage sink: one row per
/// metered call (`purpose = tool.<name>`).
#[must_use]
pub fn usage_sink(store: &Arc<Store>, session: SessionId) -> UsageSink {
    let store = Arc::clone(store);
    Arc::new(move |usage: ToolUsage| record_usage(&store, session, usage))
}

/// The usage ledger of the profile as a sink for calls made outside a
/// session's tools (voice mode, messaging speech): each row goes to the
/// session the call names.
#[must_use]
pub fn profile_usage_sink(store: Arc<Store>) -> UsageSink {
    Arc::new(move |usage: ToolUsage| {
        if let Ok(session) = SessionId::parse(&usage.session_id) {
            record_usage(&store, session, usage);
        } else {
            tracing::warn!(
                session = usage.session_id,
                "speech usage not recorded: no session"
            );
        }
    })
}

fn record_usage(store: &Store, session: SessionId, usage: ToolUsage) {
    let cost_status = if usage.billing_source.starts_with("subscription:") {
        pmt_store::CostStatus::Subscription
    } else {
        pmt_store::CostStatus::Unknown
    };
    let event = pmt_store::usage::UsageEvent {
        call_id: format!("tool:{}:{}", usage.session_id, usage.call_id),
        ts: jiff::Timestamp::now(),
        session_id: session,
        turn_id: None,
        parent_call_id: None,
        purpose: usage.purpose,
        purpose_detail: None,
        provider: usage.provider,
        model: usage.model,
        service_tier: None,
        tokens: pmt_store::usage::Tokens::default(),
        cost_usd: None,
        cost_status,
        billing_source: usage.billing_source,
        request_hash: None,
        cache_hit: false,
    };
    if let Err(err) = store.record_usage(&event) {
        tracing::warn!(error = %err, "tool usage not recorded");
    }
}

/// The profile's credentials for the media tools.
#[must_use]
pub fn credentials(scope: &ProfileScope) -> Credentials {
    Credentials {
        config: Arc::new(scope.config().clone()),
        secrets: Arc::new(scope.secrets().clone()),
        extras: Arc::new(pmt_providers::auth::Credentials::new(
            scope.secrets().store().clone(),
        )),
    }
}

/// Runs command-template speech providers in the session's sandbox with
/// its child environment and shell.
struct SandboxRunner(super::env::Commands);

impl pmt_media::speech::command::CommandRunner for SandboxRunner {
    fn syntax(&self) -> pmt_media::speech::command::ShellSyntax {
        if self.0.shell.kind.is_posix() {
            pmt_media::speech::command::ShellSyntax::Posix
        } else {
            pmt_media::speech::command::ShellSyntax::PowerShell
        }
    }

    fn command(&self, line: &str, cwd: &Path) -> Result<tokio::process::Command, String> {
        let shell = &self.0.shell;
        let mut command = self
            .0
            .sandbox
            .command(&shell.path)
            .map_err(|e| e.message().to_owned())?;
        self.0.env.apply(&mut command);
        if shell.kind.is_posix() {
            command.args(["-c", line]);
        } else {
            command.args(["-NoProfile", "-Command", line]);
        }
        command.current_dir(cwd);
        Ok(tokio::process::Command::from(command))
    }
}

/// Command speech providers run outside a session (gateway voice notes,
/// voice mode): in a sandbox of the profile with `process.spawn`.
pub struct ProfileRunner(pub ProfileScope);

impl CommandRunner for ProfileRunner {
    fn syntax(&self) -> ShellSyntax {
        if cfg!(windows) {
            ShellSyntax::Cmd
        } else {
            ShellSyntax::Posix
        }
    }

    fn command(&self, line: &str, cwd: &Path) -> Result<tokio::process::Command, String> {
        let (shell, flag) = if cfg!(windows) {
            ("cmd", "/C")
        } else {
            ("sh", "-c")
        };
        let cwd = cwd.to_string_lossy();
        let mut command = crate::hooks_host::sandboxed(&self.0, Path::new(shell), &[], Some(&cwd))
            .map_err(|e| e.message().to_owned())?;
        command.args([flag, line]);
        Ok(tokio::process::Command::from(command))
    }
}

/// Starts the local speech engines with the managed `uv` runtime.
struct UvEngines {
    home: PathBuf,
    models: PathBuf,
}

impl UvEngines {
    fn uv(&self) -> Option<PathBuf> {
        pmt_update::runtimes::Store::open(&self.home)
            .installed("uv")
            .map(|installed| installed.exe())
    }
}

impl pmt_media::speech::local::Launcher for UvEngines {
    fn scope(&self) -> String {
        self.models.display().to_string()
    }

    fn available(&self, _engine: &str) -> bool {
        self.uv().is_some()
    }

    fn worker(
        &self,
        _engine: &str,
        package: &str,
        script: &str,
    ) -> Result<tokio::process::Command, String> {
        let uv = self
            .uv()
            .ok_or("the uv runtime is not installed: run 'prometheus runtimes install uv'")?;
        let mut command = tokio::process::Command::new(uv);
        command
            .args([
                "run",
                "--no-project",
                "--quiet",
                "--with",
                package,
                "python",
                "-c",
                script,
            ])
            .env("PMT_ENGINE_DIR", &self.models);
        Ok(command)
    }
}

/// The managed ffmpeg of the home: the pinned package through the managed
/// `uv` runtime when it is installed, else ffmpeg on the `PATH`.
#[must_use]
pub fn ffmpeg(scope: &ProfileScope) -> pmt_media::speech::audio::Ffmpeg {
    match pmt_update::runtimes::Store::open(scope.home()).installed("uv") {
        Some(uv) => pmt_media::speech::audio::Ffmpeg::managed(uv.exe()),
        None => pmt_media::speech::audio::Ffmpeg::on_path(),
    }
}

/// The profile's local speech engines (the managed `uv` runtime).
#[must_use]
pub fn launcher(scope: &ProfileScope) -> Arc<dyn pmt_media::speech::local::Launcher> {
    Arc::new(UvEngines {
        home: scope.home().to_path_buf(),
        models: scope.dir().join("cache").join("speech-models"),
    })
}

/// What a session's media tools are built from.
pub struct MediaInputs<'a> {
    /// The profile.
    pub scope: &'a ProfileScope,
    /// The session store.
    pub store: &'a Arc<Store>,
    /// The session.
    pub session: SessionId,
    /// Its surface (`cli`, `tui`, a platform name).
    pub surface: String,
    /// Its working directory.
    pub cwd: &'a Path,
    /// Its sandboxed command parts, when it has them.
    pub commands: Option<&'a super::env::Commands>,
    /// Its URL policy.
    pub fetcher: Option<Arc<pmt_net::fetch::Fetcher>>,
    /// The video-capable `vision` route, when there is one.
    pub video: Option<Arc<dyn VideoDescriber>>,
    /// Image, speech and transcription providers of enabled plugins.
    pub plugins: Arc<dyn pmt_media::plugin::MediaPlugins>,
}

/// Attaches the session's media tools; `None` when the profile cannot
/// build an outbound client.
#[must_use]
pub fn attach(inputs: MediaInputs<'_>) -> Option<pmt_media::Attachment> {
    let scope = inputs.scope;
    let config = scope.config();
    let outbound = pmt_net::OutboundPolicy {
        force_ipv4: config.network.force_ipv4,
        extra_ca_file: config.network.extra_ca_file.as_ref().map(Into::into),
        proxy: config.network.proxy.clone(),
    };
    let client = pmt_net::client(&outbound).ok()?;
    // Without the session's sandboxed commands (its sandbox could not be
    // set up) command providers still run confined, or are refused: never
    // through a plain shell with the parent's environment.
    let runner: Arc<dyn pmt_media::speech::command::CommandRunner> = match inputs.commands {
        Some(commands) => Arc::new(SandboxRunner(commands.clone())),
        None => Arc::new(ProfileRunner(scope.clone())),
    };
    let session = MediaSession::new(MediaSetup {
        credentials: credentials(scope),
        client,
        fetcher: inputs.fetcher?,
        host: Arc::new(BlobHost {
            store: Arc::clone(inputs.store),
            session: inputs.session.to_string(),
        }),
        profile_dir: scope.dir().to_path_buf(),
        workspace: inputs.cwd.to_path_buf(),
        surface: inputs.surface,
        endpoints: std::collections::BTreeMap::new(),
        usage: Some(usage_sink(inputs.store, inputs.session)),
        video: inputs.video,
        runner,
        local: Some(launcher(scope)),
        plugins: inputs.plugins,
    })
    .with_ffmpeg(ffmpeg(scope));
    Some(MediaSessions::process().attach(&inputs.session.to_string(), Arc::new(session)))
}

/// Fetches what the media tools' surface of `session_id` depends on (the
/// live OpenRouter image catalog) before the surface is built.
pub async fn prime(session_id: &str) {
    if let Some(media) = pmt_media::MediaSessions::process().session(session_id) {
        media.prime().await;
    }
}

/// The surface name a session's media tools see: the platform of a
/// gateway conversation, else the session's source.
#[must_use]
pub fn surface_of(session: &pmt_store::Session) -> String {
    match &session.origin {
        pmt_store::Origin::Gateway { platform, .. } => platform.clone(),
        _ => session.source.as_str().to_owned(),
    }
}

/// The web tools' tool-gateway route (when the profile is signed in or
/// has a self-hosted gateway token) and the `xai` backend's credential
/// (the xAI sign-in or API key per the subscription rule), read at each
/// call.
#[must_use]
pub fn web_credentials(
    scope: &ProfileScope,
) -> (
    Option<pmt_tools::web::GatewayRoute>,
    Option<pmt_tools::web::TokenLookup>,
) {
    use pmt_providers::auth::gateway::{Category, ToolRouting};
    let media = credentials(scope);
    let gateway = media
        .gateway(Category::WebSearch)
        .ok()
        .and_then(|access| {
            Some(pmt_tools::web::GatewayRoute {
                url: access.gateway_url?,
                token: access.token?,
                billing_source: access.billing_source,
            })
        })
        .or_else(|| {
            // A web category routed elsewhere still reaches the gateway
            // when `nous` is named explicitly for the other capability.
            let routing = pmt_providers::auth::gateway::Routing {
                config: &media.config,
                secrets: media.secrets.as_ref(),
                extras: media.extras.as_ref(),
            };
            match routing.route(Category::WebExtract) {
                Ok(ToolRouting::Gateway {
                    url,
                    token,
                    billing_source,
                }) => Some(pmt_tools::web::GatewayRoute {
                    url,
                    token,
                    billing_source,
                }),
                _ => None,
            }
        });
    let xai: pmt_tools::web::TokenLookup = Arc::new(move || {
        let registry =
            pmt_providers::registry::ProviderRegistry::for_profile(&media.config, Vec::new())
                .ok()?;
        pmt_providers::auth::tools::tool_credential(
            &registry,
            media.secrets.as_ref(),
            media.extras.as_ref(),
            "xai",
        )
        .ok()
        .flatten()
        .map(|found| (found.token, found.billing_source))
    });
    (gateway, Some(xai))
}
