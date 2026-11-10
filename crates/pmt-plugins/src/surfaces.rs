//! What plugins register beyond tools, hooks and prompt contributions
//! (spec: plugin-system, Plugin CLI subcommands; Approval transports;
//! Model-provider plugins): `prometheus plugins run` commands (hook world
//! 1.2), approval transports (hook world 1.2) and model providers
//! (provider world 1.1). Each needs its grant: a name the grant does not
//! cover is refused at registration with a warning on the plugin's
//! status, and the plugin otherwise loads.

use std::sync::Arc;

use pmt_config::schema::{LoginFlow, ProviderAuth, WireFormat};
use pmt_core::capability::{Capability, CapabilitySet, PluginRight};
use pmt_providers::descriptor::is_provider_id;
use pmt_providers::descriptor::{Descriptor, PluginOAuth};
use serde::Deserialize;

use crate::backend::Backend;
use crate::host::Instance;
use crate::host::surfaces_world::CliCommand;

/// One `prometheus plugins run <plugin> <name>` command.
#[derive(Debug, Clone)]
pub struct CliEntry {
    /// The plugin.
    pub plugin: String,
    /// The command name.
    pub name: String,
    /// One line of help.
    pub description: String,
    /// The instance that runs it.
    pub instance: Arc<Instance>,
}

/// One registered approval transport (`<plugin>:<name>`).
#[derive(Debug, Clone)]
pub struct Transport {
    /// The plugin.
    pub plugin: String,
    /// The transport name.
    pub name: String,
    /// What presents requests.
    pub backend: Arc<dyn Backend>,
}

/// One model provider a plugin registered.
#[derive(Debug, Clone)]
pub struct ModelProvider {
    /// The plugin.
    pub plugin: String,
    /// Its descriptor.
    pub descriptor: Descriptor,
    /// It replaces the built-in provider of the same id
    /// (`providers_override` granted).
    pub replaces_builtin: bool,
    /// What its request hooks run in.
    pub backend: Arc<dyn Backend>,
}

/// Every plugin's commands, transports and model providers.
#[derive(Debug, Clone, Default)]
pub struct Surfaces {
    /// `prometheus plugins run` commands, in load order.
    pub cli: Vec<CliEntry>,
    /// Approval transports.
    pub transports: Vec<Transport>,
    /// Model providers.
    pub models: Vec<ModelProvider>,
}

fn grant(right: PluginRight, target: &str) -> Capability {
    Capability::Plugin {
        right,
        target: Some(target.to_owned()),
    }
}

impl Surfaces {
    /// Registers `commands`; returns warnings for names outside the
    /// `cli_commands` grant (refused).
    pub fn add_cli(
        &mut self,
        instance: &Arc<Instance>,
        commands: Vec<CliCommand>,
        granted: &CapabilitySet,
    ) -> Vec<String> {
        let mut warnings = Vec::new();
        for command in commands {
            if !granted.allows(&grant(PluginRight::CliCommand, &command.name)) {
                warnings.push(format!(
                    "capability_denied: cli_commands {name}; `prometheus plugins run {} {name}` is not registered",
                    instance.id(),
                    name = command.name
                ));
                continue;
            }
            self.cli.push(CliEntry {
                plugin: instance.id().to_owned(),
                name: command.name,
                description: command.description,
                instance: Arc::clone(instance),
            });
        }
        warnings
    }

    /// Registers `names` as approval transports when the plugin holds
    /// `approval_transport`.
    pub fn add_transports(
        &mut self,
        backend: &Arc<dyn Backend>,
        names: Vec<String>,
        granted: &CapabilitySet,
    ) -> Vec<String> {
        if names.is_empty() {
            return Vec::new();
        }
        let right = Capability::Plugin {
            right: PluginRight::ApprovalTransport,
            target: None,
        };
        if !granted.allows(&right) {
            return vec![format!(
                "capability_denied: approval_transport; transports {} are not registered",
                names.join(", ")
            )];
        }
        self.transports
            .extend(names.into_iter().map(|name| Transport {
                plugin: backend.id().to_owned(),
                name,
                backend: Arc::clone(backend),
            }));
        Vec::new()
    }

    /// The transport `plugins.approval_transport` names
    /// (`<plugin_id>:<name>`).
    #[must_use]
    pub fn transport(&self, selected: &str) -> Option<&Transport> {
        let (plugin, name) = selected.split_once(':')?;
        self.transports
            .iter()
            .find(|t| t.plugin == plugin && t.name == name)
    }

    /// Registers model providers from `descriptors` (JSON objects). A
    /// built-in id (`builtin` lists the built-in ids and aliases) needs
    /// `providers_override`; any other id needs `providers`, and a later
    /// plugin cannot take an id an earlier one registered. Refusals are
    /// returned as warnings and the built-in stays active.
    pub fn add_models(
        &mut self,
        backend: &Arc<dyn Backend>,
        descriptors: &[String],
        granted: &CapabilitySet,
        builtin: &[String],
    ) -> Vec<String> {
        let mut warnings = Vec::new();
        for text in descriptors {
            match self.model(backend, text, granted, builtin) {
                Ok(provider) => self.models.push(provider),
                Err(why) => warnings.push(why),
            }
        }
        warnings
    }

    fn model(
        &self,
        backend: &Arc<dyn Backend>,
        text: &str,
        granted: &CapabilitySet,
        builtin: &[String],
    ) -> Result<ModelProvider, String> {
        let plugin = backend.id();
        let descriptor = parse_descriptor(text)
            .map_err(|why| format!("model provider descriptor refused: {why}"))?;
        let id = descriptor.id.clone();
        let names: Vec<&String> = std::iter::once(&id).chain(&descriptor.aliases).collect();
        if !granted.allows(&grant(PluginRight::Provider, &id)) {
            return Err(format!(
                "capability_denied: providers {id}; model provider '{id}' is not registered"
            ));
        }
        let replaces_builtin = builtin.contains(&id);
        if replaces_builtin && !granted.allows(&grant(PluginRight::ProviderOverride, &id)) {
            return Err(format!(
                "model provider '{id}' is a built-in provider; replacing it needs the providers_override grant, so the built-in '{id}' stays active"
            ));
        }
        // A replacement keeps the names of the provider it replaces.
        let replaced = if replaces_builtin {
            builtin_aliases(&id)
        } else {
            Vec::new()
        };
        if let Some(name) = names
            .iter()
            .find(|n| builtin.contains(n) && **n != &id && !replaced.contains(n))
        {
            return Err(format!(
                "model provider '{id}': alias '{name}' is a built-in provider name"
            ));
        }
        if let Some(owner) = self.models.iter().find(|m| {
            names
                .iter()
                .any(|n| m.descriptor.id == **n || m.descriptor.aliases.contains(n))
        }) {
            return Err(format!(
                "model provider '{id}' is already registered by plugin '{}'",
                owner.plugin
            ));
        }
        Ok(ModelProvider {
            plugin: plugin.to_owned(),
            descriptor,
            replaces_builtin,
            backend: Arc::clone(backend),
        })
    }
}

/// The provider caller's request hooks over the plugins' model providers
/// (header building and request preparation run in the plugin; the HTTP
/// request stays in the host).
#[derive(Debug, Clone)]
pub struct ModelRequestHooks {
    models: Vec<ModelProvider>,
}

impl ModelRequestHooks {
    /// Hooks of `models`.
    #[must_use]
    pub fn new(models: Vec<ModelProvider>) -> Self {
        Self { models }
    }

    fn backend(&self, provider: &str) -> Option<Arc<dyn Backend>> {
        self.models
            .iter()
            .find(|m| m.descriptor.id == provider)
            .map(|m| Arc::clone(&m.backend))
    }
}

/// Runs a plugin call off the async workers.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<Result<T, String>, pmt_core::error::Error> + Send + 'static,
) -> Result<T, String> {
    match pmt_core::spawn::blocking(work).await {
        Ok(Ok(answer)) => answer,
        Ok(Err(err)) => Err(err.message().to_owned()),
        Err(join) => Err(join.to_string()),
    }
}

impl pmt_providers::transport::RequestHooks for ModelRequestHooks {
    fn handles(&self, provider: &str) -> bool {
        self.models.iter().any(|m| m.descriptor.id == provider)
    }

    fn headers<'a>(
        &'a self,
        provider: &'a str,
    ) -> pmt_providers::resolve::BoxFuture<'a, Result<Vec<(String, String)>, String>> {
        Box::pin(async move {
            let backend = self.backend(provider).ok_or("no plugin provider")?;
            let id = provider.to_owned();
            blocking(move || backend.model_headers(&crate::host::CallInfo::default(), &id)).await
        })
    }

    fn prepare<'a>(
        &'a self,
        provider: &'a str,
        body: serde_json::Value,
    ) -> pmt_providers::resolve::BoxFuture<'a, Result<serde_json::Value, String>> {
        Box::pin(async move {
            let backend = self.backend(provider).ok_or("no plugin provider")?;
            let id = provider.to_owned();
            let text = body.to_string();
            let prepared = blocking(move || {
                backend.prepare_request(&crate::host::CallInfo::default(), &id, &text)
            })
            .await?;
            serde_json::from_str(&prepared)
                .map_err(|e| format!("prepare-request answered invalid JSON: {e}"))
        })
    }
}

/// The descriptor fields a plugin sets; the rest keep their contract
/// defaults.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDescriptor {
    id: String,
    wire_format: WireFormat,
    base_url: Option<String>,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    secret_names: Vec<String>,
    #[serde(default)]
    key_hosts: Vec<String>,
    default_model: Option<String>,
    models_url: Option<String>,
    #[serde(default)]
    supports_model_listing: bool,
    /// An OAuth PKCE sign-in the host runs for the plugin.
    oauth: Option<RawOAuth>,
    /// `external_process`: the agent CLI the host drives over ACP once
    /// the user names it in `providers.<id>.command` (only suggested here).
    command: Option<String>,
}

/// A plugin's OAuth PKCE sign-in (spec: model-providers, Plugin OAuth and
/// external-process providers): only public metadata; the host keeps the
/// verifier, `state` and tokens.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOAuth {
    client_id: String,
    authorize_url: String,
    token_url: String,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    allowed_hosts: Vec<String>,
    #[serde(default)]
    redirect_port: u16,
    redirect_path: Option<String>,
    #[serde(default)]
    authorize_params: std::collections::BTreeMap<String, String>,
}

/// Parses and validates one descriptor a plugin registered.
///
/// # Errors
///
/// What is wrong with it.
pub fn parse_descriptor(text: &str) -> Result<Descriptor, String> {
    let raw: RawDescriptor = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if !is_provider_id(&raw.id) {
        return Err(format!("'{}' is not a valid provider id", raw.id));
    }
    let mut descriptor = Descriptor::new(&raw.id, raw.wire_format, raw.base_url.as_deref());
    descriptor.aliases = raw.aliases;
    descriptor.secret_names = raw.secret_names;
    descriptor.key_hosts = raw.key_hosts;
    descriptor.default_model = raw.default_model;
    descriptor.models_url = raw.models_url;
    descriptor.supports_model_listing = raw.supports_model_listing;
    if let Some(oauth) = raw.oauth {
        descriptor.auth = ProviderAuth::Oauth;
        descriptor.login_flows = vec![LoginFlow::Browser];
        descriptor.oauth = Some(PluginOAuth {
            client_id: oauth.client_id,
            authorize_url: oauth.authorize_url,
            token_url: oauth.token_url,
            scopes: oauth.scopes,
            allowed_hosts: oauth.allowed_hosts,
            redirect_port: oauth.redirect_port,
            redirect_path: oauth.redirect_path,
            authorize_params: oauth.authorize_params.into_iter().collect(),
        });
    }
    if raw.command.is_some() {
        descriptor.auth = ProviderAuth::ExternalProcess;
        descriptor.supports_model_listing = false;
        descriptor.command = raw.command;
    }
    descriptor.validate().map_err(|e| e.message().to_owned())?;
    Ok(descriptor)
}

/// Every built-in provider id and alias.
#[must_use]
pub fn builtin_names() -> Vec<String> {
    pmt_providers::builtin::descriptors()
        .into_iter()
        .flat_map(|d| std::iter::once(d.id).chain(d.aliases))
        .collect()
}

/// The aliases of built-in provider `id`.
fn builtin_aliases(id: &str) -> Vec<String> {
    pmt_providers::builtin::descriptors()
        .into_iter()
        .find(|d| d.id == id)
        .map(|d| d.aliases)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_descriptor_needs_a_valid_id_and_known_fields() {
        let good = r#"{"id":"acme","wire_format":"chat_completions","base_url":"https://a.example/v1","key_hosts":["a.example"]}"#;
        let descriptor = parse_descriptor(good).unwrap();
        assert_eq!(descriptor.id, "acme");
        assert_eq!(descriptor.key_hosts, vec!["a.example".to_owned()]);
        assert!(parse_descriptor(r#"{"id":"Bad Id","wire_format":"chat_completions"}"#).is_err());
        assert!(
            parse_descriptor(r#"{"id":"x","wire_format":"chat_completions","oops":1}"#).is_err()
        );
    }

    #[test]
    fn openrouter_is_a_builtin_name() {
        assert!(builtin_names().contains(&"openrouter".to_owned()));
    }
}
