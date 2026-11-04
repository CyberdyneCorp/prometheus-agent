//! `manage_connections` arguments: the action, its targets and their
//! checks (target kind per action, alias pattern, one aliased target).

use serde_json::Value;

use crate::connectors::names::is_alias;

/// What the call asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    Status,
    Display,
    Connect,
    Reconnect,
    Rename,
    Install,
    Enable,
    Authorize,
}

impl Action {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "status" => Self::Status,
            "display" => Self::Display,
            "connect" => Self::Connect,
            "reconnect" => Self::Reconnect,
            "rename" => Self::Rename,
            "install" => Self::Install,
            "enable" => Self::Enable,
            "authorize" => Self::Authorize,
            _ => return None,
        })
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Display => "display",
            Self::Connect => "connect",
            Self::Reconnect => "reconnect",
            Self::Rename => "rename",
            Self::Install => "install",
            Self::Enable => "enable",
            Self::Authorize => "authorize",
        }
    }

    /// The actions on catalog MCP targets (approval required).
    pub(super) fn is_mcp(self) -> bool {
        matches!(self, Self::Install | Self::Enable | Self::Authorize)
    }

    fn is_hosted(self) -> bool {
        matches!(self, Self::Connect | Self::Reconnect | Self::Rename)
    }
}

/// One target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Target {
    pub(super) name: String,
    pub(super) mcp: bool,
    pub(super) alias: Option<String>,
    pub(super) to: Option<String>,
}

/// A checked call.
#[derive(Debug, Clone)]
pub(super) struct Request {
    pub(super) action: Action,
    pub(super) targets: Vec<Target>,
    pub(super) force: bool,
}

/// The words of requests this tool never fulfils.
const REMOVALS: [&str; 6] = [
    "disconnect",
    "delete",
    "remove",
    "revoke",
    "unlink",
    "logout",
];

/// The reply to a removal request.
pub(super) const NO_REMOVAL: &str = "manage_connections cannot disconnect, delete or revoke accounts. Tell the user to run `prometheus connectors disconnect <app> [--alias <name>]` or to remove the account in the Nous portal.";

/// Parses and checks `arguments`.
pub(super) fn parse(arguments: &Value) -> Result<Request, String> {
    let action_name = arguments
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("status");
    let Some(action) = Action::parse(action_name) else {
        if REMOVALS.contains(&action_name) {
            return Err(NO_REMOVAL.to_owned());
        }
        return Err(format!(
            "unknown action '{action_name}'; use status, display, connect, reconnect, rename, install, enable or authorize"
        ));
    };
    let targets = targets(arguments.get("connectors"))?;
    if targets.is_empty() && !matches!(action, Action::Status | Action::Display) {
        return Err(format!(
            "action '{action_name}' needs connectors, e.g. {{\"action\": \"{action_name}\", \"connectors\": [\"gmail\"]}}"
        ));
    }
    let force = arguments
        .get("force")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if force && action != Action::Reconnect {
        return Err("force applies only to action 'reconnect'".to_owned());
    }
    check_kinds(action, &targets)?;
    check_aliases(action, &targets)?;
    Ok(Request {
        action,
        targets,
        force,
    })
}

fn targets(value: Option<&Value>) -> Result<Vec<Target>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let Value::Array(items) = value else {
        return Err(
            "connectors must be an array of names or {name, mcp?, alias?, to?} objects".to_owned(),
        );
    };
    items.iter().map(target).collect()
}

fn target(item: &Value) -> Result<Target, String> {
    if let Some(name) = item.as_str() {
        return Ok(Target {
            name: name.to_owned(),
            mcp: false,
            alias: None,
            to: None,
        });
    }
    let text = |key: &str| item.get(key).and_then(Value::as_str).map(str::to_owned);
    let name = text("name").filter(|n| !n.is_empty()).ok_or_else(|| {
        "every connector needs a name: \"gmail\" or {\"name\": \"gmail\"}".to_owned()
    })?;
    Ok(Target {
        name,
        mcp: item.get("mcp").and_then(Value::as_bool).unwrap_or(false),
        alias: text("alias"),
        to: text("to"),
    })
}

/// Hosted actions take hosted targets, MCP actions MCP ones; the error
/// names the call that would work.
fn check_kinds(action: Action, targets: &[Target]) -> Result<(), String> {
    let verb = action.as_str();
    for target in targets {
        let name = &target.name;
        if action.is_hosted() && target.mcp {
            return Err(format!(
                "'{verb}' works on hosted connectors; '{name}' is an MCP server: use action \"install\", \"enable\" or \"authorize\" with {{\"name\": \"{name}\", \"mcp\": true}}"
            ));
        }
        if action.is_mcp() && !target.mcp {
            return Err(format!(
                "'{verb}' needs an MCP target such as {{\"name\": \"{name}\", \"mcp\": true}}; if '{name}' is a hosted connector, use action \"connect\" with connectors [\"{name}\"]"
            ));
        }
    }
    Ok(())
}

fn check_aliases(action: Action, targets: &[Target]) -> Result<(), String> {
    for target in targets {
        for alias in [&target.alias, &target.to].into_iter().flatten() {
            let current_label = action == Action::Rename && Some(alias) == target.alias.as_ref();
            if !is_alias(alias) && !current_label {
                return Err(format!(
                    "invalid account name '{alias}': use lower-case letters, digits and dashes (^[a-z0-9][a-z0-9-]{{0,31}}$)"
                ));
            }
        }
        if target.to.is_some() && action != Action::Rename {
            return Err("'to' applies only to action 'rename'".to_owned());
        }
    }
    if action == Action::Rename && targets.iter().any(|t| t.to.is_none()) {
        return Err("rename needs the new name: {\"name\": \"gmail\", \"alias\": \"<current name or label>\", \"to\": \"<new>\"}".to_owned());
    }
    let aliased = targets
        .iter()
        .filter(|t| t.alias.is_some() || t.to.is_some())
        .count();
    if aliased > 1 {
        return Err("at most one target per call may name an account (alias or to); make one call per account".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Action, NO_REMOVAL, parse};

    #[test]
    fn install_of_a_hosted_connector_names_the_call_that_works() {
        let err = parse(&json!({"action": "install", "connectors": ["gmail"]})).unwrap_err();
        assert!(err.contains(r#"{"name": "gmail", "mcp": true}"#), "{err}");
        assert!(err.contains("\"connect\""), "{err}");
        let err =
            parse(&json!({"action": "connect", "connectors": [{"name": "github", "mcp": true}]}))
                .unwrap_err();
        assert!(err.contains("\"install\""), "{err}");
    }

    #[test]
    fn removal_requests_point_to_the_cli() {
        assert_eq!(
            parse(&json!({"action": "disconnect", "connectors": ["gmail"]})).unwrap_err(),
            NO_REMOVAL
        );
    }

    #[test]
    fn aliases_are_checked_and_one_aliased_target_is_allowed() {
        assert!(
            parse(
                &json!({"action": "connect", "connectors": [{"name": "gmail", "alias": "Work"}]})
            )
            .is_err()
        );
        assert!(
            parse(&json!({"action": "connect", "connectors": [
                {"name": "gmail", "alias": "a"}, {"name": "notion", "alias": "b"}
            ]}))
            .is_err()
        );
        let ok = parse(&json!({"action": "rename", "connectors": [{"name": "gmail", "alias": "me@x.org", "to": "home"}]})).unwrap();
        assert_eq!(ok.action, Action::Rename);
        assert!(
            parse(&json!({"action": "rename", "connectors": [{"name": "gmail", "to": "-x"}]}))
                .is_err()
        );
        assert_eq!(parse(&json!({})).unwrap().action, Action::Status);
        assert!(parse(&json!({"action": "connect"})).is_err());
    }
}
