//! The Discord adapter's tools (spec: platform-adapters, Discord tools;
//! Platform-contributed toolsets): `discord` (read a channel, find
//! members, open a thread) and `discord_admin` (server administration),
//! each one tool with an `action` argument. `options.server_actions`
//! filters both schemas and is checked again on every call; without the
//! Server Members intent (detected at connect) the member actions are
//! hidden. Every call reads the bot token when it runs and goes to the
//! REST API like the adapter's own sends.

use std::sync::Arc;

use pmt_config::platforms::{DiscordAction, DiscordOptions};
use pmt_tools::descriptor::{CallContext, HandlerFuture, Owner, ToolHandler};
use pmt_tools::platform_tools::{self, Action, Context, Contributor, SecretReader};
use pmt_tools::{RegisteredTool, ToolDescriptor, ToolFailure, ToolResult};
use reqwest::Method;
use serde_json::{Map, Value, json};

use crate::api::{Api, Body, Endpoints};
use crate::{PLATFORM, TOKEN_SECRET};

/// The permission the member actions need: the Server Members intent.
pub const SERVER_MEMBERS: &str = "server_members";

/// The `discord` tool's actions.
const CHANNEL_ACTIONS: &[Action] = &[
    Action {
        name: "fetch_messages",
        permission: None,
    },
    Action {
        name: "search_members",
        permission: Some(SERVER_MEMBERS),
    },
    Action {
        name: "create_thread",
        permission: None,
    },
];

/// The `discord_admin` tool's actions.
const ADMIN_ACTIONS: &[Action] = &[
    Action {
        name: "list_guilds",
        permission: None,
    },
    Action {
        name: "server_info",
        permission: None,
    },
    Action {
        name: "list_channels",
        permission: None,
    },
    Action {
        name: "channel_info",
        permission: None,
    },
    Action {
        name: "list_roles",
        permission: None,
    },
    Action {
        name: "member_info",
        permission: Some(SERVER_MEMBERS),
    },
    Action {
        name: "list_pins",
        permission: None,
    },
    Action {
        name: "pin_message",
        permission: None,
    },
    Action {
        name: "unpin_message",
        permission: None,
    },
    Action {
        name: "delete_message",
        permission: None,
    },
    Action {
        name: "add_role",
        permission: None,
    },
    Action {
        name: "remove_role",
        permission: None,
    },
];

/// One action's REST call: `{arg}` placeholders in `path` take the
/// argument of that name (a Discord id); `query` arguments are appended.
struct Route {
    method: Method,
    path: &'static str,
    query: &'static [&'static str],
}

fn route(action: &str) -> Option<Route> {
    let (method, path, query): (Method, &'static str, &'static [&'static str]) = match action {
        "fetch_messages" => (Method::GET, "/channels/{channel_id}/messages", &["limit"]),
        "search_members" => (
            Method::GET,
            "/guilds/{guild_id}/members/search",
            &["query", "limit"],
        ),
        "create_thread" => (Method::POST, "/channels/{channel_id}/threads", &[]),
        "list_guilds" => (Method::GET, "/users/@me/guilds", &[]),
        "server_info" => (Method::GET, "/guilds/{guild_id}", &["with_counts"]),
        "list_channels" => (Method::GET, "/guilds/{guild_id}/channels", &[]),
        "channel_info" => (Method::GET, "/channels/{channel_id}", &[]),
        "list_roles" => (Method::GET, "/guilds/{guild_id}/roles", &[]),
        "member_info" => (Method::GET, "/guilds/{guild_id}/members/{user_id}", &[]),
        "list_pins" => (Method::GET, "/channels/{channel_id}/pins", &[]),
        "pin_message" => (Method::PUT, "/channels/{channel_id}/pins/{message_id}", &[]),
        "unpin_message" => (
            Method::DELETE,
            "/channels/{channel_id}/pins/{message_id}",
            &[],
        ),
        "delete_message" => (
            Method::DELETE,
            "/channels/{channel_id}/messages/{message_id}",
            &[],
        ),
        "add_role" => (
            Method::PUT,
            "/guilds/{guild_id}/members/{user_id}/roles/{role_id}",
            &[],
        ),
        "remove_role" => (
            Method::DELETE,
            "/guilds/{guild_id}/members/{user_id}/roles/{role_id}",
            &[],
        ),
        _ => return None,
    };
    Some(Route {
        method,
        path,
        query,
    })
}

/// The arguments with their defaults filled in (`limit` 50, at most 100;
/// a thread archives after 1440 minutes).
fn with_defaults(action: &str, arguments: &Value) -> Map<String, Value> {
    let mut args = arguments.as_object().cloned().unwrap_or_default();
    match action {
        "fetch_messages" | "search_members" => {
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(50);
            args.insert("limit".into(), json!(limit.clamp(1, 100)));
        }
        "create_thread" => {
            args.entry("auto_archive_duration").or_insert(json!(1440));
        }
        "server_info" => {
            args.insert("with_counts".into(), json!(true));
        }
        _ => {}
    }
    args
}

fn invalid(message: impl Into<String>) -> ToolFailure {
    ToolFailure::new("invalid_input", message)
}

/// A Discord id argument: digits only, so it cannot leave its path
/// segment.
fn id_argument(args: &Map<String, Value>, name: &str) -> Result<String, ToolFailure> {
    let id = match args.get(name) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => return Err(invalid(format!("{name} is required"))),
    };
    if id.is_empty() || id.len() > 20 || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid(format!("{name} must be a Discord id (digits)")));
    }
    Ok(id)
}

/// The request path of `route` for `args`.
fn path_of(route: &Route, args: &Map<String, Value>) -> Result<String, ToolFailure> {
    let mut path = String::new();
    for segment in route.path.split('/').filter(|s| !s.is_empty()) {
        path.push('/');
        match segment.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
            Some(name) => path.push_str(&id_argument(args, name)?),
            None => path.push_str(segment),
        }
    }
    let mut query = Vec::new();
    for name in route.query {
        let value = match args.get(*name) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => continue,
            Some(other) => other.to_string(),
        };
        let value =
            percent_encoding::utf8_percent_encode(&value, percent_encoding::NON_ALPHANUMERIC);
        query.push(format!("{name}={value}"));
    }
    if route.query.contains(&"query") && !args.contains_key("query") {
        return Err(invalid("query is required"));
    }
    if !query.is_empty() {
        path.push('?');
        path.push_str(&query.join("&"));
    }
    Ok(path)
}

/// The JSON body of a call that sends one.
fn body_of(action: &str, args: &Map<String, Value>) -> Result<Body, ToolFailure> {
    if action != "create_thread" {
        return Ok(Body::Empty);
    }
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.trim().is_empty())
        .ok_or_else(|| invalid("name is required"))?;
    Ok(Body::Json(json!({
        "name": name,
        "auto_archive_duration": args["auto_archive_duration"],
        // A public thread.
        "type": 11,
    })))
}

/// One of the two tools, for one profile.
struct Handler {
    tool: &'static str,
    actions: &'static [Action],
    allowlist: Option<Vec<String>>,
    secrets: SecretReader,
    secret: String,
    client: reqwest::Client,
    endpoints: Endpoints,
}

impl Handler {
    async fn run(&self, arguments: Value) -> Result<ToolResult, ToolFailure> {
        let action = arguments["action"]
            .as_str()
            .ok_or_else(|| invalid("action is required"))?;
        if !self.actions.iter().any(|a| a.name == action) {
            return Err(invalid(format!("{} has no action {action}", self.tool)));
        }
        platform_tools::check_action(action, self.allowlist.as_deref())
            .map_err(|why| ToolFailure::new("disabled", why))?;
        let route = route(action).ok_or_else(|| invalid(format!("unknown action {action}")))?;
        let args = with_defaults(action, &arguments);
        let path = path_of(&route, &args)?;
        let body = body_of(action, &args)?;
        let token = (self.secrets)(&self.secret).ok_or_else(|| {
            ToolFailure::new(
                "missing_secret",
                format!("secret {} is not set", self.secret),
            )
        })?;
        let api = Api {
            client: self.client.clone(),
            endpoints: self.endpoints.clone(),
            token,
        };
        let answer = api
            .call(route.method, &path, body)
            .await
            .map_err(|e| ToolFailure::new("discord", e.describe()))?;
        Ok(ToolResult::ok(match answer {
            Value::Null => format!("{action}: done"),
            other => other.to_string(),
        }))
    }
}

impl ToolHandler for Handler {
    fn call<'a>(&'a self, _: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(self.run(arguments))
    }
}

/// The arguments every action may take.
fn properties() -> Value {
    let id = |what: &str| json!({"type": "string", "description": format!("The {what} id.")});
    json!({
        "guild_id": id("server"),
        "channel_id": id("channel"),
        "message_id": id("message"),
        "user_id": id("member"),
        "role_id": id("role"),
        "query": {"type": "string", "description": "search_members: the start of a name."},
        "name": {"type": "string", "description": "create_thread: the thread name."},
        "limit": {"type": "integer", "minimum": 1, "maximum": 100, "description": "How many (default 50)."},
        "auto_archive_duration": {
            "type": "integer",
            "enum": [60, 1440, 4320, 10080],
            "description": "create_thread: minutes of inactivity before it archives (default 1440)."
        },
    })
}

/// The Discord tools for every profile that enables the platform.
#[derive(Debug, Clone)]
pub struct DiscordTools {
    endpoints: Endpoints,
}

impl DiscordTools {
    /// The tools, calling `endpoints`.
    #[must_use]
    pub fn new(endpoints: Endpoints) -> Self {
        Self { endpoints }
    }

    fn tool(
        &self,
        context: &Context<'_>,
        client: &reqwest::Client,
        (name, actions, summary): (&'static str, &'static [Action], &str),
        allowlist: Option<&Vec<String>>,
    ) -> Option<RegisteredTool> {
        let shown: Vec<&str> = platform_tools::visible(
            actions,
            allowlist.map(Vec::as_slice),
            context.granted.as_ref(),
        )
        .iter()
        .map(|a| a.name)
        .collect();
        if shown.is_empty() {
            return None;
        }
        // The actions are named in the description, not an `enum`, so a
        // call naming a disabled one reaches the call-time check and its
        // message instead of a schema error.
        let mut parameters = properties();
        parameters["action"] = json!({
            "type": "string",
            "description": format!("One of: {}.", shown.join(", ")),
        });
        let schema = json!({
            "type": "object",
            "properties": parameters,
            "required": ["action"],
        });
        let description = format!("{summary} Actions: {}.", shown.join(", "));
        let descriptor = ToolDescriptor::new(name, name, description, schema);
        let secret = context
            .config
            .token_secret
            .as_ref()
            .map_or_else(|| TOKEN_SECRET.to_owned(), |s| s.0.clone());
        let handler = Handler {
            tool: name,
            actions,
            allowlist: allowlist.cloned(),
            secrets: Arc::clone(&context.secrets),
            secret,
            client: client.clone(),
            endpoints: self.endpoints.clone(),
        };
        Some(RegisteredTool::new(
            descriptor,
            Owner::Capability(format!("platform:{PLATFORM}")),
            Arc::new(handler),
        ))
    }
}

impl Default for DiscordTools {
    fn default() -> Self {
        Self::new(Endpoints::default())
    }
}

impl Contributor for DiscordTools {
    fn platform(&self) -> &str {
        PLATFORM
    }

    fn toolsets(&self) -> Vec<(String, String)> {
        vec![
            (
                "discord".to_owned(),
                "Read Discord channels, find members and open threads".to_owned(),
            ),
            (
                "discord_admin".to_owned(),
                "Administer Discord servers: channels, roles, pins and messages".to_owned(),
            ),
        ]
    }

    fn tools(&self, context: &Context<'_>) -> Vec<RegisteredTool> {
        let options: DiscordOptions =
            pmt_config::platforms::options(PLATFORM, &context.config.options).unwrap_or_default();
        let client = match pmt_net::http::client(context.network) {
            Ok(client) => client,
            Err(e) => {
                tracing::warn!("Discord tools unavailable: {}", e.message());
                return Vec::new();
            }
        };
        let allowlist: Option<Vec<String>> = options.server_actions.map(|list| {
            list.into_iter()
                .map(|a| DiscordAction::name(a).to_owned())
                .collect()
        });
        [
            (
                "discord",
                CHANNEL_ACTIONS,
                "Read and organize Discord channels.",
            ),
            (
                "discord_admin",
                ADMIN_ACTIONS,
                "Administer the Discord servers the bot is in.",
            ),
        ]
        .into_iter()
        .filter_map(|tool| self.tool(context, &client, tool, allowlist.as_ref()))
        .collect()
    }
}

/// The permissions the bot holds, from its application's flags: the
/// Server Members intent (`GATEWAY_GUILD_MEMBERS`, or its limited form).
pub(crate) fn permissions_of(application: &Value) -> std::collections::BTreeSet<String> {
    const MEMBERS: u64 = 1 << 14;
    const MEMBERS_LIMITED: u64 = 1 << 15;
    let flags = application["flags"].as_u64().unwrap_or(0);
    let mut granted = std::collections::BTreeSet::new();
    if flags & (MEMBERS | MEMBERS_LIMITED) != 0 {
        granted.insert(SERVER_MEMBERS.to_owned());
    }
    granted
}

/// Detects the bot's permissions and records them for the tools (a failed
/// lookup records "unknown", which shows every action).
pub(crate) async fn detect_permissions(api: &Api, profile_dir: &std::path::Path) {
    let granted = api
        .call(Method::GET, "/applications/@me", Body::Empty)
        .await
        .ok()
        .map(|application| permissions_of(&application));
    if let Err(e) = platform_tools::store_permissions(profile_dir, PLATFORM, granted.as_ref()) {
        tracing::warn!("cannot record the Discord bot's permissions: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_has_a_route_and_a_config_name() {
        for action in CHANNEL_ACTIONS.iter().chain(ADMIN_ACTIONS) {
            assert!(route(action.name).is_some(), "{}", action.name);
            let parsed: DiscordAction = serde_json::from_value(json!(action.name)).unwrap();
            assert_eq!(parsed.name(), action.name);
        }
    }

    #[test]
    fn ids_cannot_leave_their_path_segment() {
        let route = route("delete_message").unwrap();
        let args = with_defaults(
            "delete_message",
            &json!({"channel_id": "1/../../users", "message_id": "2"}),
        );
        let err = path_of(&route, &args).unwrap_err();
        assert!(err.message.contains("channel_id"), "{err:?}");
    }

    #[test]
    fn the_members_intent_is_read_from_the_application_flags() {
        assert!(permissions_of(&json!({"flags": 1 << 15})).contains(SERVER_MEMBERS));
        assert!(permissions_of(&json!({"flags": 1 << 19})).is_empty());
    }
}
