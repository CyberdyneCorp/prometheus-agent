//! The Bedrock model catalog from the control-plane API: the region's
//! foundation models (`GET /foundation-models`) and inference profiles
//! (`GET /inference-profiles`), signed like runtime calls. The control
//! plane reports no context windows, so each model's window comes from
//! its family; the listing (and so each window) is cached for
//! `providers.<id>.catalog_ttl_secs` by the metadata catalog.

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use serde_json::Value;

use super::Keys;
use super::sigv4::{Signing, sign};
use crate::metadata::listing::Listed;

/// Inference profile pages fetched at most (100 profiles each).
const MAX_PAGES: usize = 20;

/// The region's foundation models that stream text on demand, then its
/// active inference profiles.
///
/// # Errors
///
/// `provider.call_failed` naming the request that failed.
pub async fn list(
    client: &reqwest::Client,
    control: &str,
    region: &str,
    keys: &Keys,
) -> Result<Vec<Listed>, Error> {
    let base = control.trim_end_matches('/');
    let models = get(
        client,
        &format!("{base}/foundation-models?byOutputModality=TEXT"),
        region,
        keys,
    )
    .await?;
    let models: Vec<&Value> = models["modelSummaries"]
        .as_array()
        .map(|m| m.iter().collect())
        .unwrap_or_default();
    let mut listed: Vec<Listed> = models
        .iter()
        .filter(|m| on_demand(m))
        .filter_map(|m| Some(entry(m["modelId"].as_str()?, vision(m))))
        .collect();
    let mut next: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut url = format!("{base}/inference-profiles?maxResults=100");
        if let Some(token) = &next {
            url.push_str("&nextToken=");
            url.extend(percent_encoding::utf8_percent_encode(
                token,
                super::PATH_SEGMENT,
            ));
        }
        let page = get(client, &url, region, keys).await?;
        for profile in page["inferenceProfileSummaries"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|p| p["status"].as_str().is_none_or(|s| s == "ACTIVE"))
        {
            let target = backing_model(profile);
            let vision = models
                .iter()
                .find(|m| Some(m["modelId"].as_str().unwrap_or_default()) == target.as_deref())
                .is_some_and(|m| vision(m));
            if let Some(id) = profile["inferenceProfileId"].as_str() {
                listed.push(entry(id, vision));
            }
        }
        next = page["nextToken"].as_str().map(str::to_owned);
        if next.is_none() {
            break;
        }
    }
    Ok(listed)
}

async fn get(
    client: &reqwest::Client,
    url: &str,
    region: &str,
    keys: &Keys,
) -> Result<Value, Error> {
    let failed = |why: String| {
        Error::new(
            &codes::PROVIDER_CALL_FAILED,
            format!("bedrock: listing {url} failed: {why}"),
        )
    };
    let parsed = reqwest::Url::parse(url).map_err(|e| failed(e.to_string()))?;
    let headers = sign(
        &Signing {
            method: "GET",
            url: &parsed,
            headers: &[],
            body: b"",
            region,
            service: super::SERVICE,
            now: Timestamp::now(),
        },
        keys,
    );
    let mut request = client.get(parsed.clone());
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = request.send().await.map_err(|e| failed(e.to_string()))?;
    let status = response.status();
    let text = response.text().await.map_err(|e| failed(e.to_string()))?;
    if !status.is_success() {
        return Err(failed(format!(
            "HTTP {}: {}",
            status.as_u16(),
            crate::classify::body_message(&text)
        )));
    }
    serde_json::from_str(&text).map_err(|e| failed(e.to_string()))
}

/// Invocable without provisioning, with a streaming response.
fn on_demand(model: &Value) -> bool {
    let streams = model["responseStreamingSupported"].as_bool() != Some(false);
    let on_demand = model["inferenceTypesSupported"]
        .as_array()
        .is_none_or(|types| types.iter().any(|t| t == "ON_DEMAND"));
    streams && on_demand
}

fn vision(model: &Value) -> bool {
    model["inputModalities"]
        .as_array()
        .is_some_and(|m| m.iter().any(|v| v == "IMAGE"))
}

/// The foundation model id behind a profile (from its first model ARN).
fn backing_model(profile: &Value) -> Option<String> {
    let arn = profile.pointer("/models/0/modelArn")?.as_str()?;
    Some(arn.rsplit('/').next()?.to_owned())
}

fn entry(id: &str, vision: bool) -> Listed {
    Listed {
        id: id.to_owned(),
        context_window: context_window(id),
        supports_vision: Some(vision),
        ..Listed::default()
    }
}

/// Region prefixes of cross-region inference profiles.
const REGION_PREFIXES: &[&str] = &[
    "us-gov.", "global.", "apac.", "us.", "eu.", "jp.", "au.", "ca.",
];

/// Context windows of Bedrock model families, by the model name after the
/// vendor (`anthropic.`), longest prefix first.
const FAMILIES: &[(&str, u64)] = &[
    ("claude-", 200_000),
    ("nova-premier", 1_000_000),
    ("nova-pro", 300_000),
    ("nova-lite", 300_000),
    ("nova-micro", 128_000),
    ("nova-2-lite", 1_000_000),
    ("titan-text-premier", 32_000),
    ("titan-text", 8_000),
    ("llama4-scout", 3_500_000),
    ("llama4-maverick", 1_000_000),
    ("llama3", 128_000),
    ("mistral-large", 128_000),
    ("pixtral-large", 128_000),
    ("command-r", 128_000),
    ("r1", 128_000),
    ("jamba", 256_000),
    ("gpt-oss", 128_000),
    ("qwen3", 262_144),
];

/// The context window of a Bedrock model or inference profile id
/// (`eu.anthropic.claude-sonnet-4-5-20250929-v1:0`).
#[must_use]
pub fn context_window(id: &str) -> Option<u64> {
    let id = REGION_PREFIXES
        .iter()
        .find_map(|p| id.strip_prefix(p))
        .unwrap_or(id);
    let name = id.split_once('.').map_or(id, |(_, name)| name);
    FAMILIES
        .iter()
        .filter(|(prefix, _)| name.starts_with(prefix))
        .max_by_key(|(prefix, _)| prefix.len())
        .map(|(_, window)| *window)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_follow_the_family_behind_region_and_vendor_prefixes() {
        assert_eq!(
            context_window("eu.anthropic.claude-sonnet-4-5-20250929-v1:0"),
            Some(200_000)
        );
        assert_eq!(context_window("amazon.nova-pro-v1:0"), Some(300_000));
        assert_eq!(
            context_window("us.meta.llama3-3-70b-instruct-v1:0"),
            Some(128_000)
        );
        assert_eq!(context_window("cohere.embed-english-v3"), None);
    }
}
