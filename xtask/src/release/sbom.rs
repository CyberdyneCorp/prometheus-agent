//! The SPDX 2.3 JSON SBOM of the shipped binary for one target: every
//! crate linked into `prometheus` (normal dependencies, resolved for that
//! target from the committed lockfile).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use pmt_update::manifest::MACOS_UNIVERSAL;
use serde_json::{Value, json};

/// Writes the SBOM of `target` to `out`.
pub(super) fn write(root: &Path, target: &str, out: &Path) -> Result<(), String> {
    // The universal binary links the same crates as its arm64 half.
    let platform = if target == MACOS_UNIVERSAL {
        "aarch64-apple-darwin"
    } else {
        target
    };
    let metadata = cargo_metadata(root, platform)?;
    let document = document(&metadata, target)?;
    let text = serde_json::to_string_pretty(&document).map_err(|e| e.to_string())? + "\n";
    std::fs::write(out, text).map_err(|e| format!("{}: {e}", out.display()))?;
    println!("wrote {}", out.display());
    Ok(())
}

fn cargo_metadata(root: &Path, platform: &str) -> Result<Value, String> {
    let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned()))
        .current_dir(root)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--filter-platform",
            platform,
        ])
        .output()
        .map_err(|e| format!("cargo metadata: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("cargo metadata: {e}"))
}

/// Package ids reachable from `prometheus` through normal dependencies,
/// with their direct dependencies.
fn closure(metadata: &Value) -> Result<(String, BTreeMap<String, Vec<String>>), String> {
    let packages = metadata["packages"].as_array().ok_or("no packages")?;
    let root = packages
        .iter()
        .find(|p| p["name"] == "prometheus")
        .and_then(|p| p["id"].as_str())
        .ok_or("no prometheus package")?
        .to_owned();
    let nodes: BTreeMap<&str, &Value> = metadata["resolve"]["nodes"]
        .as_array()
        .ok_or("no resolve graph")?
        .iter()
        .filter_map(|n| Some((n["id"].as_str()?, n)))
        .collect();
    let mut edges = BTreeMap::new();
    let mut todo = vec![root.clone()];
    while let Some(id) = todo.pop() {
        if edges.contains_key(&id) {
            continue;
        }
        let deps: Vec<String> = nodes
            .get(id.as_str())
            .and_then(|n| n["deps"].as_array())
            .into_iter()
            .flatten()
            .filter(|d| is_normal(d))
            .filter_map(|d| d["pkg"].as_str().map(str::to_owned))
            .collect();
        todo.extend(deps.iter().cloned());
        edges.insert(id, deps);
    }
    Ok((root, edges))
}

fn is_normal(dep: &Value) -> bool {
    dep["dep_kinds"]
        .as_array()
        .is_some_and(|kinds| kinds.iter().any(|k| k["kind"].is_null()))
}

fn document(metadata: &Value, target: &str) -> Result<Value, String> {
    let (root, edges) = closure(metadata)?;
    let by_id: BTreeMap<&str, &Value> = metadata["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| Some((p["id"].as_str()?, p)))
        .collect();
    let ids: BTreeSet<&String> = edges.keys().collect();
    let spdx_id = |id: &str| {
        let p = by_id[id];
        let name = format!(
            "{}-{}",
            p["name"].as_str().unwrap_or("?"),
            p["version"].as_str().unwrap_or("?")
        );
        let clean: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        format!("SPDXRef-Package-{clean}")
    };
    let packages: Vec<Value> = ids
        .iter()
        .map(|id| package(by_id[id.as_str()], &spdx_id(id)))
        .collect();
    let mut relationships = vec![json!({
        "spdxElementId": "SPDXRef-DOCUMENT",
        "relationshipType": "DESCRIBES",
        "relatedSpdxElement": spdx_id(&root),
    })];
    for (from, deps) in &edges {
        for dep in deps {
            relationships.push(json!({
                "spdxElementId": spdx_id(from),
                "relationshipType": "DEPENDS_ON",
                "relatedSpdxElement": spdx_id(dep),
            }));
        }
    }
    let version = by_id[root.as_str()]["version"].as_str().unwrap_or("0");
    Ok(json!({
        "spdxVersion": "SPDX-2.3",
        "dataLicense": "CC0-1.0",
        "SPDXID": "SPDXRef-DOCUMENT",
        "name": format!("prometheus-{version}-{target}"),
        "documentNamespace": format!("https://github.com/CyberdyneCorp/prometheus-agent/spdx/prometheus-{version}-{target}"),
        "creationInfo": {
            "created": jiff::Timestamp::now().strftime("%Y-%m-%dT%H:%M:%SZ").to_string(),
            "creators": ["Tool: cargo-xtask-release"],
        },
        "packages": packages,
        "relationships": relationships,
    }))
}

fn package(p: &Value, spdx_id: &str) -> Value {
    let name = p["name"].as_str().unwrap_or("?");
    let version = p["version"].as_str().unwrap_or("?");
    let from_registry = p["source"]
        .as_str()
        .is_some_and(|s| s.starts_with("registry+"));
    let download = if from_registry {
        format!("https://crates.io/api/v1/crates/{name}/{version}/download")
    } else {
        "NOASSERTION".to_owned()
    };
    json!({
        "SPDXID": spdx_id,
        "name": name,
        "versionInfo": version,
        "downloadLocation": download,
        "filesAnalyzed": false,
        "licenseConcluded": "NOASSERTION",
        "licenseDeclared": p["license"].as_str().unwrap_or("NOASSERTION"),
        "copyrightText": "NOASSERTION",
        "externalRefs": [{
            "referenceCategory": "PACKAGE-MANAGER",
            "referenceType": "purl",
            "referenceLocator": format!("pkg:cargo/{name}@{version}"),
        }],
    })
}
