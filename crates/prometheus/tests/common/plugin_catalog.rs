//! A live plugin catalog for a test home: a wiremock server the home's
//! `plugins.catalog_url` points at, serving a catalog signed with the test
//! key debug builds trust, and packages signed by a publisher key.

use std::path::Path;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{ok, run};

pub const TEST_KEY: &str = include_str!("../../../pmt-update/tests/fixtures/test.key");

pub fn sign_with(secret: &str, data: &[u8]) -> String {
    let key = minisign::SecretKeyBox::from_string(secret)
        .unwrap()
        .into_unencrypted_secret_key()
        .unwrap();
    minisign::sign(None, &key, data, None, None)
        .unwrap()
        .into_string()
}

pub struct CatalogServer {
    pub runtime: tokio::runtime::Runtime,
    pub server: MockServer,
}

impl CatalogServer {
    /// Starts the server and points `home`'s default profile at it.
    pub fn start(home: &Path) -> Self {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let server = runtime.block_on(MockServer::start());
        let url = format!("{}/catalog.json", server.uri());
        ok(&run(home, &["config", "set", "plugins.catalog_url", &url]));
        Self { runtime, server }
    }

    pub fn serve(&self, at: &str, body: Vec<u8>) {
        self.runtime.block_on(
            Mock::given(method("GET"))
                .and(path(at))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
                .mount(&self.server),
        );
    }

    pub fn url(&self, at: &str) -> String {
        format!("{}{at}", self.server.uri())
    }

    /// The paths asked for so far.
    pub fn asked(&self) -> Vec<String> {
        self.runtime
            .block_on(self.server.received_requests())
            .unwrap_or_default()
            .iter()
            .map(|r| r.url.path().to_owned())
            .collect()
    }

    /// Publishes plugin `weather` (the sample tool, asking for network
    /// access to `api.weather.example`) as a package signed by a fresh
    /// publisher key, in a catalog signed with the test key.
    pub fn publish_weather(&self, home: &Path) {
        let dir = home.join("src/weather");
        pmt_plugins::testkit::install("sample-tool", &dir);
        std::fs::write(
            dir.join("plugin.toml"),
            "manifest_version = 1\nid = \"weather\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
             description = \"Forecasts\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
             worlds = [\"tool@1.0\"]\n\n[capabilities]\nnetwork = [\"api.weather.example\"]\n",
        )
        .unwrap();
        let package = home.join("weather.pmtpkg");
        let printed = ok(&run(
            home,
            &[
                "plugins",
                "pack",
                dir.to_str().unwrap(),
                "--out",
                package.to_str().unwrap(),
            ],
        ));
        let digest = printed.split_whitespace().nth(1).unwrap().to_owned();
        let publisher = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        let key_file = home.join("publisher.key");
        std::fs::write(&key_file, publisher.sk.to_box(None).unwrap().to_string()).unwrap();
        ok(&run(
            home,
            &[
                "plugins",
                "sign",
                package.to_str().unwrap(),
                "--key",
                key_file.to_str().unwrap(),
            ],
        ));
        let catalog = serde_json::json!({"plugins": [{
            "id": "weather",
            "repo": "https://github.com/acme/weather",
            "ref": digest,
            "tier": "community",
            "category": "tools",
            "capabilities_summary": "network api.weather.example",
            "publisher_key": publisher.pk.to_base64(),
            "package": self.url("/weather.pmtpkg"),
            "description": "Forecasts for the agent",
            "version": "1.0.0",
        }]});
        let bytes = catalog.to_string().into_bytes();
        self.serve(
            "/catalog.json.minisig",
            sign_with(TEST_KEY, &bytes).into_bytes(),
        );
        self.serve("/catalog.json", bytes);
        self.serve("/weather.pmtpkg", std::fs::read(&package).unwrap());
        self.serve(
            "/weather.pmtpkg.minisig",
            std::fs::read(pmt_plugins::package::signature_path(&package)).unwrap(),
        );
    }
}
