//! A plane in memory for tests of the readers of a profile's
//! configuration: it serves one profile-level document per profile.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, PoisonError};

use pmt_core::secret::SecretString;
use serde_json::json;

use super::{BackendKind, ConfigBackend, PLANES, Plane, Reply, Transport};

struct Documents(BTreeMap<String, serde_json::Value>);

impl Transport for Documents {
    fn get(&self, url: &str, _token: &SecretString) -> Result<Reply, String> {
        let profile = url
            .strip_suffix("/config")
            .and_then(|u| u.rsplit('/').next())
            .unwrap_or_default();
        Ok(match self.0.get(profile) {
            Some(layers) => Reply {
                status: 200,
                body: serde_json::to_vec(&json!({"version": "1", "layers": layers}))
                    .unwrap_or_default(),
            },
            None => Reply {
                status: 404,
                body: Vec::new(),
            },
        })
    }

    fn patch(&self, _: &str, _: &SecretString, _: &str, _: &[u8]) -> Result<Reply, String> {
        Err("this fake plane is read-only".to_owned())
    }
}

/// Makes `home` (with the given profiles) use a plane serving `values`
/// (JSON tables) at each profile's level.
///
/// # Panics
///
/// When the home cannot be written.
pub(crate) fn serve(home: &Path, values: &[(&str, serde_json::Value)]) {
    let layers: Vec<_> = values
        .iter()
        .map(|(p, v)| (*p, json!([{"level": "profile", "values": v}])))
        .collect();
    serve_layers(home, &layers);
}

/// Makes `home` use a plane serving each profile's `layers` (the wire's
/// `layers` array).
///
/// # Panics
///
/// When the home cannot be written.
pub(crate) fn serve_layers(home: &Path, values: &[(&str, serde_json::Value)]) {
    for (profile, _) in values {
        let dir = home.join("profiles").join(profile);
        std::fs::create_dir_all(&dir).unwrap();
        pmt_core::private_fs::write_atomic(
            &dir.join("secrets.toml"),
            b"CONFIG_PLANE_TOKEN = \"tok\"\n",
        )
        .unwrap();
    }
    let documents = values
        .iter()
        .map(|(p, v)| ((*p).to_owned(), v.clone()))
        .collect();
    let settings = ConfigBackend {
        kind: BackendKind::Remote,
        url: Some("https://plane.example".to_owned()),
        instance_id: Some("inst-1".to_owned()),
        ..ConfigBackend::default()
    };
    let plane = Plane::new(home, settings, Arc::new(Documents(documents)));
    PLANES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(home.to_owned(), Some(Arc::new(plane)));
}
