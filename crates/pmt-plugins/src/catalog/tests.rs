use super::*;

fn key() -> String {
    minisign::KeyPair::generate_unencrypted_keypair()
        .unwrap()
        .pk
        .to_base64()
}

fn entry_json(id: &str, extra: &str) -> String {
    format!(
        r#"{{"id":"{id}","repo":"https://github.com/acme/{id}","ref":"{sha}","tier":"community","category":"tools","capabilities_summary":"network api.example","publisher_key":"{key}"{extra}}}"#,
        sha = "a".repeat(40),
        key = key()
    )
}

#[test]
fn the_embedded_catalog_parses() {
    let catalog = Catalog::embedded();
    assert!(
        catalog
            .entries
            .iter()
            .all(|e| e.repo.starts_with("https://"))
    );
}

#[test]
fn invalid_entries_are_skipped_and_valid_ones_kept() {
    let text = format!(
        r#"{{"plugins":[{good},{http},{tag},{tier},{nopkg}],"removed":[{{"reason":"x"}}]}}"#,
        good = entry_json("weather", ""),
        http = entry_json("plain", "").replace("https://", "http://"),
        tag = entry_json("tagged", "").replace(&"a".repeat(40), "v1.2.0"),
        tier = entry_json("odd", "").replace("community", "gold"),
        nopkg =
            entry_json("pkg", "").replace(&"a".repeat(40), &format!("sha256:{}", "b".repeat(64))),
    );
    let catalog = Catalog::parse(&text).unwrap();
    let ids: Vec<&str> = catalog.entries.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["weather"]);
    assert!(catalog.removed.is_empty());
    assert_eq!(catalog.search("WEATH").len(), 1);
    assert_eq!(catalog.search("api.example").len(), 1);
    assert!(catalog.search("nothing").is_empty());
}

#[test]
fn the_kill_list_matches_ids_and_canonical_repositories() {
    let text = r#"{"plugins":[],"removed":[
        {"id":"evil","reason":"exfiltrates tokens"},
        {"repo":"https://www.GitHub.com/acme/bad.git/","reason":"malware"}
    ]}"#;
    let catalog = Catalog::parse(text).unwrap();
    assert_eq!(
        catalog.blocked("evil", None).unwrap().reason,
        "exfiltrates tokens"
    );
    for spelling in [
        "https://github.com/acme/bad",
        "http://github.com/ACME/bad.git",
        "github.com/acme/bad/",
    ] {
        assert!(
            catalog.blocked("renamed", Some(spelling)).is_some(),
            "{spelling}"
        );
    }
    assert!(
        catalog
            .blocked("fine", Some("https://github.com/acme/good"))
            .is_none()
    );
    let err = catalog.blocked("evil", None).unwrap().refusal("evil");
    assert_eq!(
        err.message(),
        "Plugin 'evil' was removed from the catalog and is blocked: exfiltrates tokens"
    );
}

#[test]
fn a_live_catalog_needs_a_verifying_signature_and_a_bounded_size() {
    let good = |_: &[u8], sig: &str| {
        if sig == "ok" {
            Ok("KEY".to_owned())
        } else {
            Err("bad signature".to_owned())
        }
    };
    let text = format!(r#"{{"plugins":[{}]}}"#, entry_json("weather", ""));
    assert_eq!(
        accept(text.as_bytes(), "ok", &good).unwrap().entries.len(),
        1
    );
    let err = accept(text.as_bytes(), "tampered", &good).unwrap_err();
    assert!(err.starts_with("signature verification failed"), "{err}");
    let huge = vec![b' '; MAX_BYTES + 1];
    assert!(
        accept(&huge, "ok", &good)
            .unwrap_err()
            .contains("more than")
    );
}

#[test]
fn the_cache_keeps_a_verified_catalog_and_its_kill_list_joins_the_embedded_one() {
    let dir = tempfile::tempdir().unwrap();
    let cache = Cache::of(dir.path());
    let verify = |_: &[u8], sig: &str| {
        (sig == "ok")
            .then(|| "KEY".to_owned())
            .ok_or_else(|| "bad".to_owned())
    };
    assert!(cache.read(&verify).is_none());
    let text = r#"{"plugins":[],"removed":[{"id":"evil","reason":"r"}]}"#;
    cache.write(text.as_bytes(), "ok").unwrap();
    let (catalog, age) = cache.read(&verify).unwrap();
    assert_eq!(catalog.removed.len(), 1);
    assert!(age < CACHE_TTL);
    assert!(
        cache
            .kill_list(&verify)
            .iter()
            .any(|r| r.id.as_deref() == Some("evil"))
    );
    // A cache whose signature no longer verifies is ignored.
    std::fs::write(
        dir.path().join("cache/plugins/catalog.json.minisig"),
        "tampered",
    )
    .unwrap();
    assert!(cache.read(&verify).is_none());
}

#[test]
fn refs_are_commits_or_digests() {
    assert!(is_commit(&"0123456789abcdef".repeat(3)[..40]));
    assert!(!is_commit("main"));
    assert!(!is_commit(&"a".repeat(39)));
    assert!(is_digest(&format!("sha256:{}", "f".repeat(64))));
    assert!(!is_digest("sha256:abc"));
}
