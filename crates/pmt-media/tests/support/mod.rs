//! A media session against local fakes: secrets and sign-ins in memory,
//! media written under a temp profile, providers reached through endpoint
//! overrides on loopback mocks.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pmt_config::Config;
use pmt_core::error::Error;
use pmt_core::secret::SecretString;
use pmt_media::plugin::{MediaPlugins, NoPlugins};
use pmt_media::speech::command::PlainShell;
use pmt_media::speech::local::Launcher;
use pmt_media::{Credentials, DirHost, MediaSession, MediaSetup};
use pmt_net::{DestinationPolicy, Fetcher, OutboundPolicy};
use pmt_providers::resolve::{CredentialExtras, SecretSource, SignedIn};
use pmt_tools::usage::ToolUsage;

/// Secrets `NAME` = `NAME-value` for each listed name.
pub(crate) struct Secrets(pub(crate) Vec<String>);

impl SecretSource for Secrets {
    fn secret(&self, name: &str) -> Result<Option<SecretString>, Error> {
        Ok(self
            .0
            .iter()
            .any(|n| n == name)
            .then(|| SecretString::new(format!("{name}-value"))))
    }

    fn names(&self) -> Result<Vec<String>, Error> {
        Ok(self.0.clone())
    }
}

/// A sign-in (label `personal`, token `<provider>-token`) to each listed
/// provider.
pub(crate) struct Signins(pub(crate) Vec<String>);

impl CredentialExtras for Signins {
    fn subscription(&self, provider: &str) -> Result<Option<SignedIn>, Error> {
        Ok(self.0.iter().any(|p| p == provider).then(|| SignedIn {
            label: "personal".to_owned(),
            token: SecretString::new(format!("{provider}-token")),
            account_id: None,
            ineligible: false,
            borrowed: None,
        }))
    }
}

/// The session under test.
pub(crate) struct Rig {
    pub(crate) config: Config,
    pub(crate) secrets: Vec<String>,
    pub(crate) signins: Vec<String>,
    pub(crate) endpoints: BTreeMap<String, String>,
    pub(crate) profile: PathBuf,
    pub(crate) surface: String,
    pub(crate) local: Option<Arc<dyn Launcher>>,
    pub(crate) video: Option<Arc<dyn pmt_media::video::VideoDescriber>>,
    pub(crate) usage: Arc<Mutex<Vec<ToolUsage>>>,
    pub(crate) plugins: Arc<dyn MediaPlugins>,
}

impl Rig {
    pub(crate) fn new(config: Config) -> Self {
        Self {
            config,
            secrets: Vec::new(),
            signins: Vec::new(),
            endpoints: BTreeMap::new(),
            profile: tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR"))
                .unwrap()
                .keep(),
            surface: "cli".to_owned(),
            local: None,
            video: None,
            usage: Arc::default(),
            plugins: Arc::new(NoPlugins),
        }
    }

    pub(crate) fn secret(mut self, name: &str) -> Self {
        self.secrets.push(name.to_owned());
        self
    }

    pub(crate) fn signed_in(mut self, provider: &str) -> Self {
        self.signins.push(provider.to_owned());
        self
    }

    pub(crate) fn at(mut self, provider: &str, url: &str) -> Self {
        self.endpoints.insert(provider.to_owned(), url.to_owned());
        self
    }

    pub(crate) fn workspace(&self) -> PathBuf {
        let dir = self.profile.join("work");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    pub(crate) fn media_dir(&self) -> PathBuf {
        self.profile.join("blobs")
    }

    pub(crate) fn session(&self) -> MediaSession {
        let policy = DestinationPolicy::new(true, &[]).unwrap();
        let fetcher = Arc::new(Fetcher::new(&OutboundPolicy::default(), policy).unwrap());
        let usage = Arc::clone(&self.usage);
        MediaSession::new(MediaSetup {
            credentials: Credentials {
                config: Arc::new(self.config.clone()),
                secrets: Arc::new(Secrets(self.secrets.clone())),
                extras: Arc::new(Signins(self.signins.clone())),
            },
            client: pmt_net::client(&OutboundPolicy::default()).unwrap(),
            fetcher,
            host: Arc::new(DirHost(self.media_dir())),
            profile_dir: self.profile.clone(),
            workspace: self.workspace(),
            surface: self.surface.clone(),
            endpoints: self.endpoints.clone(),
            usage: Some(Arc::new(move |u| usage.lock().unwrap().push(u))),
            video: self.video.clone(),
            runner: Arc::new(PlainShell),
            local: self.local.clone(),
            plugins: Arc::clone(&self.plugins),
        })
    }
}

/// The `MEDIA:` path of a result.
pub(crate) fn media_path(text: &str) -> PathBuf {
    let line = text.lines().next().unwrap();
    let path = line
        .strip_prefix("MEDIA:")
        .unwrap_or_else(|| panic!("{text}"));
    PathBuf::from(path.trim_end_matches(" [[as_voice]]"))
}

/// The JSON details line of a media result.
pub(crate) fn details(text: &str) -> serde_json::Value {
    serde_json::from_str(text.lines().nth(1).unwrap()).unwrap()
}

/// An executable script at `dir/name`.
#[cfg(unix)]
pub(crate) fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// A tiny PNG.
pub(crate) fn png() -> Vec<u8> {
    let image = image::RgbImage::new(8, 4);
    let mut out = std::io::Cursor::new(Vec::new());
    image.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}
