//! Pets and achievements through the daemon (spec: pets, Pets are
//! cosmetic and cache-neutral; Generating a pet; Affection reactions;
//! Achievements from session history; Achievement unlocks celebrate).

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use base64::Engine as _;
use common::pets::install;
use common::{daemon, mock_home, ok, run};
use pmt_rpc::client::{Client, Inbox};
use serde_json::json;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

async fn session(client: &Client) -> String {
    let created = client.call("session.create", json!({})).await.unwrap();
    created["session_id"].as_str().unwrap().to_owned()
}

/// Submits `text` and waits for its `turn.finished`.
async fn turn(client: &Client, inbox: &mut Inbox, session: &str, text: &str) {
    client
        .call(
            "prompt.submit",
            json!({"session_id": session, "text": text}),
        )
        .await
        .unwrap();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), inbox.events.recv())
            .await
            .expect("a turn finishes within 30 s")
            .expect("the connection is open");
        if event.event == "turn.finished" {
            return;
        }
    }
}

fn db(home: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open_with_flags(
        home.join("profiles/default/state.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap()
}

fn prompt_and_tools(home: &Path, session: &str) -> (String, String) {
    let db = db(home);
    let hash: String = db
        .query_row(
            "SELECT system_prompt_hash FROM sessions WHERE id = ?1",
            [session],
            |r| r.get(0),
        )
        .unwrap();
    let tools: String = db
        .query_row(
            "SELECT tools FROM session_tool_surface WHERE session_id = ?1",
            [session],
            |r| r.get(0),
        )
        .unwrap();
    (hash, tools)
}

/// Scenario: Selecting a pet mid-session.
#[test]
fn selecting_a_pet_mid_session_leaves_the_prompt_and_tools_unchanged() {
    let home = mock_home();
    let _gallery = install(home.path(), &["boba", "mochi"]);
    let _daemon = daemon::start(home.path(), &[]);
    runtime().block_on(async {
        let (client, mut inbox) = daemon::connect(home.path(), "default").await;
        let session = session(&client).await;
        turn(&client, &mut inbox, &session, "first").await;
        let before = prompt_and_tools(home.path(), &session);
        ok(&run(home.path(), &["pets", "select", "boba"]));
        let set = client
            .call(
                "pets.set",
                json!({"profile": "default", "slug": "mochi", "scale": 0.5, "roam": true}),
            )
            .await
            .unwrap();
        assert_eq!(set["active"], "mochi");
        assert_eq!(set["enabled"], true);
        turn(&client, &mut inbox, &session, "second").await;
        assert_eq!(prompt_and_tools(home.path(), &session), before);
    });
    let trajectory = common::files_under(&home.path().join("profiles/default/trajectories"));
    for file in trajectory
        .iter()
        .filter(|f| f.extension().is_some_and(|e| e == "jsonl"))
    {
        let text = std::fs::read_to_string(file).unwrap();
        assert!(
            !text.contains("\"prompt.rebuilt\"") || text.matches("\"prompt.rebuilt\"").count() == 1,
            "{text}"
        );
        assert!(!text.contains("session.tools_rebuilt"), "{text}");
    }
}

#[test]
fn the_desktop_reads_the_state_and_the_atlas() {
    let home = mock_home();
    let _gallery = install(home.path(), &["boba"]);
    let _daemon = daemon::start(home.path(), &[]);
    runtime().block_on(async {
        let (client, _inbox) = daemon::connect(home.path(), "default").await;
        let state = client
            .call("pets.state", json!({"profile": "default"}))
            .await
            .unwrap();
        assert_eq!(state["enabled"], false);
        assert_eq!(state["installed"][0]["slug"], "boba");
        let sprite = client
            .call("pets.sprite", json!({"profile": "default"}))
            .await
            .unwrap();
        assert!(
            sprite["atlas"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert_eq!(sprite["frame_width"], 192);
        assert_eq!(sprite["states"]["idle"], json!({"row": 0, "frames": 6}));
        assert_eq!(sprite["states"]["waiting"], json!({"row": 6, "frames": 6}));
        let clamped = client
            .call("pets.set", json!({"profile": "default", "scale": 9.0}))
            .await
            .unwrap();
        assert_eq!(clamped["scale"], 3.0);
        assert_eq!(clamped["notice"], "scale clamped to 3.0");
        let gallery = client
            .call("pets.gallery", json!({"profile": "default", "query": "bo"}))
            .await
            .unwrap();
        assert_eq!(
            gallery["pets"],
            json!([{"slug": "boba", "name": "BOBA", "author": "tester", "installed": true}])
        );
        let missing = client
            .call("pets.sprite", json!({"profile": "default", "slug": "gone"}))
            .await;
        assert!(
            format!("{missing:?}").contains("pet.not_found"),
            "{missing:?}"
        );
    });
}

/// A PNG strip: a green body on the magenta background the prompts ask
/// for.
fn strip() -> String {
    let image = image::RgbaImage::from_fn(800, 104, |x, _| {
        if x % 100 < 50 {
            image::Rgba([20, 160, 60, 255])
        } else {
            image::Rgba([255, 0, 255, 255])
        }
    });
    let mut png = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    base64::engine::general_purpose::STANDARD.encode(png)
}

/// Scenario: Hatched pet is a normal pet (the desktop generate flow
/// against the OpenAI Images API).
#[test]
fn a_pet_hatched_through_the_daemon_is_installed_like_a_gallery_pet() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let home = mock_home();
    let images = runtime();
    let server = images.block_on(MockServer::start());
    let reply = json!({"data": [{"b64_json": strip()}]});
    for at in ["/v1/images/generations", "/v1/images/edits"] {
        images.block_on(
            Mock::given(method("POST"))
                .and(path(at))
                .respond_with(ResponseTemplate::new(200).set_body_json(reply.clone()))
                .mount(&server),
        );
    }
    for (key, value) in [
        ("image_gen.provider", "openai".to_owned()),
        ("image_gen.openai.base_url", format!("{}/v1", server.uri())),
        ("image_gen.openai.model", "gpt-image-2-low".to_owned()),
    ] {
        ok(&run(home.path(), &["config", "set", key, &value]));
    }
    let mut secret = common::command(home.path());
    secret.args(["secrets", "set", "OPENAI_API_KEY"]);
    common::run_cmd_with_stdin(&mut secret, "sk-test\n");
    let _daemon = daemon::start(home.path(), &[]);
    runtime().block_on(async {
        let (client, _inbox) = daemon::connect(home.path(), "default").await;
        let started = client
            .call(
                "pets.hatch.start",
                json!({"profile": "default", "description": "a sleepy fox"}),
            )
            .await
            .unwrap();
        assert_eq!(started["done"], false);
        assert!(
            started["text"]
                .as_str()
                .unwrap()
                .contains("about $0.04 (4 images with openai/gpt-image-2-low)"),
            "{started}"
        );
        let flow = started["flow"].as_str().unwrap().to_owned();
        let answer = |text: &str| json!({"profile": "default", "flow": flow, "answer": text});
        let drafts = client.call("pets.hatch.answer", answer("y")).await.unwrap();
        assert_eq!(drafts["drafts"].as_array().unwrap().len(), 4, "{drafts}");
        let confirm = client.call("pets.hatch.answer", answer("3")).await.unwrap();
        assert!(
            confirm["text"]
                .as_str()
                .unwrap()
                .contains("9 animation rows from draft 3"),
            "{confirm}"
        );
        let done = client.call("pets.hatch.answer", answer("y")).await.unwrap();
        assert_eq!(done["done"], true, "{done}");
        assert_eq!(done["slug"], "a-sleepy-fox");
        let adopted = client
            .call(
                "pets.set",
                json!({"profile": "default", "slug": "a-sleepy-fox"}),
            )
            .await
            .unwrap();
        assert_eq!(adopted["active"], "a-sleepy-fox");
        let sprite = client
            .call("pets.sprite", json!({"profile": "default"}))
            .await
            .unwrap();
        assert_eq!(sprite["states"]["run"]["frames"], 8);
    });
    let asked = images.block_on(server.received_requests()).unwrap();
    assert_eq!(asked.len(), 4 + 9, "four drafts, then one row per state");
    let edits = asked
        .iter()
        .filter(|r| r.url.path().ends_with("/edits"))
        .count();
    assert_eq!(edits, 9, "every row takes the chosen draft as reference");
    ok(&run(home.path(), &["pets", "doctor"]));
}

/// Scenario: Thank-you message (the submitted message is unchanged).
#[test]
fn an_affectionate_message_is_stored_as_sent() {
    let home = mock_home();
    let _daemon = daemon::start(home.path(), &[]);
    let id = runtime().block_on(async {
        let (client, mut inbox) = daemon::connect(home.path(), "default").await;
        let session = session(&client).await;
        turn(&client, &mut inbox, &session, "thanks, good bot").await;
        session
    });
    let messages = common::session_messages(home.path(), &id);
    let user = messages.iter().find(|m| m["role"] == "user").unwrap();
    assert_eq!(user["content"], "thanks, good bot");
    assert!(pmt_pets::affection::is_affectionate("thanks, good bot"));
}

/// Scenario: First scan does not block.
#[test]
fn the_first_scan_runs_in_the_background() {
    let home = mock_home();
    {
        let scope = common_scope(home.path());
        let store = pmt_store::Store::open(&scope).unwrap();
        for _ in 0..400 {
            let session = store
                .create_session(&pmt_store::NewSession::new(pmt_store::Source::Cli))
                .unwrap();
            for n in 0..5 {
                let text = format!("message {n}");
                store
                    .append_message(
                        session.id,
                        &pmt_store::NewMessage::new(pmt_store::Role::User, text),
                    )
                    .unwrap();
            }
        }
    }
    let _daemon = daemon::start(home.path(), &[]);
    runtime().block_on(async {
        let (client, _inbox) = daemon::connect(home.path(), "default").await;
        let first = client
            .call("achievements.list", json!({"profile": "default"}))
            .await
            .unwrap();
        assert_eq!(first["scanning"], true, "{first}");
        assert_eq!(first["sessions"], 0);
        assert!(first["badges"].as_array().unwrap().len() >= 10);
        let deadline = Instant::now() + Duration::from_mins(1);
        let done = loop {
            let now = client
                .call("achievements.list", json!({"profile": "default"}))
                .await
                .unwrap();
            if now["scanning"] == false && now["sessions"] == 400 {
                break now;
            }
            assert!(Instant::now() < deadline, "{now}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let regular = done["badges"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["id"] == "regular")
            .unwrap()
            .clone();
        assert_eq!(regular["value"], 400);
        assert_eq!(regular["tier"], "silver");
    });
    assert!(
        home.path()
            .join("profiles/default/achievements.json")
            .is_file()
    );
}

fn common_scope(home: &Path) -> pmt_config::ProfileScope {
    let home = pmt_config::Home::open(home).unwrap();
    let profile = home
        .profile(&pmt_config::ProfileName::parse("default").unwrap())
        .unwrap();
    pmt_config::ProfileScope::open(
        profile,
        &pmt_config::Overrides::parse::<&str>(&[]).unwrap(),
        pmt_config::Launch::Other,
    )
    .unwrap()
}

/// An `Etc/GMT` zone in which it is now between 02:00 and 02:59 (a night
/// hour for `night-owl`).
fn night_zone() -> String {
    let hour = i32::from(
        jiff::Timestamp::now()
            .to_zoned(jiff::tz::TimeZone::UTC)
            .hour(),
    );
    let mut offset = (2 - hour).rem_euclid(24);
    if offset > 12 {
        offset -= 24;
    }
    match offset {
        0 => "Etc/GMT".to_owned(),
        // `Etc/GMT-5` is five hours east of UTC.
        east if east > 0 => format!("Etc/GMT-{east}"),
        west => format!("Etc/GMT+{}", -west),
    }
}

/// Scenario: Unlock during a session (the event and the untouched
/// session; the TUI's toast is `pets_tui.rs`).
#[test]
fn a_turn_that_reaches_a_tier_announces_it_and_the_session_never_hears_of_it() {
    let home = mock_home();
    ok(&run(
        home.path(),
        &["config", "set", "cron.timezone", &night_zone()],
    ));
    {
        let store = pmt_store::Store::open(&common_scope(home.path())).unwrap();
        for _ in 0..8 {
            store
                .create_session(&pmt_store::NewSession::new(pmt_store::Source::Cli))
                .unwrap();
        }
    }
    // The first scan records night-owl copper without a celebration.
    let shown = ok(&run(home.path(), &["achievements"]));
    assert!(
        shown.contains("Night Owl          copper    8, silver at 10"),
        "{shown}"
    );
    let _daemon = daemon::start(home.path(), &[]);
    runtime().block_on(async {
        let (client, mut inbox) = daemon::connect(home.path(), "default").await;
        let session = session(&client).await;
        turn(&client, &mut inbox, &session, "first").await;
        let before = prompt_and_tools(home.path(), &session);
        // A second session started now is the tenth night session.
        let other = client.call("session.create", json!({})).await.unwrap();
        let other = other["session_id"].as_str().unwrap().to_owned();
        client
            .call("session.attach", json!({"session_id": other}))
            .await
            .unwrap();
        turn(&client, &mut inbox, &other, "second").await;
        let unlocked = loop {
            let event = tokio::time::timeout(Duration::from_secs(30), inbox.events.recv())
                .await
                .expect("the unlock arrives")
                .unwrap();
            if event.event == "achievement.unlocked" && event.data["id"] == "night-owl" {
                break event;
            }
        };
        assert_eq!(
            unlocked.data,
            json!({"profile": "default", "id": "night-owl", "tier": "silver"})
        );
        assert!(
            unlocked.session_id.is_none(),
            "a daemon-level event, never in a session's log"
        );
        turn(&client, &mut inbox, &session, "third").await;
        assert_eq!(prompt_and_tools(home.path(), &session), before);
    });
    for id in [common::newest_session(home.path())] {
        for message in common::session_messages(home.path(), &id) {
            let text = message.to_string().to_lowercase();
            assert!(
                !text.contains("night-owl") && !text.contains("achievement"),
                "{text}"
            );
        }
    }
}

/// Scenario: Unlock during a session (a tier first reached by the scan a
/// page's `achievements.list` starts, from sessions no daemon turn
/// rescanned, still reaches the attached clients).
#[test]
fn a_tier_found_by_a_page_scan_is_announced() {
    let home = mock_home();
    ok(&run(
        home.path(),
        &["config", "set", "cron.timezone", &night_zone()],
    ));
    let store = pmt_store::Store::open(&common_scope(home.path())).unwrap();
    let new_session = || {
        store
            .create_session(&pmt_store::NewSession::new(pmt_store::Source::Cli))
            .unwrap();
    };
    for _ in 0..8 {
        new_session();
    }
    // The first scan records night-owl copper without a celebration.
    ok(&run(home.path(), &["achievements"]));
    // That scan finished long ago, so the page asks for a fresh one.
    let unlocks = home.path().join("profiles/default/achievements.json");
    let mut recorded: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&unlocks).unwrap()).unwrap();
    recorded["scanned_at"] = json!("2026-01-01T00:00:00Z");
    std::fs::write(&unlocks, recorded.to_string()).unwrap();
    // Two sessions from outside the daemon (an in-process chat) make ten.
    new_session();
    new_session();
    drop(store);
    let _daemon = daemon::start(home.path(), &[]);
    runtime().block_on(async {
        let (client, mut inbox) = daemon::connect(home.path(), "default").await;
        session(&client).await;
        let listed = client
            .call("achievements.list", json!({"profile": "default"}))
            .await
            .unwrap();
        assert_eq!(listed["scanning"], true, "{listed}");
        let unlocked = loop {
            let event = tokio::time::timeout(Duration::from_secs(30), inbox.events.recv())
                .await
                .expect("the unlock arrives")
                .unwrap();
            if event.event == "achievement.unlocked" {
                break event;
            }
        };
        assert_eq!(
            unlocked.data,
            json!({"profile": "default", "id": "night-owl", "tier": "silver"})
        );
        assert!(unlocked.session_id.is_none());
    });
}
