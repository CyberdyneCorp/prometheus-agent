//! `cargo xtask release channels`: the package-manager and image channels
//! of one release (spec: distribution-and-update, Package-manager and
//! image channels), rendered from its signed `release-manifest.json` so
//! every channel ships the same signed binaries:
//!
//! - `homebrew/prometheus.rb`: the cyberdynecorp/tap formula
//! - `winget/*.yaml`: the `WinGet` manifests, plus the `.zip` repacks of the
//!   Windows archives they point at (`WinGet` reads zips, not tarballs)
//! - `termux/build.sh`: the termux-packages recipe
//! - `nix/release.json`: what the flake's package fetches per system
//!
//! `release container-context` makes the image build context earlier (the
//! images must exist before the manifest that pins them).
//!
//! Each channel stamps its install receipt with its own owner through
//! `prometheus install-receipt --owner <owner>` (`WinGet`, which runs no
//! scripts, is recognised by its install location).

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::Path;

use pmt_update::manifest::{MANIFEST_FILE, ReleaseManifest, sha256_hex};

/// Where release files are downloaded from.
const DOWNLOAD: &str = "https://github.com/CyberdyneCorp/prometheus-agent/releases/download";

/// Windows targets repacked as `.zip` for `WinGet`.
const WINDOWS: [&str; 2] = ["x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"];

/// Linux binaries of the image, per Docker `TARGETARCH`.
const IMAGE_ARCHES: [(&str, &str); 2] = [
    ("amd64", "x86_64-unknown-linux-musl"),
    ("arm64", "aarch64-unknown-linux-musl"),
];

/// Nix systems and the archive each fetches.
const NIX_SYSTEMS: [(&str, &str); 4] = [
    ("x86_64-linux", "x86_64-unknown-linux-musl"),
    ("aarch64-linux", "aarch64-unknown-linux-musl"),
    ("x86_64-darwin", "universal2-apple-darwin"),
    ("aarch64-darwin", "universal2-apple-darwin"),
];

/// Templates under `distrib/channels`, rendered to the same relative path
/// without `.in`.
const TEMPLATES: [&str; 5] = [
    "homebrew/prometheus.rb.in",
    "winget/CyberdyneCorp.Prometheus.yaml.in",
    "winget/CyberdyneCorp.Prometheus.installer.yaml.in",
    "winget/CyberdyneCorp.Prometheus.locale.en-US.yaml.in",
    "termux/build.sh.in",
];

/// Renders every channel of the release in `dir` into `out`.
pub(super) fn render(root: &Path, dir: &Path, out: &Path) -> Result<(), String> {
    let manifest_path = dir.join(MANIFEST_FILE);
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("{}: {e}", manifest_path.display()))?;
    let manifest: ReleaseManifest =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", manifest_path.display()))?;
    let mut values = values(&manifest);
    let zips = out.join("archives");
    create(&zips)?;
    for target in WINDOWS {
        if manifest.artifact_for(target).is_none() {
            continue;
        }
        let name = format!("prometheus-{target}.zip");
        let zip = repack_zip(&read(&dir.join(format!("prometheus-{target}.tar.gz")))?)?;
        values.insert(
            format!("URL_{target}.zip"),
            format!("{DOWNLOAD}/{}/{name}", manifest.version),
        );
        values.insert(format!("SHA256_{target}.zip"), sha256_hex(&zip));
        write(&zips.join(name), &zip)?;
    }
    for template in TEMPLATES {
        let source = root.join("distrib/channels").join(template);
        let text =
            std::fs::read_to_string(&source).map_err(|e| format!("{}: {e}", source.display()))?;
        let rendered = fill(&text, &values).map_err(|e| format!("{template}: {e}"))?;
        let target = out.join(template.trim_end_matches(".in"));
        create(target.parent().unwrap_or(out))?;
        write(&target, rendered.as_bytes())?;
    }
    write_nix(&manifest, out)
}

/// `@NAME@` values: the version and each artifact's URL and digest.
fn values(manifest: &ReleaseManifest) -> BTreeMap<String, String> {
    let tag = manifest.version.to_string();
    let mut values = BTreeMap::from([
        ("TAG".to_owned(), tag.clone()),
        (
            "BARE_VERSION".to_owned(),
            tag.trim_start_matches('v').to_owned(),
        ),
        ("CHANNEL".to_owned(), manifest.version.channel().to_string()),
    ]);
    for artifact in &manifest.artifacts {
        values.insert(
            format!("URL_{}", artifact.target),
            format!("{DOWNLOAD}/{tag}/{}", artifact.name),
        );
        values.insert(
            format!("SHA256_{}", artifact.target),
            artifact.sha256.clone(),
        );
    }
    values
}

/// Replaces every `@NAME@`; a placeholder without a value is an error, so
/// a release missing a target never renders a half-filled channel.
pub(super) fn fill(text: &str, values: &BTreeMap<String, String>) -> Result<String, String> {
    let mut out = text.to_owned();
    for (name, value) in values {
        out = out.replace(&format!("@{name}@"), value);
    }
    let placeholder = regex::Regex::new(r"@([A-Z][A-Za-z0-9_.-]*)@").expect("valid pattern");
    let missing: Vec<&str> = placeholder
        .captures_iter(&out)
        .filter_map(|c| c.get(1).map(|m| m.as_str()))
        .collect();
    if missing.is_empty() {
        Ok(out)
    } else {
        Err(format!("no value for {}", missing.join(", ")))
    }
}

/// `nix/release.json`: per Nix system, the archive URL and its SRI hash.
fn write_nix(manifest: &ReleaseManifest, out: &Path) -> Result<(), String> {
    let mut systems = serde_json::Map::new();
    for (system, target) in NIX_SYSTEMS {
        let Some(artifact) = manifest.artifact_for(target) else {
            continue;
        };
        let bytes = hex(&artifact.sha256)?;
        systems.insert(
            system.to_owned(),
            serde_json::json!({
                "url": format!("{DOWNLOAD}/{}/{}", manifest.version, artifact.name),
                "hash": format!("sha256-{}", base64(&bytes)),
            }),
        );
    }
    let body = serde_json::json!({
        "version": manifest.version.to_string().trim_start_matches('v'),
        "systems": systems,
    });
    let dir = out.join("nix");
    create(&dir)?;
    let text = serde_json::to_string_pretty(&body).map_err(|e| e.to_string())? + "\n";
    write(&dir.join("release.json"), text.as_bytes())
}

/// The image build context: `distrib/container` plus
/// `bin/<arch>/prometheus` from each Linux musl archive in `dir`. It runs
/// before the manifest exists, which pins the images built from it.
pub(super) fn container_context(root: &Path, dir: &Path, out: &Path) -> Result<(), String> {
    copy_dir(&root.join("distrib/container"), out)?;
    for (arch, target) in IMAGE_ARCHES {
        let path = dir.join(format!("prometheus-{target}.tar.gz"));
        if !path.exists() {
            continue;
        }
        let archive = read(&path)?;
        let binary = binary_of(&archive)?;
        let bin = out.join("bin").join(arch);
        create(&bin)?;
        write(&bin.join("prometheus"), &binary)?;
    }
    Ok(())
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    create(to)?;
    let entries = std::fs::read_dir(from).map_err(|e| format!("{}: {e}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .map_err(|e| format!("{}: {e}", target.display()))?;
        }
    }
    Ok(())
}

/// The `prometheus` executable inside a release `.tar.gz`.
fn binary_of(archive: &[u8]) -> Result<Vec<u8>, String> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in tar.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        if path.file_name().is_some_and(|n| n == "prometheus") {
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
            return Ok(bytes);
        }
    }
    Err("the archive holds no prometheus binary".to_owned())
}

/// The files of a release `.tar.gz` as a deflated `.zip`, same paths.
pub(super) fn repack_zip(archive: &[u8]) -> Result<Vec<u8>, String> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o755);
    for entry in tar.entries().map_err(|e| e.to_string())? {
        let mut entry = entry.map_err(|e| e.to_string())?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = entry.path().map_err(|e| e.to_string())?;
        let name = path.to_string_lossy().replace('\\', "/");
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).map_err(|e| e.to_string())?;
        zip.start_file(name, options).map_err(|e| e.to_string())?;
        std::io::Write::write_all(&mut zip, &bytes).map_err(|e| e.to_string())?;
    }
    let cursor = zip.finish().map_err(|e| e.to_string())?;
    Ok(cursor.into_inner())
}

fn hex(text: &str) -> Result<Vec<u8>, String> {
    (0..text.len())
        .step_by(2)
        .map(|i| {
            text.get(i..i + 2)
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or_else(|| format!("bad sha256 {text}"))
        })
        .collect()
}

/// Standard base64 with padding (Nix SRI hashes).
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i)) as usize & 63]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}

fn create(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use pmt_update::manifest::{
        Artifact, MANIFEST_FILE, MANIFEST_SCHEMA, ReleaseDetails, ReleaseManifest, sha256_hex,
    };

    use super::{base64, fill, render};

    /// A `.tar.gz` laid out like a release archive.
    fn archive(target: &str, exe: &str) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let mut tar = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        header.set_size(6);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(
            &mut header,
            format!("prometheus-{target}/{exe}"),
            &b"binary"[..],
        )
        .unwrap();
        tar.into_inner().unwrap().finish().unwrap()
    }

    fn release(dir: &Path) -> ReleaseManifest {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).unwrap();
        let targets = [
            ("universal2-apple-darwin", "prometheus"),
            ("x86_64-unknown-linux-musl", "prometheus"),
            ("aarch64-unknown-linux-musl", "prometheus"),
            ("x86_64-pc-windows-msvc", "prometheus.exe"),
            ("aarch64-pc-windows-msvc", "prometheus.exe"),
            ("aarch64-linux-android", "prometheus"),
        ];
        let artifacts = targets
            .iter()
            .map(|(target, exe)| {
                let bytes = archive(target, exe);
                std::fs::write(dir.join(Artifact::file_name(target)), &bytes).unwrap();
                Artifact::of(target, &bytes)
            })
            .collect();
        let manifest = ReleaseManifest {
            schema: MANIFEST_SCHEMA.to_owned(),
            version: pmt_update::ReleaseVersion::parse("v1.4.0").unwrap(),
            commit: "abc".to_owned(),
            published_at: "2026-11-01T00:00:00Z".to_owned(),
            artifacts,
            details: ReleaseDetails::default(),
        };
        std::fs::write(dir.join(MANIFEST_FILE), manifest.to_published_json()).unwrap();
        manifest
    }

    #[test]
    fn each_channel_is_rendered_from_the_signed_manifest_with_its_owner() {
        let root = crate::repo_root();
        let dir = root.join("target/xtask-test/channels-release");
        let out = root.join("target/xtask-test/channels-out");
        let _ = std::fs::remove_dir_all(&out);
        let manifest = release(&dir);
        render(&root, &dir, &out).unwrap();
        let read = |p: &str| std::fs::read_to_string(out.join(p)).unwrap();
        let mac = manifest.artifact_for("universal2-apple-darwin").unwrap();
        let formula = read("homebrew/prometheus.rb");
        assert!(formula.contains(&mac.sha256), "{formula}");
        assert!(formula.contains("version \"1.4.0\""), "{formula}");
        assert!(formula.contains(r#""install-receipt", "--owner", "homebrew""#));
        let zip =
            std::fs::read(out.join("archives/prometheus-x86_64-pc-windows-msvc.zip")).unwrap();
        let installer = read("winget/CyberdyneCorp.Prometheus.installer.yaml");
        assert!(installer.contains(&sha256_hex(&zip)), "{installer}");
        assert!(installer.contains("v1.4.0/prometheus-x86_64-pc-windows-msvc.zip"));
        let unzipped = zip::ZipArchive::new(std::io::Cursor::new(zip)).unwrap();
        assert!(
            unzipped
                .file_names()
                .any(|n| n.is_ok_and(|n| n == "prometheus-x86_64-pc-windows-msvc/prometheus.exe"))
        );
        assert!(read("termux/build.sh").contains("install-receipt --owner termux"));
        let nix: serde_json::Value = serde_json::from_str(&read("nix/release.json")).unwrap();
        assert_eq!(nix["version"], "1.4.0");
        assert!(
            nix["systems"]["aarch64-darwin"]["hash"]
                .as_str()
                .unwrap()
                .starts_with("sha256-")
        );
        let ctx = root.join("target/xtask-test/channels-image");
        let _ = std::fs::remove_dir_all(&ctx);
        super::container_context(&root, &dir, &ctx).unwrap();
        assert_eq!(
            std::fs::read(ctx.join("bin/arm64/prometheus")).unwrap(),
            b"binary"
        );
        let dockerfile = std::fs::read_to_string(ctx.join("Dockerfile")).unwrap();
        assert!(dockerfile.contains("install-receipt --owner image"));
    }

    #[test]
    fn every_placeholder_needs_a_value() {
        let values = BTreeMap::from([("TAG".to_owned(), "v1.4.0".to_owned())]);
        assert_eq!(fill("tag @TAG@", &values).unwrap(), "tag v1.4.0");
        let err = fill("url @URL_x86_64-unknown-linux-musl@", &values).unwrap_err();
        assert!(err.contains("URL_x86_64-unknown-linux-musl"), "{err}");
        // An e-mail address or a Ruby `@ivar` is not a placeholder.
        assert!(fill("a@b.c and @libexec", &values).is_ok());
    }

    #[test]
    fn sri_hashes_are_standard_base64() {
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
    }
}
