//! A local release origin for the `prometheus update` tests (spec:
//! distribution-and-update): a copy of the built binary is "installed"
//! with a receipt, and a wiremock server plays the release origin
//! (`update.mirror_url`), serving channel indexes, manifests and archives
//! signed with the test key that debug builds trust.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use pmt_config::{Home, Launch, Overrides, ProfileName, ProfileScope};
use pmt_store::{NewSession, Source, Store};
use pmt_update::manifest::{
    Artifact, ChannelIndex, MANIFEST_SCHEMA, ReleaseDetails, ReleaseManifest,
};
use pmt_update::{Channel, ReleaseVersion};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub const TEST_KEY: &str = include_str!("../../../pmt-update/tests/fixtures/test.key");

pub struct Fixture {
    pub dir: tempfile::TempDir,
    pub home: PathBuf,
    pub binary: PathBuf,
    pub server: MockServer,
    pub runtime: tokio::runtime::Runtime,
    pub target: String,
    pub version: ReleaseVersion,
}

impl Fixture {
    /// An install owned by `owner` (`None`: no receipt, a source build).
    pub fn new(owner: Option<&str>) -> Self {
        let dir = super::temp_home();
        let home = dir.path().join("home");
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let binary = bin_dir.join(if cfg!(windows) {
            "prometheus.exe"
        } else {
            "prometheus"
        });
        std::fs::copy(super::BIN, &binary).unwrap();
        let info: serde_json::Value =
            serde_json::from_str(&super::ok(&super::run(&home, &["version", "--json"]))).unwrap();
        let target = info["target"].as_str().unwrap().to_owned();
        let version = ReleaseVersion::parse(info["version"].as_str().unwrap()).unwrap();
        if let Some(owner) = owner {
            let receipt = serde_json::json!({
                "owner": owner, "channel": "stable", "version": version.to_string(),
                "target": target, "installed_at": "2026-10-31T00:00:00Z", "binary": binary,
            });
            std::fs::write(bin_dir.join("install.json"), receipt.to_string()).unwrap();
        }
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let server = runtime.block_on(MockServer::start());
        std::fs::write(
            home.join("daemon.toml"),
            format!("[update]\nmirror_url = \"{}\"\n", server.uri()),
        )
        .unwrap();
        Self {
            dir,
            home,
            binary,
            server,
            runtime,
            target,
            version,
        }
    }

    pub fn self_install() -> Self {
        Self::new(Some("self"))
    }

    /// Runs the installed copy.
    pub fn run(&self, args: &[&str]) -> Output {
        let mut cmd = Command::new(&self.binary);
        super::isolate_user_home(&mut cmd, &self.home);
        cmd.arg("--home").arg(&self.home).args(args);
        cmd.stdin(Stdio::null()).output().unwrap()
    }

    /// Appends `text` to `daemon.toml` (which already sets the mirror).
    pub fn daemon(&self, text: &str) {
        let path = self.home.join("daemon.toml");
        let mut current = std::fs::read_to_string(&path).unwrap();
        current.push_str(text);
        std::fs::write(path, current).unwrap();
    }

    pub fn bytes(&self) -> Vec<u8> {
        std::fs::read(&self.binary).unwrap()
    }

    /// Publishes release `tag` on `channel` with `binary` in its archive;
    /// `served` replaces the archive bytes on the wire (after signing).
    pub fn publish(&self, tag: &str, channel: Channel, binary: &[u8], served: Option<Vec<u8>>) {
        self.publish_with(tag, channel, binary, served, ReleaseDetails::default());
    }

    /// [`Self::publish`] with release notes, schemas and pins.
    pub fn publish_with(
        &self,
        tag: &str,
        channel: Channel,
        binary: &[u8],
        served: Option<Vec<u8>>,
        details: ReleaseDetails,
    ) {
        let version = ReleaseVersion::parse(tag).unwrap();
        let archive = archive(binary);
        let artifact = Artifact::of(&self.target, &archive);
        let manifest = ReleaseManifest {
            schema: MANIFEST_SCHEMA.to_owned(),
            version,
            commit: "fixture".to_owned(),
            published_at: "2026-10-31T00:00:00Z".to_owned(),
            artifacts: vec![artifact.clone()],
            details,
        };
        let index = serde_json::to_vec(&ChannelIndex::new(channel, version)).unwrap();
        self.serve(&format!("/channels/{channel}.json"), index);
        self.serve(
            &format!("/{tag}/release-manifest.json"),
            manifest.to_published_json().into_bytes(),
        );
        let path = format!("/{tag}/{}", artifact.name);
        self.mount(&format!("{path}.minisig"), sign(&archive).into_bytes());
        self.mount(&path, served.unwrap_or(archive));
    }

    /// Serves `bytes` and their signature at `path`.
    pub fn serve(&self, path: &str, bytes: Vec<u8>) {
        self.mount(&format!("{path}.minisig"), sign(&bytes).into_bytes());
        self.mount(path, bytes);
    }

    pub fn mount(&self, at: &str, body: Vec<u8>) {
        self.runtime.block_on(
            Mock::given(method("GET"))
                .and(path(at))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
                .mount(&self.server),
        );
    }

    pub fn requests(&self) -> Vec<wiremock::Request> {
        self.runtime
            .block_on(self.server.received_requests())
            .unwrap_or_default()
    }

    pub fn latest_receipt(&self) -> serde_json::Value {
        let text = std::fs::read_to_string(self.home.join("updates/receipts/latest.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    pub fn prev(&self) -> PathBuf {
        pmt_update::swap::prev_path(&self.binary)
    }

    pub fn scope(&self, profile: &str) -> ProfileScope {
        let home = Home::open(&self.home).unwrap();
        let dir = home.profile(&ProfileName::parse(profile).unwrap()).unwrap();
        ProfileScope::open(dir, &Overrides::default(), Launch::Other).unwrap()
    }

    pub fn create_session(&self, title: &str) {
        let store = Store::open(&self.scope("default")).unwrap();
        let session = store.create_session(&NewSession::new(Source::Cli)).unwrap();
        store
            .set_title(session.id, Some(title), pmt_store::TitleSource::User)
            .unwrap();
    }
}

/// `tar.gz` holding `prometheus-<target>/prometheus` as dist lays it out.
pub fn archive(binary: &[u8]) -> Vec<u8> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut tar = tar::Builder::new(gz);
    let mut header = tar::Header::new_gnu();
    header.set_size(binary.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    let name = if cfg!(windows) {
        "prometheus.exe"
    } else {
        "prometheus"
    };
    tar.append_data(&mut header, format!("prometheus-x/{name}"), binary)
        .unwrap();
    tar.into_inner().unwrap().finish().unwrap()
}

pub fn sign(data: &[u8]) -> String {
    let sk = minisign::SecretKeyBox::from_string(TEST_KEY)
        .unwrap()
        .into_unencrypted_secret_key()
        .unwrap();
    minisign::sign(None, &sk, data, None, None)
        .unwrap()
        .into_string()
}

pub fn code(out: &Output) -> i32 {
    out.status.code().unwrap()
}

pub fn next_patch(v: ReleaseVersion) -> String {
    let text = v.to_string();
    let (head, patch) = text.rsplit_once('.').unwrap();
    format!("{head}.{}", patch.parse::<u64>().unwrap() + 1)
}
