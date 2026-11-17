use std::sync::{Arc, Mutex};

use pmt_core::secret::SecretString;
use serde_json::json;

use super::{BackendKind, ConfigBackend, Plane, Reply, Transport};

/// A plane in memory: one profile document with a version, recording
/// every write.
#[derive(Default)]
struct FakePlane {
    document: Mutex<serde_json::Value>,
    writes: Mutex<Vec<(String, serde_json::Value)>>,
    down: bool,
}

impl Transport for FakePlane {
    fn get(&self, url: &str, token: &SecretString) -> Result<Reply, String> {
        assert!(url.ends_with("/v1/instances/inst-1/profiles/default/config"));
        assert_eq!(token.expose(), "tok");
        if self.down {
            return Err("connection refused".to_owned());
        }
        Ok(Reply {
            status: 200,
            body: serde_json::to_vec(&*self.document.lock().unwrap()).unwrap(),
        })
    }

    fn patch(
        &self,
        _url: &str,
        _token: &SecretString,
        version: &str,
        body: &[u8],
    ) -> Result<Reply, String> {
        let mut document = self.document.lock().unwrap();
        if document["version"] != version {
            return Ok(Reply {
                status: 412,
                body: Vec::new(),
            });
        }
        let patch: serde_json::Value = serde_json::from_slice(body).unwrap();
        self.writes
            .lock()
            .unwrap()
            .push((version.to_owned(), patch.clone()));
        let next = document["version"]
            .as_str()
            .unwrap()
            .parse::<u32>()
            .unwrap()
            + 1;
        document["version"] = json!(next.to_string());
        Ok(Reply {
            status: 200,
            body: Vec::new(),
        })
    }
}

fn settings() -> ConfigBackend {
    ConfigBackend {
        kind: BackendKind::Remote,
        url: Some("https://plane.example".to_owned()),
        instance_id: Some("inst-1".to_owned()),
        ..ConfigBackend::default()
    }
}

fn home_with_token() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("profiles/default");
    std::fs::create_dir_all(&dir).unwrap();
    pmt_core::private_fs::write_atomic(
        &dir.join("secrets.toml"),
        b"CONFIG_PLANE_TOKEN = \"tok\"\n",
    )
    .unwrap();
    home
}

fn document() -> serde_json::Value {
    json!({
        "version": "1",
        "layers": [
            {"level": "global", "values": {"approvals": {"mode": "manual"}}, "locked": ["approvals.mode"]},
            {"level": "profile", "values": {"model": {"default": "remote-model"}}}
        ]
    })
}

#[test]
fn a_fetch_layers_the_plane_and_a_write_sends_only_the_changed_keys() {
    let home = home_with_token();
    let fake = Arc::new(FakePlane {
        document: Mutex::new(document()),
        ..FakePlane::default()
    });
    let plane = Plane::new(home.path(), settings(), fake.clone());
    let text = plane.text("default").unwrap();
    assert!(text.contains("remote-model"), "{text}");
    let new = text.replace("remote-model", "mine");
    plane.write("default", &text, &new).unwrap();
    let writes = fake.writes.lock().unwrap().clone();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].0, "1");
    assert_eq!(
        writes[0].1,
        json!({"set": {"model.default": "mine"}, "unset": []})
    );
}

#[test]
fn a_locked_key_is_refused_naming_its_level() {
    let home = home_with_token();
    let fake = Arc::new(FakePlane {
        document: Mutex::new(document()),
        ..FakePlane::default()
    });
    let plane = Plane::new(home.path(), settings(), fake.clone());
    let text = plane.text("default").unwrap();
    let new = text.replace("\"manual\"", "\"yolo\"");
    let err = plane.write("default", &text, &new).unwrap_err();
    assert_eq!(err.code().code, "config.remote_locked");
    assert!(err.message().contains("global level"), "{}", err.message());
    assert!(fake.writes.lock().unwrap().is_empty());
}

#[test]
fn a_stale_version_is_a_conflict() {
    let home = home_with_token();
    let fake = Arc::new(FakePlane {
        document: Mutex::new(document()),
        ..FakePlane::default()
    });
    let plane = Plane::new(home.path(), settings(), fake.clone());
    let text = plane.text("default").unwrap();
    fake.document.lock().unwrap()["version"] = json!("5");
    let err = plane
        .write("default", &text, &text.replace("remote-model", "x"))
        .unwrap_err();
    assert_eq!(err.code().code, "config.remote_conflict");
}

#[test]
fn settings_without_an_instance_are_refused() {
    let mut backend = settings();
    backend.instance_id = None;
    assert!(backend.problem().unwrap().contains("instance_id"));
    assert!(ConfigBackend::default().problem().is_none());
}
