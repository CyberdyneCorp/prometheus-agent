//! The Bedrock checks of the `providers` group (spec: model-providers,
//! Gemini native and AWS Bedrock providers): the region the provider
//! signs for, and which link of the AWS credential chain would supply
//! credentials. Offline only local sources are looked at (environment
//! secrets, the shared profile and its SSO cache, the container
//! endpoint's secrets); `--online` runs the whole chain, instance role
//! included.

use pmt_core::doctor::{Check, Status};
use pmt_providers::aws::chain::{self, Chain, Located};
use pmt_providers::{Provider, ProviderRegistry};

use super::Inputs;

/// The region and credential checks of every Bedrock provider the
/// profile routes to (main provider and fallback chain).
pub(super) async fn checks(inputs: &Inputs<'_>) -> Vec<Check> {
    let config = inputs.scope.config();
    let Ok(registry) = ProviderRegistry::for_profile(config, Vec::new()) else {
        return Vec::new();
    };
    let ids = config
        .model
        .provider
        .iter()
        .chain(config.fallback.chain.iter().map(|e| &e.provider));
    let mut seen: Vec<&str> = Vec::new();
    let mut checks = Vec::new();
    for id in ids {
        let Some(provider) = registry
            .lookup(id)
            .ok()
            .filter(|p| pmt_providers::aws::is_bedrock(p) && !seen.contains(&p.id()))
        else {
            continue;
        };
        seen.push(provider.id());
        checks.push(region(provider));
        checks.push(credentials(inputs, provider).await);
    }
    checks
}

fn region(provider: &Provider) -> Check {
    let region = pmt_providers::aws::region(provider);
    if pmt_providers::aws::valid_region(&region) {
        Check::new(
            "providers",
            "aws_region",
            Status::Ok,
            format!("{}: region {region}", provider.id()),
        )
    } else {
        Check::new(
            "providers",
            "aws_region",
            Status::Fail,
            format!("{}: '{region}' is not an AWS region", provider.id()),
        )
        .with_hint(format!(
            "prometheus config set providers.{}.region eu-west-1",
            provider.id()
        ))
    }
}

async fn credentials(inputs: &Inputs<'_>, provider: &Provider) -> Check {
    let id = provider.id();
    let secrets = inputs.scope.secrets().clone();
    let user_home = inputs.ctx.launch.user_home().cloned();
    let check =
        |status, message: String| Check::new("providers", "aws_credentials", status, message);
    let located = match chain::locate(provider, &secrets, user_home.as_deref()) {
        Ok(located) => located,
        Err(err) => return check(Status::Fail, format!("{id}: {}", err.message())),
    };
    match located {
        Located::Found(source) => check(
            Status::Ok,
            format!("{id}: AWS credentials from {}", source.describe()),
        ),
        Located::SsoLoginNeeded(profile) => check(
            Status::Warn,
            format!("{id}: the SSO sign-in of AWS profile '{profile}' expired or is missing"),
        )
        .with_hint(format!("aws sso login --profile {profile}")),
        Located::Nothing(profile) if !inputs.online => check(
            Status::Warn,
            format!(
                "{id}: no AWS credentials in the environment secrets or shared profile '{profile}'; the instance role is probed only with --online"
            ),
        )
        .with_hint(format!(
            "prometheus secrets set {}, or set providers.{id}.aws_profile",
            chain::ACCESS_KEY_ID
        )),
        Located::Nothing(_) => {
            let Ok(client) = crate::model_cmd::client(inputs.scope.config()) else {
                return check(Status::Fail, format!("{id}: no HTTP client"));
            };
            match Chain::new(client, user_home).keys(provider, &secrets).await {
                Ok((_, source)) => check(
                    Status::Ok,
                    format!("{id}: AWS credentials from {}", source.describe()),
                ),
                Err(err) => {
                    let mut failed = check(Status::Fail, err.message().to_owned());
                    if let Some(hint) = err.hint() {
                        failed = failed.with_hint(hint.to_owned());
                    }
                    failed
                }
            }
        }
    }
}
