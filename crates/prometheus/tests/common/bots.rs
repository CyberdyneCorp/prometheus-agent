//! Bot Mode test homes (spec: bot-mode): a multiplexing home whose
//! profiles (bots) answer with mock scripts, desktop-style clients of its
//! daemon, and readers of each bot's store.

use std::path::Path;
use std::time::{Duration, Instant};

use pmt_rpc::client::Client;
use pmt_rpc::contracts::ClientKind;
use serde_json::{Value, json};

use super::{mock_home, ok, run};

/// A home serving every profile, its default profile answering with
/// `rules`.
pub fn home(rules: &str) -> tempfile::TempDir {
    let home = mock_home();
    std::fs::write(home.path().join("profiles/default/mock.toml"), rules).unwrap();
    for (key, value) in [
        ("providers.mock.script", "mock.toml"),
        ("gateway.multiplex", "true"),
    ] {
        ok(&run(home.path(), &["config", "set", key, value]));
    }
    home
}

/// Bot `name` of `home`, answering with `rules`, with `extra` settings.
pub fn bot(home: &Path, name: &str, rules: &str, extra: &[(&str, &str)]) {
    ok(&run(home, &["profile", "create", name, "--no-alias"]));
    std::fs::write(home.join(format!("profiles/{name}/mock.toml")), rules).unwrap();
    let base = [
        ("model.provider", "mock"),
        ("model.default", "mock"),
        ("providers.mock.script", "mock.toml"),
    ];
    for (key, value) in base.iter().chain(extra) {
        ok(&run(home, &["-p", name, "config", "set", key, value]));
    }
}

/// The store of `profile`.
pub fn store(home: &Path, profile: &str) -> pmt_store::Store {
    let layout = pmt_config::Home::open(home).unwrap();
    let dir = layout
        .existing_profile(&pmt_config::ProfileName::parse(profile).unwrap())
        .unwrap();
    let scope = pmt_config::ProfileScope::open(
        dir,
        &pmt_config::Overrides::default(),
        pmt_config::Launch::Other,
    )
    .unwrap();
    pmt_store::Store::open(&scope).unwrap()
}

/// The canonical Bot Chat of `profile`, once it has one.
pub fn bot_chat(home: &Path, profile: &str) -> Option<String> {
    store(home, profile).bot_chat_id().unwrap()
}

/// The texts of `role` in `profile`'s session `id` (active messages).
pub fn said(home: &Path, profile: &str, id: &str, role: &str) -> Vec<String> {
    store(home, profile)
        .messages(id.parse().unwrap(), pmt_store::Visibility::Active)
        .unwrap()
        .into_iter()
        .filter(|m| m.role.as_str() == role)
        .map(|m| m.content.unwrap_or_default())
        .collect()
}

/// Waits up to `limit` for `check`.
pub fn wait(what: &str, limit: Duration, mut check: impl FnMut() -> bool) {
    let start = Instant::now();
    while !check() {
        assert!(start.elapsed() < limit, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Waits until `profile`'s Bot Chat has a `role` message containing
/// `text`; every such message.
pub fn wait_said(home: &Path, profile: &str, role: &str, text: &str) -> Vec<String> {
    let mut found = Vec::new();
    wait(
        &format!("{profile} {role}: {text}"),
        Duration::from_mins(2),
        || {
            found = bot_chat(home, profile)
                .map(|id| said(home, profile, &id, role))
                .unwrap_or_default();
            found.iter().any(|t| t.contains(text))
        },
    );
    found
}

/// A desktop client of `home`'s daemon granted `profiles`.
pub async fn client(home: &Path, profiles: &[&str]) -> Client {
    let mut hello = super::daemon::hello(true);
    hello.client = ClientKind::Desktop;
    let (client, _inbox) = Client::connect(
        &super::daemon::record(home).endpoint,
        if profiles.len() > 1 {
            pmt_rpc::control::Purpose::ProfileAdmin
        } else {
            pmt_rpc::control::Purpose::Interactive
        },
        profiles.iter().map(|p| (*p).to_owned()).collect(),
        &hello,
    )
    .await
    .unwrap();
    client
}

/// Calls `method` with `params`.
pub async fn call(client: &Client, method: &str, params: Value) -> Value {
    client
        .call(method, params.clone())
        .await
        .unwrap_or_else(|e| panic!("{method} {params}: {e:?}"))
}

/// Sends `text` to `profile`'s Bot Chat as its person would; the Bot
/// Chat's id.
pub async fn tell(client: &Client, profile: &str, text: &str) -> String {
    let chat = call(client, "bot.chat.ensure", json!({"profile": profile})).await;
    let id = chat["session_id"].as_str().unwrap().to_owned();
    call(client, "session.attach", json!({"session_id": id})).await;
    call(
        client,
        "prompt.submit",
        json!({"session_id": id, "text": text, "files": []}),
    )
    .await;
    id
}
