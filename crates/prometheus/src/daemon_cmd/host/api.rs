//! What the API server needs from the daemon host (spec: api-server): API
//! sessions (`source = "api"`, caller instructions and session key kept
//! beside them), turns admitted behind a running one with their
//! per-request route, model options and approval decision, and the
//! profile's key. Jobs, catalogs and readiness live in `api_profile.rs`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use pmt_agent::turn::model::ProviderModel;
use pmt_api::{ApiImage, ApiTurn, ModelChoice, NewApiSession};
use pmt_config::ProfileScope;
use pmt_config::schema::{BusyInputMode, FastMode, ReasoningLevel, UnattendedApproval};
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use pmt_gateway::{Author, HostFuture, Link};
use pmt_providers::resolve::Target;
use pmt_rpc::contracts::{PromptImage, SessionSetup, SubmitParams, Submitted};
use pmt_store::{NewMessage, NewSession, Role, Source, Store};
use serde_json::Value;

use super::{DaemonHost, Loaded, Profile, session_id};

/// How long a looked-up API key is reused before the secrets store is
/// read again.
const KEY_TTL: Duration = Duration::from_secs(10);
/// The largest remote image fetched for a turn.
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

/// A key per (profile, secret name), with when it was read.
type Keys = HashMap<(String, String), (Instant, Option<SecretString>)>;

/// The profiles' keys as last read.
#[derive(Default)]
pub(super) struct KeyCache {
    keys: Mutex<Keys>,
}

/// What an API request changes for its own turn only.
pub(super) struct ApiSetup {
    /// The route of this turn's model calls.
    pub route: Option<(Target, Arc<ProviderModel>)>,
    pub reasoning: Option<ReasoningLevel>,
    pub fast: Option<FastMode>,
    pub unattended: Option<UnattendedApproval>,
    pub additions: Vec<String>,
}

/// What [`ApiSetup::begin`] changed, put back when the turn ends.
pub(super) struct Applied {
    /// The session's own `/fast` choice, when the request replaced it.
    fast: Option<FastBefore>,
    unattended: bool,
    route: bool,
}

/// The session's `/fast` override before a request's `service_tier`.
struct FastBefore(Option<FastMode>);

impl ApiSetup {
    /// Applies the setup to `session` for the turn about to run; the route
    /// is a `--once` switch (the session's prompt and tools stay as they
    /// are) that the turn's end puts back.
    pub(super) fn begin(&self, session: &Loaded) -> Applied {
        let live = &session.live;
        if let Some((target, port)) = &self.route {
            live.agent.keep_prompt(true);
            live.model.switch_once(target.clone(), Arc::clone(port));
            if self.reasoning.is_some() {
                live.model.set_reasoning(self.reasoning);
            }
        }
        let fast = self.fast.map(|mode| {
            let before = FastBefore(live.agent.fast_override());
            live.agent.set_fast_mode(Some(mode));
            before
        });
        if self.unattended.is_some() {
            live.gate.set_turn_unattended(self.unattended);
        }
        Applied {
            fast,
            unattended: self.unattended.is_some(),
            route: self.route.is_some(),
        }
    }
}

impl Applied {
    /// Puts back what the turn changed.
    pub(super) fn end(self, session: &Loaded) {
        if let Some(FastBefore(before)) = self.fast {
            session.live.agent.set_fast_mode(before);
        }
        if self.unattended {
            session.live.gate.set_turn_unattended(None);
        }
        if self.route {
            session.live.agent.keep_prompt(false);
        }
    }
}

fn reasoning(text: Option<&str>) -> Result<Option<ReasoningLevel>, Error> {
    let Some(text) = text else {
        return Ok(None);
    };
    serde_json::from_value(Value::String(text.to_ascii_lowercase()))
        .map(Some)
        .map_err(|_| {
            Error::new(
                &codes::CLI_USAGE,
                format!("unknown reasoning_effort '{text}'"),
            )
        })
}

/// `service_tier`: `priority`/`fast` ask for the fast tier, `default`
/// for the normal one; other tiers keep the session's mode.
fn fast(tier: Option<&str>) -> Option<FastMode> {
    match tier? {
        "priority" | "fast" => Some(FastMode::Fast),
        "default" | "standard" => Some(FastMode::Normal),
        other => FastMode::parse(other),
    }
}

impl DaemonHost {
    fn api_workspace(profile: &Profile) -> Result<PathBuf, Error> {
        let cwd = Self::cwd_of(profile, None);
        std::fs::create_dir_all(&cwd).map_err(|e| Error::io(&codes::HOME_IO, &cwd, &e))?;
        Ok(cwd)
    }

    async fn create_api_session(
        &self,
        profile: &Profile,
        new: NewApiSession,
    ) -> Result<String, Error> {
        let cwd = Self::api_workspace(profile)?;
        let setup = SessionSetup::default();
        let factory = self.factory(profile, cwd.clone(), &setup)?;
        let target = factory.models.primary(&super::setup::flags(&setup)).await?;
        let mut row = NewSession::new(Source::Api);
        row.provider = Some(target.provider.clone());
        row.model.clone_from(&target.model);
        row.cwd = Some(cwd.to_string_lossy().into_owned());
        if let Some(title) = &new.title {
            row.title = pmt_store::titles::sanitize(title)?;
            row.title_source = pmt_store::TitleSource::User;
        }
        let session = profile.store.create_session(&row)?;
        let history: Vec<NewMessage> = new
            .history
            .iter()
            .filter(|(_, text)| !text.is_empty())
            .map(|(role, text)| {
                let role = if role == "assistant" {
                    Role::Assistant
                } else {
                    Role::User
                };
                NewMessage::new(role, text.clone())
            })
            .collect();
        if !history.is_empty() {
            profile.store.append_messages(session.id, &history)?;
        }
        profile.store.put_api_session(&pmt_store::api::ApiSession {
            session_id: session.id,
            instructions: new.instructions,
            session_key: new.session_key,
            derived_key: new.derived_key,
        })?;
        Ok(session.id.to_string())
    }

    /// The image files of a turn: data URLs written under the profile's
    /// cache, remote URLs fetched under the profile's URL policy.
    async fn api_images(
        scope: &ProfileScope,
        images: &[ApiImage],
    ) -> Result<Vec<PromptImage>, Error> {
        let dir = scope.dir().join("cache").join("api-images");
        let mut paths = Vec::new();
        for image in images {
            let (bytes, media_type) = match image {
                ApiImage::Data { media_type, bytes } => (bytes.clone(), media_type.clone()),
                ApiImage::Url(url) => fetch_image(scope, url).await?,
            };
            let ext = pmt_api::image_extension(&media_type).ok_or_else(|| {
                Error::new(
                    &codes::CHAT_IMAGE,
                    format!("unsupported image type {media_type}"),
                )
            })?;
            let name = format!("{}.{ext}", pmt_store::BlobRef::of(&bytes).sha256());
            std::fs::create_dir_all(&dir).map_err(|e| Error::io(&codes::HOME_IO, &dir, &e))?;
            let path = dir.join(name);
            pmt_core::private_fs::write_atomic(&path, &bytes)
                .map_err(|e| Error::io(&codes::HOME_IO, &path, &e))?;
            paths.push(PromptImage::Path(path.to_string_lossy().into_owned()));
        }
        Ok(paths)
    }

    async fn api_setup(&self, profile: &Profile, turn: &ApiTurn) -> Result<ApiSetup, Error> {
        let reasoning = reasoning(turn.options.reasoning_effort.as_deref())?;
        let factory = self.factory(
            profile,
            Self::api_workspace(profile)?,
            &SessionSetup::default(),
        )?;
        let target = match &turn.model {
            Some(choice) => Some(Target {
                provider: choice.provider.clone(),
                model: choice.model.clone(),
                base_url: None,
                credential: None,
            }),
            // A reasoning effort alone still needs a route of its own for
            // the turn, so the session's level is left as it is.
            None if reasoning.is_some() => {
                Some(self.session_target(profile, &turn.session_id).await?)
            }
            None => None,
        };
        let route = match target {
            Some(target) => {
                let port = factory.models.port(&target).await?;
                Some((target, port))
            }
            None => None,
        };
        Ok(ApiSetup {
            route,
            reasoning,
            fast: fast(turn.options.service_tier.as_deref()),
            unattended: turn.unattended,
            additions: turn.additions.clone(),
        })
    }

    async fn session_target(&self, profile: &Profile, session: &str) -> Result<Target, Error> {
        let id = session_id(session)?;
        if let Some(loaded) = profile.loaded.lock().await.get(&id) {
            return Ok(loaded.live.model.target());
        }
        let row = profile.store.session(id)?;
        Ok(Target {
            provider: row.provider.unwrap_or_default(),
            model: row.model,
            base_url: None,
            credential: None,
        })
    }

    fn api_key(&self, profile: &str, name: &str) -> Option<SecretString> {
        let key = (profile.to_owned(), name.to_owned());
        let mut keys = self.api_keys.keys.lock().expect("keys poisoned");
        if let Some((at, value)) = keys.get(&key)
            && at.elapsed() < KEY_TTL
        {
            return value.clone();
        }
        let value = self
            .profile(profile)
            .ok()
            .and_then(|p| p.scope().secrets().resolve(name).ok());
        keys.insert(key, (Instant::now(), value.clone()));
        value
    }
}

async fn fetch_image(scope: &ProfileScope, url: &str) -> Result<(Vec<u8>, String), Error> {
    let config = scope.config();
    let outbound = pmt_net::OutboundPolicy {
        force_ipv4: config.network.force_ipv4,
        extra_ca_file: config.network.extra_ca_file.as_ref().map(Into::into),
        proxy: config.network.proxy.clone(),
    };
    let egress = &config.security.egress;
    let hosts = crate::egress::host_policy(egress, &crate::egress::Overrides::default());
    let fetcher = pmt_net::fetch::Fetcher::new(&outbound, crate::egress::destinations(egress)?)?
        .with_hosts(Arc::new(hosts), "api_server", None);
    let failed = |why: String| Error::new(&codes::CHAT_IMAGE, format!("image {url}: {why}"));
    let response = fetcher.get(url).await?;
    if !response.status().is_success() {
        return Err(failed(format!("HTTP {}", response.status().as_u16())));
    }
    let media_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("image/png")
        .split(';')
        .next()
        .unwrap_or("image/png")
        .trim()
        .to_owned();
    if pmt_api::image_extension(&media_type).is_none() {
        return Err(failed(format!(
            "not a PNG, JPEG, GIF or WebP image ({media_type})"
        )));
    }
    let bytes = read_capped(response, MAX_IMAGE_BYTES)
        .await
        .map_err(failed)?;
    Ok((bytes, media_type))
}

/// The body of `response`, refused as soon as it is known to exceed `cap`
/// bytes: a server streaming without end never makes the daemon buffer
/// more than `cap`.
async fn read_capped(mut response: reqwest::Response, cap: usize) -> Result<Vec<u8>, String> {
    let too_large = || format!("larger than {} MiB", cap / (1024 * 1024));
    if response
        .content_length()
        .is_some_and(|length| length > cap as u64)
    {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        if bytes.len() + chunk.len() > cap {
            return Err(too_large());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

impl pmt_api::ApiHost for DaemonHost {
    fn api_scope(&self, profile: &str) -> Option<ProfileScope> {
        self.profile(profile).ok().map(|p| p.scope())
    }

    fn api_store(&self, profile: &str) -> Option<Arc<Store>> {
        self.profile(profile).ok().map(|p| Arc::clone(&p.store))
    }

    fn api_secret(&self, profile: &str, name: &str) -> Option<SecretString> {
        self.api_key(profile, name)
    }

    fn api_create_session<'a>(
        &'a self,
        profile: &'a str,
        new: NewApiSession,
    ) -> HostFuture<'a, String> {
        Box::pin(async move {
            let profile = self.profile(profile)?;
            self.create_api_session(&profile, new).await
        })
    }

    fn api_submit<'a>(
        &'a self,
        profile: &'a str,
        turn: ApiTurn,
        author: Author,
        link: Link,
    ) -> HostFuture<'a, Submitted> {
        let span = super::session_span(profile, &turn.session_id);
        Box::pin(tracing::Instrument::instrument(
            async move {
                let profile = self.profile(profile)?;
                let images = Self::api_images(&profile.scope(), &turn.images).await?;
                let setup = self.api_setup(&profile, &turn).await?;
                let params = SubmitParams {
                    session_id: turn.session_id,
                    text: turn.text,
                    images,
                    files: Vec::new(),
                    run_budget_secs: None,
                    input_id: None,
                    author: None,
                };
                self.submit_turn(
                    &profile,
                    params,
                    (author, link),
                    Some(BusyInputMode::Queue),
                    Some(setup),
                )
                .await
            },
            span,
        ))
    }

    fn api_check_route<'a>(
        &'a self,
        profile: &'a str,
        session: Option<&'a str>,
        route: &'a ModelChoice,
    ) -> HostFuture<'a, bool> {
        Box::pin(async move {
            let profile = self.profile(profile)?;
            let factory = self.factory(
                &profile,
                Self::api_workspace(&profile)?,
                &SessionSetup::default(),
            )?;
            let target = Target {
                provider: route.provider.clone(),
                model: route.model.clone(),
                base_url: None,
                credential: None,
            };
            factory
                .models
                .resolver()
                .resolve_target(&target, &mut pmt_providers::resolve::Trace::default())
                .await?;
            let Some(session) = session.and_then(|s| session_id(s).ok()) else {
                return Ok(true);
            };
            let overridden = profile
                .loaded
                .lock()
                .await
                .get(&session)
                .is_some_and(|loaded| loaded.live.model.session().overridden());
            Ok(!overridden)
        })
    }

    fn api_jobs<'a>(
        &'a self,
        profile: &'a str,
        call: pmt_api::JobsCall,
    ) -> HostFuture<'a, (u16, Value)> {
        Box::pin(async move {
            let profile = self.profile(profile)?;
            Ok(super::api_profile::jobs(&self.launch, &profile, &call))
        })
    }

    fn api_catalog<'a>(
        &'a self,
        profile: &'a str,
        what: pmt_api::Catalog,
    ) -> HostFuture<'a, Value> {
        Box::pin(async move {
            let profile = self.profile(profile)?;
            super::api_profile::catalog(self, &profile, what)
        })
    }

    fn api_health<'a>(&'a self, profile: &'a str) -> HostFuture<'a, Value> {
        Box::pin(async move {
            let profile = self.profile(profile)?;
            Ok(super::api_profile::health(self, &profile).await)
        })
    }

    /// Forced redaction (spec: api-server, Run events stream): the
    /// profile's known secrets whatever `security.redact_secrets` says,
    /// plus its PII rules, plus credential-shaped patterns.
    fn api_redact(&self, profile: &str, text: &str) -> String {
        let redactor = self
            .profile(profile)
            .ok()
            .and_then(|p| forced_redactor(&p.scope()).ok())
            .unwrap_or_default();
        redactor.redact_all(text)
    }
}

/// The profile's known secrets (always) merged with its configured
/// redaction (PII when `privacy.redact_pii` is set).
fn forced_redactor(scope: &ProfileScope) -> Result<pmt_core::secret::Redactor, Error> {
    let secrets = scope.secrets().redactor()?;
    Ok(match pmt_obs::redaction::profile_redactor(scope)? {
        Some(profile) => secrets.merged(&profile),
        None => secrets,
    })
}

#[cfg(test)]
mod tests {
    use pmt_config::schema::{FastMode, ReasoningLevel};

    #[test]
    fn model_options_parse() {
        assert_eq!(
            super::reasoning(Some("High")).unwrap(),
            Some(ReasoningLevel::High)
        );
        assert!(super::reasoning(Some("loud")).is_err());
        assert_eq!(super::fast(Some("priority")), Some(FastMode::Fast));
        assert_eq!(super::fast(Some("default")), Some(FastMode::Normal));
        assert_eq!(super::fast(None), None);
    }

    /// A local image server answering one request with `head` and then
    /// `body` repeated until the client hangs up; its URL.
    async fn image_server(head: &'static str, body: &'static [u8]) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        pmt_core::spawn::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            if socket.write_all(head.as_bytes()).await.is_err() {
                return;
            }
            if body.is_empty() {
                // Holds the connection until the client hangs up.
                let _ = socket.read(&mut request).await;
                return;
            }
            while socket.write_all(body).await.is_ok() {}
        });
        format!("http://{addr}/huge.png")
    }

    async fn fetch(url: &str) -> reqwest::Response {
        let policy = pmt_net::OutboundPolicy::default();
        let client = pmt_net::http::client_builder_loopback_direct(&policy)
            .unwrap()
            .build()
            .unwrap();
        client.get(url).send().await.unwrap()
    }

    /// A remote image larger than the cap is refused while it streams: the
    /// buffer never grows past the cap.
    #[tokio::test]
    async fn an_endless_image_is_refused_once_it_passes_the_cap() {
        static CHUNK: [u8; 64 * 1024] = [7; 64 * 1024];
        let url = image_server(
            "HTTP/1.1 200 OK\r\ncontent-type: image/png\r\nconnection: close\r\n\r\n",
            &CHUNK,
        )
        .await;
        let response = fetch(&url).await;
        let refused = super::read_capped(response, super::MAX_IMAGE_BYTES)
            .await
            .unwrap_err();
        assert_eq!(refused, "larger than 20 MiB");
    }

    /// A declared length over the cap is refused before any body is read.
    #[tokio::test]
    async fn a_declared_oversized_image_is_refused_up_front() {
        let url = image_server(
            "HTTP/1.1 200 OK\r\ncontent-type: image/png\r\ncontent-length: 4294967296\r\n\r\n",
            b"",
        )
        .await;
        let response = fetch(&url).await;
        let refused = super::read_capped(response, super::MAX_IMAGE_BYTES)
            .await
            .unwrap_err();
        assert_eq!(refused, "larger than 20 MiB");
    }

    #[test]
    fn file_extensions_come_from_the_allowlist_only() {
        assert_eq!(pmt_api::image_extension("image/JPEG"), Some("jpg"));
        for refused in [
            "image/../../x",
            "image/png:stream",
            "image/svg+xml",
            "text/plain",
        ] {
            assert_eq!(pmt_api::image_extension(refused), None, "{refused}");
        }
    }
}
