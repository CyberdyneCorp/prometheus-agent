//! The `homeassistant` toolset of the Home Assistant catalog adapter (spec:
//! platform-adapters, Home Assistant adapter and toolset): entities,
//! states and services over the REST API at the platform's `options.url`
//! with the `HASS_TOKEN` secret. `entity_id` must look like
//! `<domain>.<object>`, and services of the domains that run code or
//! manage the host (`shell_command`, `command_line`, `python_script`,
//! `pyscript`, `hassio`, `rest_command`) are refused.

use std::sync::Arc;

use pmt_tools::descriptor::{CallContext, HandlerFuture, Owner, ToolHandler};
use pmt_tools::platform_tools::{Context, Contributor, SecretReader};
use pmt_tools::{RegisteredTool, ToolDescriptor, ToolFailure, ToolResult};
use serde_json::{Value, json};

/// The platform (the catalog plugin's id).
pub const PLATFORM: &str = "homeassistant";
/// The access token's secret.
pub const TOKEN: &str = "HASS_TOKEN";
/// Where Home Assistant is unless `options.url` says otherwise.
pub const DEFAULT_URL: &str = "http://homeassistant.local:8123";
/// Service domains no call may reach.
pub const BLOCKED_DOMAINS: &[&str] = &[
    "shell_command",
    "command_line",
    "python_script",
    "pyscript",
    "hassio",
    "rest_command",
];

/// Which tool a handler is.
#[derive(Debug, Clone, Copy)]
enum Kind {
    ListEntities,
    GetState,
    ListServices,
    CallService,
}

fn invalid(message: impl Into<String>) -> ToolFailure {
    ToolFailure::new("invalid_input", message)
}

/// `^[a-z_][a-z0-9_]*$`: a domain or service name.
fn is_name(text: &str) -> bool {
    let mut bytes = text.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_lowercase() || b == b'_')
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// `^[a-z_][a-z0-9_]*\.[a-z0-9_]+$`.
fn is_entity_id(text: &str) -> bool {
    text.split_once('.').is_some_and(|(domain, object)| {
        is_name(domain)
            && !object.is_empty()
            && object
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    })
}

fn name_argument<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolFailure> {
    let value = args[key]
        .as_str()
        .ok_or_else(|| invalid(format!("{key} is required")))?;
    if is_name(value) {
        Ok(value)
    } else {
        Err(invalid(format!("{key} '{value}' is not a valid name")))
    }
}

fn entity_argument(args: &Value) -> Result<Option<&str>, ToolFailure> {
    match args["entity_id"].as_str() {
        None => Ok(None),
        Some(id) if is_entity_id(id) => Ok(Some(id)),
        Some(id) => Err(invalid(format!(
            "entity_id '{id}' must match ^[a-z_][a-z0-9_]*\\.[a-z0-9_]+$"
        ))),
    }
}

/// The checked service call: its path and body.
fn service_call(args: &Value) -> Result<(String, Value), ToolFailure> {
    let domain = name_argument(args, "domain")?;
    if BLOCKED_DOMAINS.contains(&domain) {
        return Err(ToolFailure::new(
            "blocked",
            format!("services of domain '{domain}' are blocked: they run code or manage the host"),
        ));
    }
    let service = name_argument(args, "service")?;
    let mut body = match &args["data"] {
        Value::Object(data) => Value::Object(data.clone()),
        Value::Null => json!({}),
        _ => return Err(invalid("data must be an object")),
    };
    if let Some(entity) = entity_argument(args)? {
        body["entity_id"] = json!(entity);
    }
    Ok((format!("/api/services/{domain}/{service}"), body))
}

struct Handler {
    kind: Kind,
    url: String,
    secrets: SecretReader,
    client: reqwest::Client,
}

impl Handler {
    async fn call_api(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, ToolFailure> {
        let token = (self.secrets)(TOKEN).ok_or_else(|| {
            ToolFailure::new("missing_secret", format!("secret {TOKEN} is not set"))
        })?;
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(token)
            .timeout(std::time::Duration::from_secs(30));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|e| {
            ToolFailure::new("network", format!("Home Assistant is unreachable: {e}"))
        })?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(ToolFailure::new(
                "homeassistant",
                format!(
                    "Home Assistant answered {status}: {}",
                    text.chars().take(300).collect::<String>()
                ),
            ));
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }

    async fn list_entities(&self, args: &Value) -> Result<Value, ToolFailure> {
        let domain = match args["domain"].as_str() {
            Some(_) => Some(name_argument(args, "domain")?),
            None => None,
        };
        let in_area: Option<Vec<String>> = match args["area"].as_str() {
            Some(area) => {
                // The area goes into a template: letters, digits, spaces,
                // `_` and `-` only, so it cannot leave its string.
                if area.is_empty()
                    || !area
                        .chars()
                        .all(|c| c.is_alphanumeric() || " _-".contains(c))
                {
                    return Err(invalid(format!("area '{area}' is not an area id or name")));
                }
                let template = json!({"template": format!("{{{{ area_entities({}) | tojson }}}}", json!(area))});
                let answer = self
                    .call_api(reqwest::Method::POST, "/api/template", Some(template))
                    .await?;
                let list = match answer {
                    Value::String(text) => serde_json::from_str(&text).unwrap_or_default(),
                    other => serde_json::from_value(other).unwrap_or_default(),
                };
                Some(list)
            }
            None => None,
        };
        let states = self
            .call_api(reqwest::Method::GET, "/api/states", None)
            .await?;
        let entities: Vec<Value> = states
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| {
                let id = s["entity_id"].as_str().unwrap_or_default();
                domain.is_none_or(|d| id.split('.').next() == Some(d))
                    && in_area
                        .as_ref()
                        .is_none_or(|ids| ids.iter().any(|i| i == id))
            })
            .map(|s| {
                json!({
                    "entity_id": s["entity_id"],
                    "state": s["state"],
                    "name": s["attributes"]["friendly_name"],
                })
            })
            .collect();
        Ok(Value::Array(entities))
    }

    async fn run(&self, args: Value) -> Result<ToolResult, ToolFailure> {
        let answer = match self.kind {
            Kind::ListEntities => self.list_entities(&args).await?,
            Kind::GetState => {
                let id = entity_argument(&args)?.ok_or_else(|| invalid("entity_id is required"))?;
                self.call_api(reqwest::Method::GET, &format!("/api/states/{id}"), None)
                    .await?
            }
            Kind::ListServices => {
                let all = self
                    .call_api(reqwest::Method::GET, "/api/services", None)
                    .await?;
                match args["domain"].as_str() {
                    Some(domain) => Value::Array(
                        all.as_array()
                            .into_iter()
                            .flatten()
                            .filter(|d| d["domain"] == domain)
                            .cloned()
                            .collect(),
                    ),
                    None => all,
                }
            }
            Kind::CallService => {
                let (path, body) = service_call(&args)?;
                self.call_api(reqwest::Method::POST, &path, Some(body))
                    .await?
            }
        };
        Ok(ToolResult::ok(answer.to_string()))
    }
}

impl ToolHandler for Handler {
    fn call<'a>(&'a self, _: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(self.run(arguments))
    }
}

/// The toolset's tools for every profile that enables the platform.
#[derive(Debug, Clone, Copy, Default)]
pub struct Tools;

fn schema(properties: &Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": properties, "required": required})
}

/// Each tool: name, description, schema, kind, external effect.
fn catalog() -> [(&'static str, &'static str, Value, Kind, bool); 4] {
    let entity = json!({"type": "string", "description": "An entity id such as light.kitchen."});
    [
        (
            "ha_list_entities",
            "List Home Assistant entities with their state, optionally of one domain or area.",
            schema(
                &json!({"domain": {"type": "string"}, "area": {"type": "string", "description": "An area id or name."}}),
                &[],
            ),
            Kind::ListEntities,
            false,
        ),
        (
            "ha_get_state",
            "Read one Home Assistant entity's state and attributes.",
            schema(&json!({"entity_id": entity}), &["entity_id"]),
            Kind::GetState,
            false,
        ),
        (
            "ha_list_services",
            "List the services Home Assistant offers, optionally of one domain.",
            schema(&json!({"domain": {"type": "string"}}), &[]),
            Kind::ListServices,
            false,
        ),
        (
            "ha_call_service",
            "Call a Home Assistant service (turn a light on, set a thermostat). Services that run code or manage the host are blocked.",
            schema(
                &json!({
                    "domain": {"type": "string"},
                    "service": {"type": "string"},
                    "entity_id": entity,
                    "data": {"type": "object", "description": "Service data."},
                }),
                &["domain", "service"],
            ),
            Kind::CallService,
            true,
        ),
    ]
}

impl Contributor for Tools {
    fn platform(&self) -> &str {
        PLATFORM
    }

    fn toolsets(&self) -> Vec<(String, String)> {
        vec![(
            PLATFORM.to_owned(),
            "Read and control Home Assistant entities and services".to_owned(),
        )]
    }

    fn tools(&self, context: &Context<'_>) -> Vec<RegisteredTool> {
        let client = match pmt_net::http::client(context.network) {
            Ok(client) => client,
            Err(e) => {
                tracing::warn!("Home Assistant tools unavailable: {}", e.message());
                return Vec::new();
            }
        };
        let url = context
            .config
            .options
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_URL)
            .trim_end_matches('/')
            .to_owned();
        catalog()
            .into_iter()
            .map(|(name, description, parameters, kind, external)| {
                let mut descriptor = ToolDescriptor::new(name, PLATFORM, description, parameters);
                if !external {
                    descriptor.effect = pmt_tools::descriptor::Effect::None;
                    descriptor.idempotent = true;
                }
                let handler = Handler {
                    kind,
                    url: url.clone(),
                    secrets: Arc::clone(&context.secrets),
                    client: client.clone(),
                };
                RegisteredTool::new(
                    descriptor,
                    Owner::Capability(format!("platform:{PLATFORM}")),
                    Arc::new(handler),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_ids_and_blocked_domains() {
        assert!(is_entity_id("light.kitchen_1"));
        assert!(
            !is_entity_id("Light.kitchen") && !is_entity_id("light.") && !is_entity_id("light/x.y")
        );
        let err = service_call(&json!({"domain": "hassio", "service": "host_reboot"})).unwrap_err();
        assert!(err.message.contains("hassio"), "{err:?}");
        let (path, body) =
            service_call(&json!({"domain": "light", "service": "turn_on", "entity_id": "light.k", "data": {"brightness": 9}}))
                .unwrap();
        assert_eq!(path, "/api/services/light/turn_on");
        assert_eq!(body, json!({"brightness": 9, "entity_id": "light.k"}));
    }
}
