//! Azure AI Foundry (spec: model-providers, Azure AI Foundry provider):
//! Microsoft Entra ID tokens for `providers.azure-foundry.auth =
//! "entra_id"` (the Azure CLI login, else a managed identity), and the
//! resource's deployment list, whose metadata (the model a deployment
//! serves) picks the wire format and fills the model picker.

use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use pmt_config::schema::WireFormat;
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use serde_json::Value;

use crate::metadata::listing::Listed;

/// The scope Foundry inference tokens are minted for (every endpoint
/// shape: OpenAI-style and Anthropic-style deployments).
pub const SCOPE: &str = "https://ai.azure.com/.default";
/// The managed identity resource of [`SCOPE`].
const RESOURCE: &str = "https://ai.azure.com";
/// The instance metadata service managed identities answer on.
pub const IMDS: &str = "http://169.254.169.254";
/// Longest an `az` call may run.
const CLI_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest the managed identity endpoint may take.
const IMDS_TIMEOUT: Duration = Duration::from_secs(2);
/// The deployment list API version of Azure OpenAI resources.
const DEPLOYMENTS_API: &str = "2022-12-01";

/// Where Entra ID tokens come from.
#[derive(Debug, Clone)]
pub struct EntraSources {
    /// The Azure CLI; `None` looks `az` up on `PATH` and the usual
    /// install prefixes.
    pub az: Option<PathBuf>,
    /// The managed identity endpoint; `None` skips it.
    pub imds: Option<String>,
}

impl Default for EntraSources {
    fn default() -> Self {
        Self {
            az: None,
            imds: Some(IMDS.to_owned()),
        }
    }
}

/// A Microsoft Entra ID access token for `provider` and its lifetime:
/// from `az account get-access-token`, else the managed identity.
///
/// # Errors
///
/// `provider.missing_credential` naming both sources when neither gave a
/// token.
pub async fn entra_token(
    client: &reqwest::Client,
    sources: &EntraSources,
    user_home: Option<&Path>,
    provider: &str,
) -> Result<(SecretString, Option<SignedDuration>), Error> {
    let cli = match cli_token(sources, user_home).await {
        Ok(token) => return Ok(token),
        Err(why) => why,
    };
    let identity = match &sources.imds {
        Some(imds) => match identity_token(client, imds).await {
            Ok(token) => return Ok(token),
            Err(why) => why,
        },
        None => "not asked".to_owned(),
    };
    Err(Error::new(
        &codes::PROVIDER_MISSING_CREDENTIAL,
        format!(
            "{provider}: no Microsoft Entra ID token (Azure CLI: {cli}; managed identity: {identity})"
        ),
    )
    .with_hint(format!(
        "run az login, or store AZURE_FOUNDRY_API_KEY and unset providers.{provider}.auth"
    )))
}

async fn cli_token(
    sources: &EntraSources,
    user_home: Option<&Path>,
) -> Result<(SecretString, Option<SignedDuration>), String> {
    let program = sources
        .az
        .clone()
        .or_else(|| crate::external::locate("az", std::env::var_os("PATH"), user_home))
        .ok_or_else(|| "az not found".to_owned())?;
    let mut command = tokio::process::Command::new(program);
    command
        .args([
            "account",
            "get-access-token",
            "--scope",
            SCOPE,
            "--output",
            "json",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = tokio::time::timeout(CLI_TIMEOUT, command.output())
        .await
        .map_err(|_| "az did not answer within 30 s".to_owned())?
        .map_err(|e| format!("az cannot run: {e}"))?;
    if !output.status.success() {
        return Err(format!("az exited with {} (run az login)", output.status));
    }
    let body: Value =
        serde_json::from_slice(&output.stdout).map_err(|_| "az printed no JSON".to_owned())?;
    let token = body["accessToken"]
        .as_str()
        .ok_or_else(|| "az printed no accessToken".to_owned())?;
    let expires_on = body["expires_on"]
        .as_i64()
        .or_else(|| body["expires_on"].as_str()?.parse().ok());
    let lifetime = expires_on
        .and_then(|at| Timestamp::from_second(at).ok())
        .map(|at| at.duration_since(Timestamp::now()));
    Ok((SecretString::new(token), lifetime))
}

async fn identity_token(
    client: &reqwest::Client,
    imds: &str,
) -> Result<(SecretString, Option<SignedDuration>), String> {
    let response = client
        .get(format!(
            "{imds}/metadata/identity/oauth2/token?api-version=2018-02-01&resource={}",
            percent_encoding::utf8_percent_encode(RESOURCE, percent_encoding::NON_ALPHANUMERIC)
        ))
        .header("metadata", "true")
        .timeout(IMDS_TIMEOUT)
        .send()
        .await
        .map_err(|e| format!("no answer ({e})"))?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status().as_u16()));
    }
    let text = response.text().await.map_err(|e| e.to_string())?;
    let body: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let token = body["access_token"]
        .as_str()
        .ok_or_else(|| "no access_token".to_owned())?;
    let expires_in = body["expires_in"]
        .as_i64()
        .or_else(|| body["expires_in"].as_str()?.parse().ok());
    Ok((
        SecretString::new(token),
        expires_in.map(SignedDuration::from_secs),
    ))
}

/// How a deployment list request authenticates.
#[derive(Debug, Clone)]
pub enum Auth {
    /// `AZURE_FOUNDRY_API_KEY`, in the `api-key` header.
    Key(SecretString),
    /// An Entra ID token.
    Bearer(SecretString),
}

/// One deployment of a Foundry resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deployment {
    /// The deployment name requests address.
    pub name: String,
    /// The model it serves (`gpt-4o`, `claude-sonnet-4-5`).
    pub model: String,
}

/// The resource's deployment endpoint under `base_url`: a Foundry
/// project's `deployments`, else the resource's Azure OpenAI deployment
/// list.
#[must_use]
pub fn deployments_url(base_url: &str) -> Option<String> {
    let url = reqwest::Url::parse(base_url).ok()?;
    let origin = url.origin().ascii_serialization();
    let path = url.path();
    if let Some(at) = path.find("/api/projects/") {
        let rest = &path[at + "/api/projects/".len()..];
        let project = rest.split('/').next().filter(|p| !p.is_empty())?;
        return Some(format!(
            "{origin}{}{project}/deployments?api-version=v1",
            &path[..at + "/api/projects/".len()]
        ));
    }
    Some(format!(
        "{origin}/openai/deployments?api-version={DEPLOYMENTS_API}"
    ))
}

/// The resource's deployments.
///
/// # Errors
///
/// `provider.call_failed` when the list cannot be read.
pub async fn deployments(
    client: &reqwest::Client,
    base_url: &str,
    auth: &Auth,
) -> Result<Vec<Deployment>, Error> {
    let failed = |why: String| {
        Error::new(
            &codes::PROVIDER_CALL_FAILED,
            format!("azure-foundry: the deployment list failed: {why}"),
        )
    };
    let url = deployments_url(base_url).ok_or_else(|| failed(format!("bad URL {base_url}")))?;
    let request = match auth {
        Auth::Key(key) => client.get(url).header("api-key", key.expose()),
        Auth::Bearer(token) => client.get(url).bearer_auth(token.expose()),
    };
    let response = request.send().await.map_err(|e| failed(e.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(failed(format!("HTTP {}", status.as_u16())));
    }
    let text = response.text().await.map_err(|e| failed(e.to_string()))?;
    let body: Value = serde_json::from_str(&text).map_err(|e| failed(e.to_string()))?;
    Ok(parse(&body))
}

/// An Azure OpenAI list (`data[].id`/`model`) or a Foundry project list
/// (`value[].name`/`modelName`).
fn parse(body: &Value) -> Vec<Deployment> {
    let items = body
        .get("data")
        .or_else(|| body.get("value"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    items
        .iter()
        .filter_map(|item| {
            let name = item.get("id").or_else(|| item.get("name"))?.as_str()?;
            let model = item
                .get("model")
                .or_else(|| item.get("modelName"))
                .and_then(Value::as_str)
                .unwrap_or(name);
            Some(Deployment {
                name: name.to_owned(),
                model: model.to_owned(),
            })
        })
        .collect()
}

/// The wire a deployment serving `model` needs: Anthropic Messages for
/// Claude, Responses for the OpenAI models Azure serves only there
/// (GPT-5, codex, o-series), else Chat Completions.
#[must_use]
pub fn wire_for(model: &str) -> WireFormat {
    let model = model.to_ascii_lowercase();
    let responses_only = ["gpt-5", "codex", "o1", "o3", "o4"]
        .iter()
        .any(|prefix| model.starts_with(prefix))
        || model.contains("codex");
    if model.contains("claude") {
        WireFormat::AnthropicMessages
    } else if responses_only {
        WireFormat::Responses
    } else {
        WireFormat::ChatCompletions
    }
}

/// The resource's Anthropic-style endpoint (`<origin>/anthropic`).
#[must_use]
pub fn anthropic_base(base_url: &str) -> String {
    reqwest::Url::parse(base_url).map_or_else(
        |_| base_url.to_owned(),
        |url| format!("{}/anthropic", url.origin().ascii_serialization()),
    )
}

/// The deployments as model picker entries (context windows by family).
#[must_use]
pub fn listed(deployments: &[Deployment]) -> Vec<Listed> {
    deployments
        .iter()
        .map(|d| Listed {
            id: d.name.clone(),
            context_window: crate::metadata::window::family_window(&d.model),
            ..Listed::default()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn claude_deployments_use_anthropic_messages() {
        assert_eq!(wire_for("claude-sonnet-4-5"), WireFormat::AnthropicMessages);
        assert_eq!(wire_for("gpt-5-mini"), WireFormat::Responses);
        assert_eq!(wire_for("o4-mini"), WireFormat::Responses);
        assert_eq!(wire_for("gpt-4o"), WireFormat::ChatCompletions);
        assert_eq!(wire_for("DeepSeek-R1"), WireFormat::ChatCompletions);
    }

    #[test]
    fn deployment_lists_of_both_shapes_are_read() {
        let openai = json!({"data": [{"id": "chat", "model": "gpt-4o"}]});
        let project = json!({"value": [{"name": "sonnet", "modelName": "claude-sonnet-4-5"}]});
        assert_eq!(
            parse(&openai),
            vec![Deployment {
                name: "chat".into(),
                model: "gpt-4o".into()
            }]
        );
        assert_eq!(parse(&project)[0].model, "claude-sonnet-4-5");
        assert_eq!(
            deployments_url("https://r.openai.azure.com/openai/v1").unwrap(),
            "https://r.openai.azure.com/openai/deployments?api-version=2022-12-01"
        );
        assert_eq!(
            deployments_url("https://r.services.ai.azure.com/api/projects/p1/openai/v1").unwrap(),
            "https://r.services.ai.azure.com/api/projects/p1/deployments?api-version=v1"
        );
        assert_eq!(
            anthropic_base("https://r.services.ai.azure.com/openai/v1"),
            "https://r.services.ai.azure.com/anthropic"
        );
    }
}
