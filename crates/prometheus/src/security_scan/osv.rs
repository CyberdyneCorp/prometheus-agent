//! The OSV client of `prometheus security scan`: one `querybatch` for
//! every package, then each advisory's record for its severity and fixed
//! version.

use std::collections::BTreeMap;
use std::time::Duration;

use pmt_core::error::{Error, codes};
use serde::Serialize;
use serde_json::{Value, json};

use super::components::Package;

/// Advisory severity, lowest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Low.
    Low,
    /// Moderate (OSV's `MEDIUM`).
    Moderate,
    /// High.
    High,
    /// Critical.
    Critical,
}

impl Severity {
    /// The severity an advisory record states; one without any is
    /// counted as moderate, so it is never silently below a gate.
    fn of(record: &Value) -> Self {
        let label = record["database_specific"]["severity"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_uppercase();
        match label.as_str() {
            "CRITICAL" => Self::Critical,
            "HIGH" => Self::High,
            "LOW" => Self::Low,
            _ => Self::Moderate,
        }
    }

    /// The wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Moderate => "moderate",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

/// One advisory against one package.
#[derive(Debug, Clone, Serialize)]
pub(super) struct Finding {
    /// The component that holds the package.
    pub component: String,
    /// The package.
    pub package: String,
    /// Its version.
    pub version: String,
    /// The advisory id.
    pub id: String,
    /// Its severity.
    pub severity: Severity,
    /// The first fixed version, when the advisory names one.
    pub fixed: Option<String>,
    /// Its one-line summary.
    pub summary: String,
}

/// Queries OSV at `base` for `packages`.
///
/// # Errors
///
/// `security.osv_unreachable` (69) when OSV does not answer.
pub(super) async fn findings(base: &str, packages: &[Package]) -> Result<Vec<Finding>, Error> {
    if packages.is_empty() {
        return Ok(Vec::new());
    }
    let client = pmt_net::http::client_builder(&pmt_net::http::OutboundPolicy::default())?
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| unreachable(base, &e.to_string()))?;
    let base = base.trim_end_matches('/');
    let queries: Vec<Value> = packages
        .iter()
        .map(|p| json!({"package": {"name": p.name, "ecosystem": p.ecosystem}, "version": p.version}))
        .collect();
    let batch: Value = post(
        &client,
        &format!("{base}/v1/querybatch"),
        &json!({"queries": queries}),
    )
    .await?;
    let mut records: BTreeMap<String, Value> = BTreeMap::new();
    let mut found = Vec::new();
    let results = batch["results"].as_array().cloned().unwrap_or_default();
    for (package, result) in packages.iter().zip(results) {
        for vuln in result["vulns"].as_array().into_iter().flatten() {
            let Some(id) = vuln["id"].as_str() else {
                continue;
            };
            if !records.contains_key(id) {
                let record = get(&client, &format!("{base}/v1/vulns/{id}")).await?;
                records.insert(id.to_owned(), record);
            }
            let record = &records[id];
            found.push(Finding {
                component: package.component.clone(),
                package: package.name.clone(),
                version: package.version.clone(),
                id: id.to_owned(),
                severity: Severity::of(record),
                fixed: fixed(record, package),
                summary: record["summary"].as_str().unwrap_or_default().to_owned(),
            });
        }
    }
    Ok(found)
}

/// The first `fixed` event of the record's range for `package`.
fn fixed(record: &Value, package: &Package) -> Option<String> {
    record["affected"]
        .as_array()?
        .iter()
        .filter(|a| a["package"]["name"].as_str() == Some(&package.name))
        .flat_map(|a| a["ranges"].as_array().cloned().unwrap_or_default())
        .flat_map(|r| r["events"].as_array().cloned().unwrap_or_default())
        .find_map(|e| e["fixed"].as_str().map(str::to_owned))
}

async fn post(client: &reqwest::Client, url: &str, body: &Value) -> Result<Value, Error> {
    let response = client
        .post(url)
        .json(body)
        .send()
        .await
        .map_err(|e| unreachable(url, &e.to_string()))?;
    answer(url, response).await
}

async fn get(client: &reqwest::Client, url: &str) -> Result<Value, Error> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|e| unreachable(url, &e.to_string()))?;
    answer(url, response).await
}

async fn answer(url: &str, response: reqwest::Response) -> Result<Value, Error> {
    if !response.status().is_success() {
        return Err(unreachable(url, &format!("HTTP {}", response.status())));
    }
    response
        .json()
        .await
        .map_err(|e| unreachable(url, &e.to_string()))
}

fn unreachable(url: &str, why: &str) -> Error {
    Error::new(
        &codes::SECURITY_OSV_UNREACHABLE,
        format!("the OSV database is unreachable ({url}): {why}"),
    )
}
