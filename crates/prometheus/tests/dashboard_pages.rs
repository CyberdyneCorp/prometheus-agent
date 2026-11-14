//! The M3 dashboard pages' APIs (spec: web-dashboard, Webhooks page;
//! Channel setup forms; Files page; Embedded documentation; Profile
//! builder; Dashboard themes).

#![cfg(feature = "dashboard")]

mod common;

use serde_json::{Value, json};

use common::dashboard::{self, Signed, call, get, sign_in};
use common::{daemon, mock_home, ok, run, run_with_stdin};

struct Open {
    port: u16,
    signed: Signed,
}

fn open(home: &std::path::Path) -> Open {
    let opened = dashboard::open(home, &[]);
    let signed = sign_in(&opened);
    Open {
        port: opened.port,
        signed,
    }
}

impl Open {
    fn get(&self, path: &str) -> common::api::Answer {
        get(self.port, path, &self.signed.cookie)
    }

    fn call(&self, method: &str, path: &str, body: &Value) -> common::api::Answer {
        call(self.port, method, path, &self.signed, Some(body))
    }
}

/// Scenario: Symlink escape.
#[cfg(unix)]
#[test]
fn files_outside_the_workspace_root_are_refused_and_never_listed() {
    let home = mock_home();
    let workspace = home.path().join("profiles/default/workspace");
    std::fs::create_dir_all(workspace.join("docs")).unwrap();
    std::fs::write(workspace.join("notes.txt"), "hello files").unwrap();
    std::os::unix::fs::symlink("/etc", workspace.join("etc")).unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let at = open(home.path());
    let listing = at.get("/api/profiles/default/files/list?root=workspace&path=");
    assert_eq!(listing.status, 200, "{listing:?}");
    let names: Vec<String> = listing.json()["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        names,
        ["docs", "notes.txt"],
        "the link out of the root is not listed"
    );
    for path in [
        "etc",
        "etc/hosts",
        "../config.toml",
        "docs/../../config.toml",
    ] {
        let list = at.get(&format!(
            "/api/profiles/default/files/list?root=workspace&path={path}"
        ));
        let read = at.get(&format!(
            "/api/profiles/default/files/read?root=workspace&path={path}"
        ));
        assert!(
            list.status == 403 || read.status == 403,
            "{path}: {list:?} {read:?}"
        );
        assert!(!read.body.contains("localhost"), "{path}: {}", read.body);
    }
    let preview = at.get("/api/profiles/default/files/read?root=workspace&path=notes.txt");
    assert_eq!(
        (preview.status, preview.body.as_str()),
        (200, "hello files")
    );
    let renamed = at.call(
        "POST",
        "/api/profiles/default/files/rename?root=workspace",
        &json!({"path": "notes.txt", "to": "renamed.txt"}),
    );
    assert_eq!(renamed.status, 200, "{renamed:?}");
    let unconfirmed = call(
        at.port,
        "DELETE",
        "/api/profiles/default/files?root=workspace&path=renamed.txt",
        &at.signed,
        None,
    );
    assert_eq!(unconfirmed.status, 409);
    let deleted = call(
        at.port,
        "DELETE",
        "/api/profiles/default/files?root=workspace&path=renamed.txt&confirm=true",
        &at.signed,
        None,
    );
    assert_eq!(deleted.status, 200, "{deleted:?}");
    assert!(!workspace.join("renamed.txt").exists());
}

/// The Files page applies a session's file-access rules inside its root
/// (spec: web-dashboard, Files page): a session started in the user's
/// home offers that home as a root, but its credential stores and dotenv
/// files answer 403 to drivers and observers alike and are not listed.
#[cfg(unix)]
#[test]
fn a_session_root_in_the_users_home_never_serves_its_credentials() {
    let home = mock_home();
    let user = common::user_home(home.path());
    std::fs::create_dir_all(user.join(".ssh")).unwrap();
    std::fs::write(user.join(".ssh/id_ed25519"), "PRIVATE KEY").unwrap();
    std::fs::create_dir_all(user.join(".aws")).unwrap();
    std::fs::write(user.join(".aws/credentials"), "aws_secret").unwrap();
    std::fs::write(user.join(".env"), "API_KEY=hunter2").unwrap();
    std::fs::write(user.join("notes.txt"), "home notes").unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let at = open(home.path());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let created = rt.block_on(async {
        let mut ws = dashboard::rpc(at.port, &at.signed).await;
        let frame = json!({"jsonrpc": "2.0", "id": 1, "method": "session.create",
            "params": {"profile": "default", "cwd": user}});
        common::ws::request_on(&mut ws, &frame).await
    });
    let session = created["result"]["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();
    let root = format!("session:{session}");
    let roots = at.get("/api/profiles/default/files").json();
    assert!(
        roots["roots"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == root.as_str()),
        "{roots}"
    );
    let read = |path: &str| format!("/api/profiles/default/files/read?root={root}&path={path}");
    let notes = at.get(&read("notes.txt"));
    assert_eq!((notes.status, notes.body.as_str()), (200, "home notes"));
    let listing = at.get(&format!(
        "/api/profiles/default/files/list?root={root}&path="
    ));
    assert_eq!(listing.status, 200, "{listing:?}");
    for hidden in [".ssh", ".aws", ".env"] {
        assert!(
            !listing.body.contains(&format!("\"{hidden}\"")),
            "{hidden}: {}",
            listing.body
        );
    }
    let paired = ok(&run(
        home.path(),
        &[
            "--json",
            "daemon",
            "pair-device",
            "viewer",
            "--role",
            "observer",
        ],
    ));
    let token = serde_json::from_str::<Value>(&paired).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_owned();
    for path in [".ssh/id_ed25519", ".aws/credentials", ".env"] {
        let driver = at.get(&read(path));
        let observer = common::api::Request::new("GET", &read(path))
            .header("Authorization", &format!("Bearer {token}"))
            .send(at.port);
        for answer in [driver, observer] {
            assert_eq!(answer.status, 403, "{path}: {answer:?}");
            for secret in ["PRIVATE KEY", "aws_secret", "hunter2"] {
                assert!(!answer.body.contains(secret), "{path}: {}", answer.body);
            }
        }
    }
}

/// Scenario: Offline docs (the documentation is compiled in; nothing is
/// fetched).
#[test]
fn the_embedded_docs_find_a_config_keys_reference_entry() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let at = open(home.path());
    let found = at.get("/api/docs/search?q=busy.input_mode");
    assert_eq!(found.status, 200, "{found:?}");
    let first = &found.json()["hits"][0];
    assert_eq!(first["key"], "busy.input_mode", "{first}");
    assert_eq!(first["path"], "reference/configuration.md");
    assert!(first["text"].as_str().unwrap().contains("queue"), "{first}");
    let page = at.get("/api/docs/page?path=guide/dashboard.md");
    assert_eq!(page.status, 200);
    assert!(
        page.json()["markdown"]
            .as_str()
            .unwrap()
            .starts_with("# The web dashboard")
    );
}

fn secret_names(home: &std::path::Path, profile: &str) -> Vec<String> {
    let listed = ok(&run(home, &["-p", profile, "--json", "secrets", "list"]));
    let doc: Value = serde_json::from_str(&listed).unwrap();
    doc["secrets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["name"].as_str().map(str::to_owned))
        .collect()
}

/// Scenario: Clone without secrets (and a failed build creates nothing).
#[test]
fn the_profile_builder_clones_configuration_only() {
    let home = mock_home();
    ok(&run_with_stdin(
        home.path(),
        &["secrets", "set", "SERVICE_TOKEN"],
        "t0p\n",
    ));
    ok(&run(
        home.path(),
        &["memory", "add", "The deploy window is Tuesday."],
    ));
    ok(&run(
        home.path(),
        &["config", "set", "busy.input_mode", "steer"],
    ));
    let _daemon = daemon::start(home.path(), &[]);
    let at = open(home.path());
    let built = at.call(
        "POST",
        "/api/profiles",
        &json!({"name": "writer", "clone_from": "default", "description": "drafts", "soul": "I write."}),
    );
    assert_eq!(built.status, 201, "{built:?}");
    let profile = home.path().join("profiles/writer");
    let config = std::fs::read_to_string(profile.join("config.toml")).unwrap();
    assert!(config.contains("input_mode = \"steer\""), "{config}");
    assert_eq!(
        std::fs::read_to_string(profile.join("SOUL.md")).unwrap(),
        "I write."
    );
    assert!(secret_names(home.path(), "default").contains(&"SERVICE_TOKEN".to_owned()));
    assert!(!secret_names(home.path(), "writer").contains(&"SERVICE_TOKEN".to_owned()));
    let memories = ok(&run(
        home.path(),
        &["-p", "writer", "--json", "memory", "list", "--all"],
    ));
    assert!(!memories.contains("Tuesday"), "{memories}");
    let sessions = ok(&run(
        home.path(),
        &["-p", "writer", "--json", "sessions", "list"],
    ));
    assert!(!sessions.contains("\"id\""), "{sessions}");

    let refused = at.call(
        "POST",
        "/api/profiles",
        &json!({"name": "broken", "adapters": ["matrix"]}),
    );
    assert_eq!(refused.status, 400, "{refused:?}");
    assert!(refused.body.contains("homeserver"), "{refused:?}");
    assert!(
        !home.path().join("profiles/broken").exists(),
        "nothing was created"
    );
}

/// Scenario: Required field missing.
#[test]
fn an_adapter_form_without_a_required_field_writes_nothing() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let at = open(home.path());
    let listed = at.get("/api/profiles/default/adapters");
    let matrix = listed.json()["adapters"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["platform"] == "matrix")
        .cloned()
        .unwrap();
    assert!(
        matrix["schema"]["properties"]["homeserver"].is_object(),
        "{matrix}"
    );
    assert_eq!(
        matrix["secrets"][0],
        json!({"name": "MATRIX_ACCESS_TOKEN", "set": false})
    );
    let config = home.path().join("profiles/default/config.toml");
    let before = std::fs::read_to_string(&config).unwrap();
    let saved = at.call(
        "PUT",
        "/api/profiles/default/adapters/matrix",
        &json!({"enabled": true, "options": {}, "secrets": {"MATRIX_ACCESS_TOKEN": "syt_x"}}),
    );
    assert_eq!(saved.status, 400, "{saved:?}");
    assert!(saved.body.contains("homeserver"), "{saved:?}");
    assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
    assert!(!secret_names(home.path(), "default").contains(&"MATRIX_ACCESS_TOKEN".to_owned()));
    let fixed = at.call(
        "PUT",
        "/api/profiles/default/adapters/matrix",
        &json!({"enabled": true, "options": {"homeserver": "https://matrix.example.org"},
                "secrets": {"MATRIX_ACCESS_TOKEN": "syt_x"}}),
    );
    assert_eq!(fixed.status, 200, "{fixed:?}");
    assert!(secret_names(home.path(), "default").contains(&"MATRIX_ACCESS_TOKEN".to_owned()));
}

/// Scenario: Theme imports remote CSS. Scenario: Oversized custom CSS.
#[test]
fn rejected_themes_are_listed_with_their_reason_and_never_served() {
    let home = mock_home();
    let dir = home.path().join("themes/dashboard");
    std::fs::create_dir_all(&dir).unwrap();
    let base = "palette = { background = \"#000000\", midground = \"#111111\", foreground = \"#ffffff\" }\n";
    std::fs::write(
        dir.join("remote.toml"),
        format!("{base}custom_css = \"@import url(https://x.example/a.css);\"\n"),
    )
    .unwrap();
    std::fs::write(
        dir.join("huge.toml"),
        format!("{base}custom_css = {:?}\n", "a{}".repeat(14_000)),
    )
    .unwrap();
    std::fs::write(dir.join("calm.toml"), base).unwrap();
    let _daemon = daemon::start(home.path(), &[]);
    let at = open(home.path());
    let themes = at.get("/api/themes").json();
    let of = |name: &str| {
        themes["themes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == name)
            .cloned()
            .unwrap()
    };
    assert!(
        of("remote")["error"].as_str().unwrap().contains("@import"),
        "{themes}"
    );
    assert!(
        of("huge")["error"].as_str().unwrap().contains("32 KiB"),
        "{themes}"
    );
    assert_eq!(of("calm")["valid"], true);
    for builtin in [
        "default",
        "default-large",
        "midnight",
        "ember",
        "mono",
        "cyberpunk",
        "rose",
    ] {
        assert_eq!(of(builtin)["valid"], true, "{builtin}");
    }
    assert_eq!(at.get("/api/themes/remote/css").status, 422);
    let css = at.get("/api/themes/midnight/css");
    assert_eq!(css.status, 200);
    assert!(css.body.contains("--bg: #0b1020ff"), "{}", css.body);
}

/// Scenario: Secret shown once (over the API: only the creation answer
/// carries it).
#[test]
fn a_webhook_secret_is_answered_once() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let at = open(home.path());
    let created = at.call(
        "POST",
        "/api/profiles/default/webhooks",
        &json!({"name": "deploys", "description": "CI", "events": "push", "deliver": "log"}),
    );
    assert_eq!(created.status, 200, "{created:?}");
    let secret = created.json()["secret"].as_str().unwrap().to_owned();
    assert!(secret.len() >= 32, "{created:?}");
    let listed = at.get("/api/profiles/default/webhooks");
    assert_eq!(listed.status, 200);
    assert!(!listed.body.contains(&secret), "{}", listed.body);
    assert_eq!(listed.json()["subscriptions"][0]["description"], "CI");
    let disabled = at.call(
        "PATCH",
        "/api/profiles/default/webhooks/deploys",
        &json!({"enabled": false}),
    );
    assert_eq!(disabled.status, 200, "{disabled:?}");
    assert_eq!(
        at.get("/api/profiles/default/webhooks").json()["subscriptions"][0]["enabled"],
        false
    );
}
