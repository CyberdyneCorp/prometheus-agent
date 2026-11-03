//! `prometheus plugins install|search|show|update|adopt|validate|pack|sign`
//! against local packages and a wiremock catalog (spec: plugin-system,
//! Plugin catalog; Catalog kill list; Signed plugin packages and
//! provenance; plugins CLI; Capability grants are user-approved and
//! digest-bound; security-boundaries, Signed plugins). Live catalogs are
//! signed with the test key debug builds trust.

mod common;

use std::path::{Path, PathBuf};
use std::process::Output;

use self::common::{ok, run, stderr, stdout, temp_home};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_KEY: &str = include_str!("../../pmt-update/tests/fixtures/test.key");

fn sign_with(secret: &str, data: &[u8]) -> String {
    let key = minisign::SecretKeyBox::from_string(secret)
        .unwrap()
        .into_unencrypted_secret_key()
        .unwrap();
    minisign::sign(None, &key, data, None, None)
        .unwrap()
        .into_string()
}

/// A plugin directory `weather` (sample tool) at `version` asking for
/// `capabilities`, outside any profile.
fn source(root: &Path, version: &str, capabilities: &str) -> PathBuf {
    let dir = root.join(format!("src-{version}/weather"));
    pmt_plugins::testkit::install("sample-tool", &dir);
    std::fs::write(
        dir.join("plugin.toml"),
        format!(
            "manifest_version = 1\nid = \"weather\"\nversion = \"{version}\"\nkind = \"wasm\"\n\
             description = \"Forecasts\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
             worlds = [\"tool@1.0\"]\n\n[capabilities]\n{capabilities}\n"
        ),
    )
    .unwrap();
    dir
}

/// Packs `dir` into `out` with the CLI; answers the package digest.
fn pack(home: &Path, dir: &Path, out: &Path) -> String {
    let printed = ok(&run(
        home,
        &[
            "plugins",
            "pack",
            dir.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ],
    ));
    printed.split_whitespace().nth(1).unwrap().to_owned()
}

fn code(out: &Output) -> i32 {
    out.status.code().unwrap()
}

fn record(home: &Path, id: &str) -> String {
    std::fs::read_to_string(
        home.join("profiles/default/plugins")
            .join(id)
            .join(".install.toml"),
    )
    .unwrap()
}

fn profile_log(home: &Path) -> String {
    std::fs::read_to_string(home.join("profiles/default/logs/prometheus.log")).unwrap_or_default()
}

#[test]
fn an_unsigned_local_package_needs_an_explicit_flag() {
    // Spec scenario "Unsigned local package needs an explicit flag".
    let home = temp_home();
    let package = home.path().join("weather.pmtpkg");
    let digest = pack(
        home.path(),
        &source(home.path(), "1.0.0", "network = [\"api.weather.example\"]"),
        &package,
    );
    let out = run(
        home.path(),
        &["plugins", "install", package.to_str().unwrap()],
    );
    assert_eq!(code(&out), 77, "{}", stderr(&out));
    assert!(
        stderr(&out)
            .contains("package is unsigned; re-run with --allow-unsigned to install anyway"),
        "{}",
        stderr(&out)
    );
    assert!(
        !home
            .path()
            .join("profiles/default/plugins/weather")
            .exists()
    );
    let out = run(
        home.path(),
        &[
            "plugins",
            "install",
            package.to_str().unwrap(),
            "--allow-unsigned",
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    // The screen names the plugin, its digest and what it asks for.
    assert!(
        stderr(&out).contains("connect to api.weather.example"),
        "{}",
        stderr(&out)
    );
    let recorded = record(home.path(), "weather");
    assert!(recorded.contains("source = \"package\""), "{recorded}");
    assert!(recorded.contains("allow_unsigned = true"), "{recorded}");
    assert!(recorded.contains(&digest), "{recorded}");
    // Installed is not enabled.
    let listing = ok(&run(home.path(), &["plugins", "list"]));
    assert!(
        listing.contains("weather  1.0.0  wasm  user  disabled"),
        "{listing}"
    );
    let doctor = run(home.path(), &["plugins", "doctor"]);
    assert!(
        stdout(&doctor).contains("weather: unsigned"),
        "{}",
        stdout(&doctor)
    );
}

#[test]
fn a_symbolic_ref_is_refused() {
    // Spec scenario "Symbolic ref refused".
    let home = temp_home();
    let out = run(
        home.path(),
        &[
            "plugins",
            "install",
            "https://github.com/x/y",
            "--ref",
            "main",
        ],
    );
    assert_eq!(code(&out), 2, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("--ref must be a full commit SHA"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn a_git_install_pins_the_commit_it_names() {
    let home = temp_home();
    let repo = source(home.path(), "1.0.0", "");
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.org"])
            .args(args)
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    git(&["init", "--quiet"]);
    git(&["add", "."]);
    git(&["commit", "--quiet", "-m", "v1"]);
    let sha = git(&["rev-parse", "HEAD"]);
    let url = format!("file://{}", repo.display());
    let out = run(
        home.path(),
        &[
            "plugins",
            "install",
            &url,
            "--ref",
            &sha,
            "--allow-unsigned",
            "--no-enable",
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let recorded = record(home.path(), "weather");
    assert!(recorded.contains("source = \"git\""), "{recorded}");
    assert!(recorded.contains(&sha), "{recorded}");
    assert!(
        !home
            .path()
            .join("profiles/default/plugins/weather/.git")
            .exists()
    );
}

/// A wiremock catalog server and the home pointing at it.
struct CatalogServer {
    runtime: tokio::runtime::Runtime,
    server: MockServer,
}

impl CatalogServer {
    fn start(home: &Path) -> Self {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let server = runtime.block_on(MockServer::start());
        ok(&run(
            home,
            &[
                "config",
                "set",
                "plugins.catalog_url",
                &format!("{}/catalog.json", server.uri()),
            ],
        ));
        Self { runtime, server }
    }

    fn serve(&self, at: &str, body: Vec<u8>) {
        self.runtime.block_on(
            Mock::given(method("GET"))
                .and(path(at))
                .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
                .mount(&self.server),
        );
    }

    /// Serves `catalog` signed by `secret`.
    fn catalog(&self, catalog: &serde_json::Value, secret: &str) {
        let bytes = catalog.to_string().into_bytes();
        let signature = sign_with(secret, &bytes);
        self.serve("/catalog.json", bytes);
        self.serve("/catalog.json.minisig", signature.into_bytes());
    }

    fn url(&self, at: &str) -> String {
        format!("{}{at}", self.server.uri())
    }
}

/// A catalog entry for `weather` at package `digest`.
fn entry(server: &CatalogServer, digest: &str, publisher: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "weather",
        "repo": "https://github.com/acme/weather",
        "ref": digest,
        "tier": "community",
        "category": "tools",
        "capabilities_summary": "network api.weather.example",
        "publisher_key": publisher,
        "package": server.url("/weather.pmtpkg"),
        "description": "Forecasts for the agent",
        "version": "1.0.0",
    })
}

#[test]
fn a_tampered_catalog_is_ignored_for_the_embedded_one() {
    // Spec scenario "Tampered catalog is ignored".
    let home = temp_home();
    let server = CatalogServer::start(home.path());
    let publisher = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let catalog = serde_json::json!({
        "plugins": [entry(&server, &format!("sha256:{}", "a".repeat(64)), &publisher.pk.to_base64())]
    });
    let stranger = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let stranger_secret = stranger.sk.to_box(None).unwrap().to_string();
    server.catalog(&catalog, &stranger_secret);
    let found = ok(&run(home.path(), &["plugins", "search", "weather"]));
    assert_eq!(found, "no catalog plugins match\n");
    assert!(
        profile_log(home.path()).contains("signature verification failed"),
        "{}",
        profile_log(home.path())
    );
    // Signed with a trusted key, the same catalog is used.
    let fresh = temp_home();
    let trusted = CatalogServer::start(fresh.path());
    trusted.catalog(&catalog, TEST_KEY);
    let found = ok(&run(fresh.path(), &["plugins", "search", "weather"]));
    assert!(found.starts_with("weather  community  tools"), "{found}");
}

#[test]
fn an_unsigned_catalog_install_is_refused_by_default() {
    // Verification of 28.8: a catalog install needs a valid signature
    // unless --allow-unsigned is given (and recorded); with one it installs.
    let home = temp_home();
    let server = CatalogServer::start(home.path());
    let package = home.path().join("weather.pmtpkg");
    let digest = pack(home.path(), &source(home.path(), "1.0.0", ""), &package);
    let publisher = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let publisher_secret = publisher.sk.to_box(None).unwrap().to_string();
    server.catalog(
        &serde_json::json!({ "plugins": [entry(&server, &digest, &publisher.pk.to_base64())] }),
        TEST_KEY,
    );
    server.serve("/weather.pmtpkg", std::fs::read(&package).unwrap());
    let out = run(
        home.path(),
        &["plugins", "install", "weather", "--no-enable"],
    );
    assert_eq!(code(&out), 77, "{}", stderr(&out));
    assert!(
        stderr(&out)
            .contains("package is unsigned; re-run with --allow-unsigned to install anyway"),
        "{}",
        stderr(&out)
    );
    assert!(
        !home
            .path()
            .join("profiles/default/plugins/weather")
            .exists()
    );

    // A signature by another key is invalid: refused even with the flag.
    let stranger = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
    let forged = sign_with(
        &stranger.sk.to_box(None).unwrap().to_string(),
        digest.as_bytes(),
    );
    server.serve("/weather.pmtpkg.minisig", forged.into_bytes());
    let out = run(
        home.path(),
        &[
            "plugins",
            "install",
            "weather",
            "--allow-unsigned",
            "--no-enable",
        ],
    );
    assert_eq!(code(&out), 77, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("signature verification failed"),
        "{}",
        stderr(&out)
    );

    // Signed by the publisher (`plugins sign`), it installs and records it.
    let key_file = home.path().join("publisher.key");
    std::fs::write(&key_file, &publisher_secret).unwrap();
    ok(&run(
        home.path(),
        &[
            "plugins",
            "sign",
            package.to_str().unwrap(),
            "--key",
            key_file.to_str().unwrap(),
        ],
    ));
    let fresh = temp_home();
    let signed = CatalogServer::start(fresh.path());
    signed.catalog(
        &serde_json::json!({ "plugins": [entry(&signed, &digest, &publisher.pk.to_base64())] }),
        TEST_KEY,
    );
    signed.serve("/weather.pmtpkg", std::fs::read(&package).unwrap());
    signed.serve(
        "/weather.pmtpkg.minisig",
        std::fs::read(pmt_plugins::package::signature_path(&package)).unwrap(),
    );
    let out = run(
        fresh.path(),
        &["plugins", "install", "weather", "--no-enable"],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let recorded = record(fresh.path(), "weather");
    assert!(recorded.contains("source = \"catalog\""), "{recorded}");
    assert!(recorded.contains("tier = \"community\""), "{recorded}");
    assert!(recorded.contains("signer = "), "{recorded}");
    assert!(recorded.contains("allow_unsigned = false"), "{recorded}");
    let shown = ok(&run(fresh.path(), &["plugins", "show", "weather"]));
    assert!(shown.contains("catalog: community tier"), "{shown}");
}

#[test]
fn a_removed_plugin_stops_loading_and_is_not_installed() {
    // Spec scenario "Removed plugin stops loading".
    let home = temp_home();
    let package = home.path().join("weather.pmtpkg");
    pack(home.path(), &source(home.path(), "1.0.0", ""), &package);
    let installed = run(
        home.path(),
        &[
            "plugins",
            "install",
            package.to_str().unwrap(),
            "--allow-unsigned",
            "--enable",
        ],
    );
    assert_eq!(code(&installed), 0, "{}", stderr(&installed));
    let listing = ok(&run(home.path(), &["plugins", "list"]));
    assert!(
        listing.contains("weather  1.0.0  wasm  user  loaded"),
        "{listing}"
    );
    let server = CatalogServer::start(home.path());
    server.catalog(
        &serde_json::json!({
            "plugins": [],
            "removed": [{ "id": "weather", "reason": "exfiltrates tokens" }]
        }),
        TEST_KEY,
    );
    // A catalog command refreshes the cache the kill list reads.
    ok(&run(home.path(), &["plugins", "search", "anything"]));
    let listing = ok(&run(home.path(), &["plugins", "list"]));
    assert!(
        listing.contains(
            "Plugin 'weather' was removed from the catalog and is blocked: exfiltrates tokens"
        ),
        "{listing}"
    );
    let out = run(home.path(), &["plugins", "enable", "weather"]);
    assert_eq!(code(&out), 77, "{}", stderr(&out));
}

#[test]
fn a_hand_placed_plugin_needs_adoption() {
    // Spec scenario "Hand-placed plugin needs adoption".
    let home = temp_home();
    let placed = home.path().join("profiles/default/plugins/notes");
    pmt_plugins::testkit::install("sample-tool", &placed);
    std::fs::write(
        placed.join("plugin.toml"),
        "manifest_version = 1\nid = \"notes\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
         description = \"Notes\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
         worlds = [\"tool@1.0\"]\n",
    )
    .unwrap();
    ok(&run(
        home.path(),
        &["config", "set", "plugins.enabled", "[\"notes\"]"],
    ));
    let listing = ok(&run(home.path(), &["plugins", "list"]));
    assert!(
        listing.contains("notes  1.0.0  wasm  user  unadopted"),
        "{listing}"
    );
    ok(&run(
        home.path(),
        &["plugins", "adopt", placed.to_str().unwrap()],
    ));
    assert!(record(home.path(), "notes").contains("source = \"adopted\""));
    let listing = ok(&run(home.path(), &["plugins", "list"]));
    assert!(
        listing.contains("notes  1.0.0  wasm  user  loaded"),
        "{listing}"
    );
}

#[test]
fn validate_checks_manifest_worlds_and_capabilities() {
    let home = temp_home();
    let good = source(home.path(), "1.0.0", "network = [\"api.weather.example\"]");
    let out = ok(&run(
        home.path(),
        &["plugins", "validate", good.to_str().unwrap()],
    ));
    assert!(
        out.contains("weather 1.0.0 (wasm) is valid; worlds: tool@1.0.0"),
        "{out}"
    );
    let bad = source(home.path(), "2.0.0", "netwrok = [\"x\"]");
    let out = run(home.path(), &["plugins", "validate", bad.to_str().unwrap()]);
    assert_eq!(code(&out), 65, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("unknown capability 'netwrok'"),
        "{}",
        stderr(&out)
    );
    let wrong_world = source(home.path(), "3.0.0", "");
    let manifest = wrong_world.join("plugin.toml");
    let text = std::fs::read_to_string(&manifest)
        .unwrap()
        .replace("tool@1.0", "hook@1.0");
    std::fs::write(&manifest, text).unwrap();
    let out = run(
        home.path(),
        &["plugins", "validate", wrong_world.to_str().unwrap()],
    );
    assert_eq!(code(&out), 65, "{}", stderr(&out));
}

#[test]
fn capability_escalation_on_update_waits_for_approval() {
    // Spec scenario "Capability escalation on update waits for approval".
    let home = temp_home();
    let package = home.path().join("weather.pmtpkg");
    pack(
        home.path(),
        &source(home.path(), "1.0.0", "network = [\"api.weather.example\"]"),
        &package,
    );
    let out = run(
        home.path(),
        &[
            "plugins",
            "install",
            package.to_str().unwrap(),
            "--allow-unsigned",
            "--enable",
            "--grant",
            "all",
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    ok(&run(
        home.path(),
        &["config", "set", "plugins.signature_policy", "off"],
    ));
    pack(
        home.path(),
        &source(
            home.path(),
            "1.1.0",
            "network = [\"api.weather.example\"]\nsecrets = [\"GITHUB_TOKEN\"]",
        ),
        &package,
    );
    let checked = ok(&run(home.path(), &["plugins", "check-updates"]));
    assert!(checked.starts_with("weather: package"), "{checked}");
    let out = run(home.path(), &["plugins", "update", "weather"]);
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("weather 1.1.0 requests new capabilities: secrets GITHUB_TOKEN"),
        "{}",
        stderr(&out)
    );
    let listing = ok(&run(home.path(), &["plugins", "list"]));
    assert!(
        listing.contains("weather  1.0.0  wasm  user  loaded"),
        "{listing}"
    );
    let diff = ok(&run(home.path(), &["plugins", "diff-grants", "weather"]));
    assert!(diff.contains("new      secrets GITHUB_TOKEN"), "{diff}");
    assert!(
        diff.contains("granted  network api.weather.example"),
        "{diff}"
    );
    ok(&run(
        home.path(),
        &["plugins", "update", "weather", "--grant", "all"],
    ));
    let listing = ok(&run(home.path(), &["plugins", "list"]));
    assert!(
        listing.contains("weather  1.1.0  wasm  user  loaded"),
        "{listing}"
    );
    let capabilities = ok(&run(home.path(), &["plugins", "capabilities", "weather"]));
    assert!(
        capabilities.contains("granted: network api.weather.example, secrets GITHUB_TOKEN"),
        "{capabilities}"
    );
    ok(&run(home.path(), &["plugins", "remove", "weather"]));
    let listing = ok(&run(home.path(), &["plugins", "list"]));
    assert_eq!(listing, "no plugins found\n");
}

#[test]
fn a_tampered_plugin_does_not_load_and_doctor_reports_it() {
    // security-boundaries scenario "Tampered plugin".
    let home = temp_home();
    let package = home.path().join("weather.pmtpkg");
    pack(home.path(), &source(home.path(), "1.0.0", ""), &package);
    let out = run(
        home.path(),
        &[
            "plugins",
            "install",
            package.to_str().unwrap(),
            "--allow-unsigned",
            "--enable",
        ],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let component = home
        .path()
        .join("profiles/default/plugins/weather/plugin.wasm");
    let mut bytes = std::fs::read(&component).unwrap();
    bytes.push(0);
    std::fs::write(&component, bytes).unwrap();
    let listing = ok(&run(home.path(), &["plugins", "list"]));
    assert!(
        listing.contains(
            "weather  1.0.0  wasm  user  error  plugin 'weather' was modified after install"
        ),
        "{listing}"
    );
    let doctor = run(home.path(), &["doctor", "plugins"]);
    assert_eq!(code(&doctor), 1, "{}", stdout(&doctor));
    assert!(
        stdout(&doctor).contains("weather: plugin 'weather' was modified after install"),
        "{}",
        stdout(&doctor)
    );
}
