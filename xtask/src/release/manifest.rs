//! `release-manifest.json`, the channel index, and the end-to-end check of
//! a release directory.

use std::path::Path;

use pmt_update::manifest::{
    Artifact, ChannelIndex, MACOS_UNIVERSAL, MANIFEST_FILE, MANIFEST_SCHEMA, ReleaseManifest,
    SIGNATURE_SUFFIX, sha256_hex,
};
use pmt_update::{ReleaseVersion, TrustRoot};

/// Every target a release ships (spec: Release artifacts and targets):
/// the per-architecture macOS archives cargo-dist builds plus the universal
/// one made from them.
pub(super) const TARGETS: [&str; 10] = [
    MACOS_UNIVERSAL,
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
    "aarch64-linux-android",
];

/// The files beside the archives that are signed too.
const INSTALLERS: [&str; 2] = ["install.sh", "install.ps1"];

/// Writes `release-manifest.json` and `channel-<channel>.json` in `dir`.
pub(super) fn write(dir: &Path, tag: &str, commit: &str) -> Result<(), String> {
    let version = ReleaseVersion::parse(tag).map_err(|e| e.to_string())?;
    let mut artifacts = Vec::new();
    for target in TARGETS {
        let path = dir.join(Artifact::file_name(target));
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        artifacts.push(Artifact::of(target, &bytes));
    }
    if artifacts.is_empty() {
        return Err(format!(
            "{} holds no prometheus-<target>.tar.gz",
            dir.display()
        ));
    }
    let manifest = ReleaseManifest {
        schema: MANIFEST_SCHEMA.to_owned(),
        version,
        commit: commit.to_owned(),
        published_at: jiff::Timestamp::now().to_string(),
        artifacts,
    };
    let index = ChannelIndex::new(version.channel(), version);
    write_file(&dir.join(MANIFEST_FILE), &manifest.to_published_json())?;
    let index_json = serde_json::to_string_pretty(&index).map_err(|e| e.to_string())? + "\n";
    write_file(
        &dir.join(format!("channel-{}.json", version.channel())),
        &index_json,
    )
}

fn write_file(path: &Path, text: &str) -> Result<(), String> {
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}

/// Checks a release directory end to end; lists every problem.
pub(super) fn verify(dir: &Path, key: &Path) -> Result<(), String> {
    let key_text = std::fs::read_to_string(key).map_err(|e| format!("{}: {e}", key.display()))?;
    let trust = TrustRoot::from_keys(&[("release", &key_text)]).map_err(|e| e.to_string())?;
    let mut problems = Vec::new();
    let manifest_bytes = signed(dir, MANIFEST_FILE, &trust, &mut problems);
    let manifest: Option<ReleaseManifest> =
        manifest_bytes.and_then(|bytes| match serde_json::from_slice(&bytes) {
            Ok(m) => Some(m),
            Err(e) => {
                problems.push(format!("{MANIFEST_FILE}: {e}"));
                None
            }
        });
    if let Some(manifest) = &manifest {
        for target in TARGETS {
            check_target(dir, manifest, target, &trust, &mut problems);
        }
        check_index(dir, manifest, &trust, &mut problems);
    }
    for installer in INSTALLERS {
        signed(dir, installer, &trust, &mut problems);
    }
    if problems.is_empty() {
        println!("release verified: {}", dir.display());
        return Ok(());
    }
    for problem in &problems {
        eprintln!("✗ {problem}");
    }
    Err(format!(
        "{} problem(s) in {}",
        problems.len(),
        dir.display()
    ))
}

/// The bytes of `dir/name` when its `.minisig` verifies.
fn signed(
    dir: &Path,
    name: &str,
    trust: &TrustRoot,
    problems: &mut Vec<String>,
) -> Option<Vec<u8>> {
    let Ok(bytes) = std::fs::read(dir.join(name)) else {
        problems.push(format!("{name}: missing"));
        return None;
    };
    let sig_path = dir.join(format!("{name}{SIGNATURE_SUFFIX}"));
    let Ok(signature) = std::fs::read_to_string(&sig_path) else {
        problems.push(format!("{name}{SIGNATURE_SUFFIX}: missing"));
        return None;
    };
    if let Err(e) = trust.verify(name, &bytes, &signature) {
        problems.push(e.to_string());
        return None;
    }
    Some(bytes)
}

fn check_target(
    dir: &Path,
    manifest: &ReleaseManifest,
    target: &str,
    trust: &TrustRoot,
    problems: &mut Vec<String>,
) {
    let Some(entry) = manifest.artifacts.iter().find(|a| a.target == target) else {
        problems.push(format!("{MANIFEST_FILE}: no artifact for {target}"));
        return;
    };
    let Some(bytes) = signed(dir, &entry.name, trust, problems) else {
        return;
    };
    if !entry.matches(&bytes) {
        problems.push(format!(
            "{}: size or digest differs from the manifest",
            entry.name
        ));
    }
    let checksum = std::fs::read_to_string(dir.join(format!("{}.sha256", entry.name)));
    if !checksum.is_ok_and(|c| c.split_whitespace().next() == Some(&sha256_hex(&bytes))) {
        problems.push(format!("{}.sha256: missing or wrong", entry.name));
    }
    let sbom = std::fs::read(dir.join(sbom_name(&entry.name)));
    let valid_sbom = sbom
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .is_some_and(|doc| {
            doc["spdxVersion"] == "SPDX-2.3"
                && doc["packages"].as_array().is_some_and(|p| p.len() > 1)
        });
    if !valid_sbom {
        problems.push(format!(
            "{}: missing or invalid SPDX SBOM",
            sbom_name(&entry.name)
        ));
    }
}

fn check_index(
    dir: &Path,
    manifest: &ReleaseManifest,
    trust: &TrustRoot,
    problems: &mut Vec<String>,
) {
    let name = format!("channel-{}.json", manifest.version.channel());
    let Some(bytes) = signed(dir, &name, trust, problems) else {
        return;
    };
    match serde_json::from_slice::<ChannelIndex>(&bytes) {
        Ok(index) if index.version == manifest.version => {}
        Ok(_) => problems.push(format!("{name}: names another version than the manifest")),
        Err(e) => problems.push(format!("{name}: {e}")),
    }
}

/// `prometheus-<target>.tar.gz` -> `prometheus-<target>.spdx.json`.
pub(super) fn sbom_name(archive: &str) -> String {
    format!("{}.spdx.json", archive.trim_end_matches(".tar.gz"))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use pmt_update::manifest::{Artifact, MANIFEST_FILE};

    use super::{TARGETS, sbom_name, verify, write};
    use crate::release::sign::sign_files;

    const KEY: &str = include_str!("../../../crates/pmt-update/tests/fixtures/test.key");
    const PUB: &str = "crates/pmt-update/keys/test.pub";

    /// A complete dry-run release directory signed with the test key.
    fn release(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        let mut signed = Vec::new();
        for target in TARGETS {
            let name = Artifact::file_name(target);
            let bytes = format!("archive of {target}");
            std::fs::write(dir.join(&name), &bytes).unwrap();
            let digest = pmt_update::manifest::sha256_hex(bytes.as_bytes());
            std::fs::write(
                dir.join(format!("{name}.sha256")),
                format!("{digest}  {name}\n"),
            )
            .unwrap();
            let sbom = r#"{"spdxVersion":"SPDX-2.3","packages":[{},{}]}"#;
            std::fs::write(dir.join(sbom_name(&name)), sbom).unwrap();
            signed.push(dir.join(name));
        }
        for installer in ["install.sh", "install.ps1"] {
            std::fs::write(dir.join(installer), "installer").unwrap();
            signed.push(dir.join(installer));
        }
        write(dir, "v1.4.0", "abc").unwrap();
        signed.push(dir.join(MANIFEST_FILE));
        signed.push(dir.join("channel-stable.json"));
        sign_files(KEY, false, None, &signed).unwrap();
    }

    #[test]
    fn manifest_covers_every_artifact_and_verifies() {
        let root = crate::repo_root();
        let dir = root.join("target/xtask-test/release-complete");
        release(&dir);
        verify(&dir, &root.join(PUB)).unwrap();
    }

    #[test]
    fn a_tampered_or_missing_artifact_fails_verification() {
        let root = crate::repo_root();
        let dir = root.join("target/xtask-test/release-broken");
        release(&dir);
        std::fs::write(
            dir.join(Artifact::file_name("x86_64-unknown-linux-musl")),
            "evil",
        )
        .unwrap();
        std::fs::remove_file(dir.join(sbom_name(&Artifact::file_name("aarch64-linux-android"))))
            .unwrap();
        let err = verify(&dir, &root.join(PUB)).unwrap_err();
        assert_eq!(err, format!("2 problem(s) in {}", dir.display()));
    }
}
