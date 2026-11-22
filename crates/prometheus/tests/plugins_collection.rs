//! Plugin collections, `PyPI` plugin installs and Nix-declared plugins
//! (spec: plugin-system, Plugin collections; Python package and Nix
//! plugin sources): `prometheus plugins collection show|install|export`
//! against local Git repositories, `plugins install pypi:...` against a
//! local package index, and `plugins nix-sync` with a module manifest.

mod common;

use std::path::{Path, PathBuf};
use std::time::Duration;

use expectrl::{Eof, Expect};

use self::common::{ok, run, stderr, stdout, temp_home};

/// A Git repository holding the sample tool as plugin `weather` under
/// `subdir` (the root when empty); answers its URL and commit.
fn repository(root: &Path, subdir: &str) -> (String, String) {
    let repo = root.join("repo");
    let dir = if subdir.is_empty() {
        repo.clone()
    } else {
        repo.join(subdir)
    };
    pmt_plugins::testkit::install("sample-tool", &dir);
    std::fs::write(
        dir.join("plugin.toml"),
        "manifest_version = 1\nid = \"weather\"\nversion = \"1.0.0\"\nkind = \"wasm\"\n\
         description = \"Forecasts\"\nlicense = \"MIT\"\nrequires_prometheus = \"*\"\n\
         worlds = [\"tool@1.0\"]\n\n[capabilities]\nnetwork = [\"api.weather.example\"]\n",
    )
    .unwrap();
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
    (format!("file://{}", repo.display()), sha)
}

fn write(home: &Path, name: &str, text: &str) -> PathBuf {
    let path = home.join(name);
    std::fs::write(&path, text).unwrap();
    path
}

fn code(out: &std::process::Output) -> i32 {
    out.status.code().unwrap()
}

#[test]
fn a_tag_ref_is_refused() {
    // Spec scenario "Tag ref refused".
    let home = temp_home();
    let file = write(
        home.path(),
        "prometheus-collection.toml",
        "[[plugins]]\nname = \"weather\"\nref = \"v1.2.0\"\n",
    );
    for action in ["install", "show"] {
        let out = run(
            home.path(),
            &["plugins", "collection", action, file.to_str().unwrap()],
        );
        assert_eq!(code(&out), 65, "{}", stderr(&out));
        assert!(
            stderr(&out).contains("entry 'weather': ref must be a full commit SHA"),
            "{}",
            stderr(&out)
        );
    }
}

#[test]
fn secrets_and_grants_never_travel_in_a_collection() {
    let home = temp_home();
    let sha = "0123456789abcdef0123456789abcdef01234567";
    for (key, expected) in [
        ("api_token", "looks like a secret"),
        ("grants", "collections cannot carry grants"),
    ] {
        let file = write(
            home.path(),
            "c.toml",
            &format!(
                "[[plugins]]\nname = \"weather\"\nref = \"{sha}\"\n[plugins.config]\n{key} = \"x\"\n"
            ),
        );
        let out = run(
            home.path(),
            &["plugins", "collection", "install", file.to_str().unwrap()],
        );
        assert_eq!(code(&out), 65, "{}", stderr(&out));
        assert!(stderr(&out).contains(expected), "{}", stderr(&out));
    }
}

#[test]
fn a_collection_installs_only_after_a_confirmation_on_a_terminal() {
    let home = temp_home();
    let (url, sha) = repository(home.path(), "");
    let file = write(
        home.path(),
        "c.toml",
        &format!("[[plugins]]\nrepo = \"{url}\"\nref = \"{sha}\"\n"),
    );
    let out = run(
        home.path(),
        &["plugins", "collection", "install", file.to_str().unwrap()],
    );
    assert_eq!(code(&out), 4, "{}", stderr(&out));
    assert!(
        !home
            .path()
            .join("profiles/default/plugins/weather")
            .exists(),
        "nothing installs without the confirmation"
    );
}

#[test]
fn show_reviews_source_ref_and_capabilities_of_a_hermes_pack() {
    let home = temp_home();
    let (url, sha) = repository(home.path(), "plugins/weather");
    let file = write(
        home.path(),
        "hermes-pack.yaml",
        &format!(
            "name: starter\nplugins:\n  - repo: \"{url}\"\n    subdir: plugins/weather\n    ref: {sha}\nconfig:\n  weather:\n    units: metric\nskills:\n  - github-pr\n"
        ),
    );
    let out = run(
        home.path(),
        &["plugins", "collection", "show", file.to_str().unwrap()],
    );
    let review = ok(&out);
    assert!(
        review.contains("Collection 'starter': 1 plugin(s)"),
        "{review}"
    );
    assert!(
        review.contains(&format!("source git {url}#plugins/weather")),
        "{review}"
    );
    assert!(review.contains(&format!("ref {sha}")), "{review}");
    assert!(review.contains("plugin 'weather' 1.0.0 (wasm)"), "{review}");
    assert!(review.contains("api.weather.example"), "{review}");
    assert!(review.contains("settings seeded: units"), "{review}");
    assert!(review.contains("skills: github-pr"), "{review}");
    assert!(
        !home
            .path()
            .join("profiles/default/plugins/weather")
            .exists()
    );
}

#[cfg(unix)]
#[test]
fn an_interactive_install_runs_the_normal_flow_per_plugin_and_reports_failures() {
    let home = temp_home();
    ok(&run(
        home.path(),
        &["config", "set", "plugins.signature_policy", "off"],
    ));
    ok(&run(
        home.path(),
        &["config", "set", "plugins.entries.weather.units", "imperial"],
    ));
    let (url, sha) = repository(home.path(), "");
    let missing = format!("file://{}", home.path().join("nowhere").display());
    let file = write(
        home.path(),
        "c.toml",
        &format!(
            "[[plugins]]\nrepo = \"{url}\"\nref = \"{sha}\"\n[plugins.config]\nunits = \"metric\"\nlang = \"pt\"\n\n\
             [[plugins]]\nrepo = \"{missing}\"\nref = \"{sha}\"\n"
        ),
    );
    let mut p = common::pty::spawn(
        home.path(),
        &["plugins", "collection", "install", file.to_str().unwrap()],
    );
    p.set_expect_timeout(Some(Duration::from_mins(2)));
    p.expect("Install this collection? [y/N]").unwrap();
    p.send_line("y").unwrap();
    p.expect("Install it anyway? [y/N]").unwrap();
    p.send_line("y").unwrap();
    p.expect("Enable 'weather' now? [y/N]").unwrap();
    p.send_line("n").unwrap();
    p.expect("not installed: ").unwrap();
    p.expect(Eof).unwrap();
    let status = p.get_process_mut().wait().unwrap();
    assert!(format!("{status:?}").ends_with(", 5)"), "{status:?}");
    let record = std::fs::read_to_string(
        home.path()
            .join("profiles/default/plugins/weather/.install.toml"),
    )
    .unwrap();
    assert!(record.contains("source = \"git\""), "{record}");
    assert!(record.contains(&sha), "{record}");
    let config = std::fs::read_to_string(home.path().join("profiles/default/config.toml")).unwrap();
    assert!(
        config.contains("units = \"imperial\""),
        "the user's value wins: {config}"
    );
    assert!(config.contains("lang = \"pt\""), "{config}");

    // Export pins the commit and strips secret-shaped settings.
    ok(&run(
        home.path(),
        &["config", "set", "plugins.entries.weather.api_key", "s3cret"],
    ));
    let exported = ok(&run(home.path(), &["plugins", "collection", "export"]));
    assert!(
        exported.contains(&format!("repo = \"{url}\"")),
        "{exported}"
    );
    assert!(exported.contains(&format!("ref = \"{sha}\"")), "{exported}");
    assert!(exported.contains("units = \"imperial\""), "{exported}");
    assert!(!exported.contains("api_key"), "{exported}");
    let enabled_only = ok(&run(
        home.path(),
        &["plugins", "collection", "export", "--enabled-only"],
    ));
    assert!(!enabled_only.contains("weather"), "{enabled_only}");
}

#[test]
fn an_unpinned_pypi_plugin_is_refused() {
    // Spec scenario "Unpinned PyPI plugin refused".
    let home = temp_home();
    let out = run(home.path(), &["plugins", "install", "pypi:hermes-spotify"]);
    assert_eq!(code(&out), 2, "{}", stderr(&out));
    assert!(
        stderr(&out).contains("pypi plugins need an exact version: pypi:<dist>==<version>"),
        "{}",
        stderr(&out)
    );
}

const SPOTIFY: &str = "def register(ctx):\n    ctx.register_command('spotify', lambda args: 'playing', description='Play music')\n";

#[test]
fn a_pypi_plugin_installs_its_entry_point_as_a_python_plugin() {
    let home = temp_home();
    pmt_pybridge::testkit::seed(home.path());
    let index = home.path().join("index");
    pmt_pybridge::testkit::index(
        &index,
        &[(
            "hermes-spotify",
            "1.2.0",
            &[
                ("hermes_spotify/__init__.py", SPOTIFY),
                (
                    "hermes_spotify-1.2.0.dist-info/entry_points.txt",
                    "[hermes_agent.plugins]\nspotify = hermes_spotify:register\n",
                ),
            ],
        )],
    );
    let served = pmt_pybridge::testkit::serve(&index);
    let out = common::command(home.path())
        .args([
            "plugins",
            "install",
            "pypi:hermes-spotify==1.2.0",
            "--allow-unsigned",
            "--no-enable",
        ])
        .env("UV_DEFAULT_INDEX", &served.url)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let dir = home.path().join("profiles/default/plugins/spotify");
    let record = std::fs::read_to_string(dir.join(".install.toml")).unwrap();
    assert!(record.contains("source = \"pypi\""), "{record}");
    assert!(
        record.contains("location = \"pypi:hermes-spotify==1.2.0\""),
        "{record}"
    );
    assert!(dir.join("site/hermes_spotify/__init__.py").is_file());
    let manifest = std::fs::read_to_string(dir.join("plugin.yaml")).unwrap();
    assert!(manifest.contains("\"hermes-spotify==1.2.0\""), "{manifest}");
    let list = stdout(&run(home.path(), &["plugins", "list"]));
    assert!(list.contains("spotify"), "{list}");
    assert!(list.contains("python"), "{list}");
}

#[test]
fn nix_declared_plugins_are_materialized_and_removed_without_touching_user_plugins() {
    let home = temp_home();
    let (url, sha) = repository(home.path(), "");
    ok(&run(
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
    ));
    let source = home.path().join("repo");
    let manifest = write(
        home.path(),
        "nix.json",
        &serde_json::json!({"plugins": [{"id": "forecast", "source": source}]}).to_string(),
    );
    let out = run(
        home.path(),
        &["plugins", "nix-sync", manifest.to_str().unwrap()],
    );
    assert_eq!(code(&out), 0, "{}", stderr(&out));
    let managed = home
        .path()
        .join("profiles/default/plugins/nix-managed-forecast");
    let record = std::fs::read_to_string(managed.join(".install.toml")).unwrap();
    assert!(record.contains("source = \"nix\""), "{record}");
    let toml = std::fs::read_to_string(managed.join("plugin.toml")).unwrap();
    assert!(toml.contains("id = \"nix-managed-forecast\""), "{toml}");
    let list = stdout(&run(home.path(), &["plugins", "list"]));
    assert!(list.contains("nix-managed-forecast"), "{list}");
    assert!(!list.contains("nix-managed-forecast  error"), "{list}");

    // Unchanged: left alone. Dropped: removed; the user's plugin stays.
    let again = run(
        home.path(),
        &["plugins", "nix-sync", manifest.to_str().unwrap()],
    );
    assert!(!stderr(&again).contains("installed"), "{}", stderr(&again));
    let empty = write(home.path(), "nix.json", "{\"plugins\": []}");
    ok(&run(
        home.path(),
        &["plugins", "nix-sync", empty.to_str().unwrap()],
    ));
    assert!(!managed.exists());
    assert!(
        home.path()
            .join("profiles/default/plugins/weather")
            .is_dir()
    );
}
