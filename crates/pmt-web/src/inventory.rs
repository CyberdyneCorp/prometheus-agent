//! The REST endpoint inventory (spec: web-dashboard, REST endpoint
//! inventory): every JSON endpoint the dashboard serves, the one table
//! both the router and the checked-in `OpenAPI` document
//! `schema/dashboard-api.json` (written by `cargo xtask gen`) come from.
//! A request whose method and path match no entry answers 404 before any
//! handler runs, so an endpoint missing from the schema cannot be reached.

use hyper::Method;
use serde_json::{Map, Value, json};

/// Who may call an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Anyone (it reads no profile data).
    Public,
    /// A signed-in browser, deployment token or paired device.
    Signed,
    /// A pairing code exchange or a device's own token (paired devices).
    Device,
}

/// One endpoint.
#[derive(Debug, Clone, Copy)]
pub struct Endpoint {
    /// Its HTTP method.
    pub method: &'static str,
    /// Its path below the base path; `{name}` segments are parameters.
    pub path: &'static str,
    /// What it does.
    pub summary: &'static str,
    /// Who may call it.
    pub access: Access,
}

const fn signed(method: &'static str, path: &'static str, summary: &'static str) -> Endpoint {
    Endpoint {
        method,
        path,
        summary,
        access: Access::Signed,
    }
}

macro_rules! profile {
    ($method:literal, $rest:literal, $summary:literal) => {
        signed($method, concat!("/api/profiles/{profile}", $rest), $summary)
    };
}

/// Every endpoint, machine-wide first.
pub const ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        method: "GET",
        path: "/api/health",
        summary: "Liveness, build identity and the active sign-in providers",
        access: Access::Public,
    },
    Endpoint {
        method: "POST",
        path: "/api/devices/pair",
        summary: "Exchange a one-use pairing code for a device credential",
        access: Access::Device,
    },
    Endpoint {
        method: "POST",
        path: "/api/devices/refresh",
        summary: "Rotate the calling device's credential",
        access: Access::Device,
    },
    signed(
        "GET",
        "/api/session",
        "The caller: subject, role and CSRF token",
    ),
    signed("POST", "/api/logout", "Sign this browser out"),
    signed(
        "GET",
        "/api/status",
        "Daemon version, uptime, clients, sessions and adapters",
    ),
    signed("GET", "/api/profiles", "The served profiles"),
    signed(
        "GET",
        "/api/logs",
        "A redacted, bounded tail of a registry log",
    ),
    signed("GET", "/api/events", "The live event stream (WebSocket)"),
    signed("GET", "/api/rpc", "The client protocol (WebSocket)"),
    signed(
        "GET",
        "/api/system/platform",
        "The honest platform support matrix",
    ),
    signed("GET", "/api/docs", "The embedded documentation's pages"),
    signed(
        "GET",
        "/api/docs/page",
        "One embedded documentation page (path=)",
    ),
    signed(
        "GET",
        "/api/docs/search",
        "Search the embedded documentation offline (q=)",
    ),
    signed(
        "GET",
        "/api/themes",
        "Built-in and user themes, with rejection reasons",
    ),
    signed(
        "GET",
        "/api/themes/{name}/css",
        "A valid theme's style sheet",
    ),
    signed(
        "POST",
        "/api/profiles",
        "Create a profile (the profile builder)",
    ),
    profile!("GET", "/config", "The effective configuration"),
    profile!("PATCH", "/config", "One validated configuration patch"),
    profile!(
        "GET",
        "/config/schema",
        "The JSON Schema of the running build's typed config"
    ),
    profile!("GET", "/config/defaults", "The configuration defaults"),
    profile!("GET", "/secrets", "Secret names and sources (never values)"),
    profile!(
        "GET",
        "/secrets/{name}",
        "Whether a secret is set and where (never its value)"
    ),
    profile!("PUT", "/secrets/{name}", "Set a secret (write-only)"),
    profile!("DELETE", "/secrets/{name}", "Delete a secret"),
    profile!(
        "GET",
        "/sessions",
        "Sessions, paged, with full-text search (q)"
    ),
    profile!("GET", "/sessions/{id}/messages", "A session's transcript"),
    profile!("DELETE", "/sessions/{id}", "Delete a session"),
    profile!("GET", "/usage", "Cost and token usage"),
    profile!("GET", "/usage.csv", "Cost and token usage as CSV"),
    profile!("GET", "/models", "Configured provider/model pairs"),
    profile!("GET", "/cron", "Scheduled jobs"),
    profile!("POST", "/cron", "Create a scheduled job"),
    profile!("PATCH", "/cron/{id}", "Change a scheduled job"),
    profile!("DELETE", "/cron/{id}", "Delete a scheduled job"),
    profile!("POST", "/cron/{id}/run", "Run a scheduled job now"),
    profile!("GET", "/skills", "Skills with their enabled state"),
    profile!("PATCH", "/skills/{name}", "Enable or disable a skill"),
    profile!("GET", "/toolsets", "Toolsets with their enabled state"),
    profile!("PATCH", "/toolsets/{name}", "Enable or disable a toolset"),
    profile!("GET", "/memory", "Memory entries"),
    profile!("PATCH", "/memory", "Add, replace or remove memory entries"),
    profile!("GET", "/plugins", "Plugins with their load state"),
    profile!("GET", "/mcp", "Configured MCP servers"),
    profile!(
        "GET",
        "/adapters",
        "Messaging adapters: state, options schema and form"
    ),
    profile!(
        "PUT",
        "/adapters/{name}",
        "Save an adapter's form (one validated patch plus secrets)"
    ),
    profile!("GET", "/files", "The workspace roots"),
    profile!(
        "GET",
        "/files/list",
        "List a directory below a root (root=, path=)"
    ),
    profile!(
        "GET",
        "/files/read",
        "Preview or download a file (root=, path=, download=)"
    ),
    profile!(
        "POST",
        "/files/upload",
        "Upload a file (root=, path=, name=)"
    ),
    profile!("POST", "/files/rename", "Rename a file (root=)"),
    profile!(
        "DELETE",
        "/files",
        "Delete a file (root=, path=, confirm=true)"
    ),
    profile!(
        "POST",
        "/adapters/{name}/pause",
        "Pause a messaging adapter"
    ),
    profile!(
        "POST",
        "/adapters/{name}/resume",
        "Resume a messaging adapter"
    ),
    profile!("GET", "/pairing", "Pending and approved messaging pairings"),
    profile!("POST", "/pairing/{code}", "Approve a pending pairing code"),
    profile!(
        "GET",
        "/webhooks",
        "Webhook subscriptions (never their secrets)"
    ),
    profile!(
        "POST",
        "/webhooks",
        "Create a webhook subscription (secret shown once)"
    ),
    profile!(
        "PATCH",
        "/webhooks/{name}",
        "Enable or disable a webhook subscription"
    ),
    profile!(
        "DELETE",
        "/webhooks/{name}",
        "Delete a webhook subscription"
    ),
    profile!("POST", "/webhooks/{name}/test", "Send a signed test event"),
    profile!(
        "POST",
        "/maintenance/{action}",
        "Bulk maintenance (needs \"confirm\": true)"
    ),
    profile!("POST", "/uploads", "Upload a chat attachment"),
    profile!("POST", "/stage", "Stage a webapp file (16 MiB cap)"),
    profile!("GET", "/blobs/{sha}", "Download a stored attachment"),
];

/// Whether `path` matches `pattern` segment by segment.
fn matches(pattern: &str, path: &str) -> bool {
    let mut want = pattern.trim_matches('/').split('/');
    let mut got = path.trim_matches('/').split('/');
    loop {
        match (want.next(), got.next()) {
            (None, None) => return true,
            (Some(w), Some(g)) if w.starts_with('{') && !g.is_empty() => {}
            (Some(w), Some(g)) if w == g => {}
            _ => return false,
        }
    }
}

/// The endpoint `method` `path` names (`HEAD` matches `GET`).
pub(crate) fn find(method: &Method, path: &str) -> Option<&'static Endpoint> {
    let method = if method == Method::HEAD {
        "GET"
    } else {
        method.as_str()
    };
    ENDPOINTS
        .iter()
        .find(|e| e.method == method && matches(e.path, path))
}

fn parameters(path: &str) -> Vec<Value> {
    path.split('/')
        .filter_map(|s| s.strip_prefix('{').and_then(|s| s.strip_suffix('}')))
        .map(|name| json!({"name": name, "in": "path", "required": true, "schema": {"type": "string"}}))
        .collect()
}

fn operation(endpoint: &Endpoint) -> Value {
    let mut op = json!({
        "summary": endpoint.summary,
        "parameters": parameters(endpoint.path),
        "responses": {
            "200": {"description": "OK", "content": {"application/json": {}}},
            "404": {"$ref": "#/components/responses/NotFound"},
        },
    });
    match endpoint.access {
        Access::Public => op["security"] = json!([]),
        Access::Device => op["security"] = json!([{"device": []}, {}]),
        Access::Signed => {}
    }
    if endpoint.path.ends_with("/config/schema") {
        op["responses"]["200"]["content"]["application/json"] =
            json!({"schema": {"$ref": "config.schema.json"}});
    }
    if endpoint.path.contains("/maintenance/") {
        op["requestBody"] = json!({"required": true, "content": {"application/json": {"schema": {
            "type": "object", "required": ["confirm"],
            "properties": {"confirm": {"const": true}, "days": {"type": "integer", "minimum": 1}}}}}});
        op["responses"]["403"] = json!({"description": "observers may not run maintenance"});
        op["responses"]["409"] = json!({"description": "\"confirm\": true is missing"});
    }
    op
}

/// The `OpenAPI` 3.1 document of [`ENDPOINTS`] for build `version`.
#[must_use]
pub fn openapi(version: &str) -> Value {
    let mut paths = Map::new();
    for endpoint in ENDPOINTS {
        let item = paths
            .entry(endpoint.path)
            .or_insert_with(|| Value::Object(Map::new()));
        item[endpoint.method.to_ascii_lowercase()] = operation(endpoint);
    }
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Prometheus dashboard API",
            "version": version,
            "description": "Generated by `cargo xtask gen` from pmt_web::inventory::ENDPOINTS; \
                every endpoint not listed here answers 404. Machine-wide endpoints live under \
                /api/, profile data under /api/profiles/{profile}/.",
        },
        "components": {
            "securitySchemes": {
                "session": {"type": "apiKey", "in": "cookie", "name": "prometheus_session",
                    "description": "Browser sessions; state-changing calls also send X-Prometheus-CSRF"},
                "bearer": {"type": "http", "scheme": "bearer",
                    "description": "dashboard.auth.mode = \"token\": the DASHBOARD_SESSION_TOKEN secret"},
                "device": {"type": "http", "scheme": "bearer",
                    "description": "A paired device's credential (prometheus dashboard pair)"},
            },
            "responses": {"NotFound": {"description": "No such endpoint or profile"}},
        },
        "security": [{"session": []}, {"bearer": []}, {"device": []}],
        "paths": paths,
    })
}

#[cfg(test)]
mod tests {
    use hyper::Method;

    use super::{ENDPOINTS, find, openapi};

    #[test]
    fn only_listed_endpoints_are_found() {
        let m = |path: &str| find(&Method::POST, path).map(|e| e.path);
        assert_eq!(
            m("/api/profiles/work/maintenance/prune-sessions"),
            Some("/api/profiles/{profile}/maintenance/{action}")
        );
        assert_eq!(m("/api/profiles/work/maintenance"), None);
        assert!(find(&Method::GET, "/api/profiles/work/usage.csv").is_some());
        assert!(find(&Method::HEAD, "/api/status").is_some());
        assert!(find(&Method::GET, "/api/profiles//config").is_none());
        assert!(find(&Method::GET, "/api/nothing").is_none());
    }

    #[test]
    fn every_endpoint_is_in_the_openapi_document_once() {
        let doc = openapi("0.0.0");
        for endpoint in ENDPOINTS {
            let op = &doc["paths"][endpoint.path][endpoint.method.to_ascii_lowercase()];
            assert_eq!(
                op["summary"], endpoint.summary,
                "{} {}",
                endpoint.method, endpoint.path
            );
        }
        let mut seen: Vec<_> = ENDPOINTS.iter().map(|e| (e.method, e.path)).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), ENDPOINTS.len(), "no endpoint is listed twice");
    }
}
