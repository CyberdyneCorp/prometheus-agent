//! The `raft` wake-only catalog adapter plugin (spec: platform-adapters,
//! Raft wake-only adapter; Catalog adapter set): its sidecar (run with the
//! system Node) starts a fake Raft CLI bridge with only the Raft profile
//! and a per-start wake token in its environment; wake hints become
//! content-free notices, and a hint carrying content is refused.

#![cfg(all(unix, feature = "gateway"))]

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::catalog::{self, Bridge};
use pmt_gateway::messaging::conformance::{Failure, MockPlatform, MockSent};
use serde_json::{Value, json};

const NOTICE: &str = "Raft wake hint received: new Raft messages may be pending. Read and answer them with the Raft CLI (run `raft manual get raft-cli-overview` first if you have not this session).";

/// A fake `raft` CLI: records its token, endpoint and environment names.
fn fake_raft(dir: &Path) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt as _;
    let seen = dir.join("raft-seen");
    let script = dir.join("raft");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nenv | cut -d= -f1 | sort | tr '\\n' ' ' > {seen}.env\necho \"$RAFT_CHANNEL_TOKEN $8 $2\" > {seen}.tmp\nmv {seen}.tmp {seen}\nexec sleep 600\n",
            seen = seen.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script, seen)
}

/// Raft, as the bridge child: posts wake hints to the sidecar.
struct Raft {
    seen: PathBuf,
    last: Mutex<Option<String>>,
}

impl Raft {
    /// The wake token and endpoint the fake bridge was given.
    fn bridge(&self) -> (String, String, String) {
        for _ in 0..300 {
            if let Ok(text) = std::fs::read_to_string(&self.seen) {
                let mut words = text.split_whitespace().map(str::to_owned);
                return (
                    words.next().unwrap(),
                    words.next().unwrap(),
                    words.next().unwrap(),
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("the Raft bridge never started");
    }

    fn wake(&self, hint: &Value) -> u16 {
        let (token, endpoint, _) = self.bridge();
        catalog::post(
            &endpoint,
            &[
                ("content-type", "application/json"),
                ("x-raft-bridge-token", &token),
            ],
            hint.to_string().as_bytes(),
        )
        .0
    }
}

impl MockPlatform for Raft {
    fn dm(&self) -> (String, String) {
        ("raft".to_owned(), "raft".to_owned())
    }

    fn inject(&self, text: &str) -> String {
        *self.last.lock().unwrap() = Some(text.to_owned());
        assert_eq!(self.wake(&json!({"eventId": text, "channel": "kit"})), 202);
        text.to_owned()
    }

    fn redeliver(&self) {
        let last = self.last.lock().unwrap().clone();
        if let Some(id) = last {
            self.wake(&json!({"eventId": id}));
        }
    }

    /// Wake hints wait in the sidecar while the adapter is away.
    fn drop_connection(&self) {}

    fn press(&self, _message_id: &str, _data: &str) {}

    fn fail_next(&self, _failure: Failure) {}

    fn reports_markup_errors(&self) -> bool {
        false
    }

    fn sent(&self) -> Vec<MockSent> {
        Vec::new()
    }

    fn typing(&self) -> usize {
        0
    }

    fn reactions(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// The agent answers with the Raft CLI: the adapter delivers nothing.
    fn sends_can_fail(&self) -> bool {
        false
    }

    fn admitted_text(&self, injected: &str) -> String {
        format!("{NOTICE} (wake {injected})")
    }
}

fn setup(dir: &Path) -> (Bridge, Arc<Raft>, Value, Vec<String>) {
    let (script, seen) = fake_raft(dir);
    let sidecar = Bridge::start_script("plugins/first-party/raft/sidecar/raft-wake.mjs", &[]);
    let options = json!({
        "raft_profile": "kit-profile",
        "raft_command": script,
        "sidecar": sidecar.sidecar,
    });
    let hosts = vec![sidecar.base.trim_start_matches("http://").to_owned()];
    let raft = Arc::new(Raft {
        seen,
        last: Mutex::new(None),
    });
    (sidecar, raft, options, hosts)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_raft_adapter_passes_the_conformance_suite() {
    let dir = catalog::scratch();
    let (_sidecar, raft, options, hosts) = setup(dir.path());
    let report = catalog::conformance(
        "raft",
        &options,
        &hosts,
        &[],
        Arc::clone(&raft) as Arc<dyn MockPlatform>,
    )
    .await;
    catalog::assert_passed(&report);
}

/// Scenario: Content-bearing wake rejected.
#[tokio::test(flavor = "multi_thread")]
async fn a_wake_carrying_content_is_rejected_and_injects_nothing() {
    let dir = catalog::scratch();
    let (_sidecar, raft, options, hosts) = setup(dir.path());
    let (kit, _root) = catalog::install("raft", &options, &hosts);
    let adapter = kit.adapter().unwrap();
    let link = catalog::Link::new();
    adapter.connect(link.dyn_link()).await.unwrap();
    let (_, _, profile) = raft.bridge();
    assert_eq!(profile, "kit-profile");
    // The bridge child holds only the Raft profile and its wake token.
    let names = std::fs::read_to_string(dir.path().join("raft-seen.env")).unwrap();
    let names: Vec<&str> = names
        .split_whitespace()
        // What the shell itself (and macOS) adds.
        .filter(|n| !["PWD", "SHLVL", "_", "OLDPWD"].contains(n) && !n.starts_with("__CF"))
        .collect();
    assert_eq!(
        names,
        ["HOME", "PATH", "RAFT_CHANNEL_TOKEN", "RAFT_PROFILE"]
    );
    assert_eq!(
        raft.wake(&json!({"eventId": "e1", "text": "secret message"})),
        400
    );
    assert_eq!(
        raft.wake(&json!({"eventId": "e2", "meta": {"messages": []}})),
        400
    );
    assert_eq!(raft.wake(&json!({"eventId": "e3"})), 202);
    let admitted = link.wait_for(1).await;
    assert_eq!(admitted.len(), 1, "{admitted:?}");
    assert_eq!(admitted[0].text, format!("{NOTICE} (wake e3)"));
    assert!(!admitted[0].text.contains("secret"));
    adapter.disconnect().await;
}
