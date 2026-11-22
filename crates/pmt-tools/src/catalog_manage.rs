//! The `catalog_manage` tool (spec: plugin-system, Catalog tool for
//! agent-initiated installs).
//!
//! The model searches the plugin and skill catalogs and asks for installs;
//! the user decides on one card with a row per item, and the host decides
//! everything else (source, pinned ref, threat scan, grants, profile).
//! The model names only `{kind, id}` per item: any other field (a version,
//! a ref, a source) is refused before a card is shown. Only surfaces that
//! can draw the card (desktop, TUI) run it; it lives in the surface-gated
//! `catalog` toolset, and elsewhere it answers how to install by hand.

use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::descriptor::{Effect, RegisteredTool, ResourceClaims, ToolDescriptor};
use crate::result::ToolResult;
use crate::search::RemoteFuture;

/// The tool's name.
pub const NAME: &str = "catalog_manage";
/// Most search results one call returns.
pub const MAX_RESULTS: usize = 10;
/// The answer on a surface without an approval card.
pub const NEEDS_CARD: &str = "catalog_manage needs the desktop app or TUI; install with prometheus plugins install <id> or prometheus skills install <id>";

const PARAMETERS: &[&str] = &["action", "kind", "query", "items", "reason"];
const ITEM_FIELDS: &[&str] = &["kind", "id"];
/// Longest `reason`.
const MAX_REASON: usize = 200;

/// What a catalog item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A plugin (`prometheus plugins install <id>`).
    Plugin,
    /// A skill (`prometheus skills install <id>`).
    Skill,
}

impl Kind {
    fn parse(text: &str) -> Result<Self, String> {
        match text {
            "plugin" => Ok(Self::Plugin),
            "skill" => Ok(Self::Skill),
            other => Err(format!("kind must be plugin or skill, not '{other}'")),
        }
    }

    /// The spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plugin => "plugin",
            Self::Skill => "skill",
        }
    }
}

/// One item the model asks to install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Plugin or skill.
    pub kind: Kind,
    /// Its catalog id, as `search` returned it.
    pub id: String,
}

/// One search hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// Plugin or skill.
    pub kind: Kind,
    /// Its catalog id.
    pub id: String,
    /// What people call it.
    pub display_name: String,
    /// `official`, `verified`, `community`...
    pub tier: String,
    /// Whether the profile already has it.
    pub installed: bool,
}

/// A parsed call.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Request {
    Search { kind: Option<Kind>, query: String },
    Install { items: Vec<Item>, reason: String },
}

/// The host's side: catalog lookups and installs, every decision its own.
pub trait CatalogHost: Send + Sync {
    /// Items matching `query`, plugins and skills unless `kind` narrows.
    fn search<'a>(&'a self, kind: Option<Kind>, query: &'a str) -> RemoteFuture<'a, Vec<Hit>>;
    /// Installs `item` its own way; the answer describes the result.
    fn install<'a>(&'a self, item: &'a Item) -> RemoteFuture<'a, Result<String, String>>;
}

/// The approval card: one row per item, each answered install (`true`)
/// or skip (`false`), in order.
pub trait InstallCard: Send + Sync {
    /// Shows the rows and waits for the user.
    fn show<'a>(&'a self, items: &'a [Item], reason: &'a str) -> RemoteFuture<'a, Vec<bool>>;
}

/// A session's `catalog_manage`.
pub struct Manager {
    host: Arc<dyn CatalogHost>,
    cards: bool,
}

impl std::fmt::Debug for Manager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Manager")
            .field("cards", &self.cards)
            .finish_non_exhaustive()
    }
}

impl Manager {
    /// The tool over `host`; `cards`: the session's surface draws the
    /// approval card (desktop, TUI).
    #[must_use]
    pub fn new(host: Arc<dyn CatalogHost>, cards: bool) -> Self {
        Self { host, cards }
    }

    /// Runs one call; `card` is the session's approval card.
    pub async fn call(&self, arguments: &Value, card: Option<&dyn InstallCard>) -> ToolResult {
        let request = match parse(arguments) {
            Ok(request) => request,
            Err(message) => return ToolResult::error(message),
        };
        let Some(card) = card.filter(|_| self.cards) else {
            return ToolResult::error(NEEDS_CARD);
        };
        match request {
            Request::Search { kind, query } => {
                ToolResult::ok(self.search(kind, &query).await.to_string())
            }
            Request::Install { items, reason } => {
                ToolResult::ok(self.install(card, &items, &reason).await.to_string())
            }
        }
    }

    async fn search(&self, kind: Option<Kind>, query: &str) -> Value {
        let hits: Vec<Value> = self
            .host
            .search(kind, query)
            .await
            .into_iter()
            .take(MAX_RESULTS)
            .map(|h| {
                json!({
                    "kind": h.kind.as_str(), "id": h.id, "display_name": h.display_name,
                    "tier": h.tier, "installed": h.installed,
                })
            })
            .collect();
        json!({"results": hits})
    }

    async fn install(&self, card: &dyn InstallCard, items: &[Item], reason: &str) -> Value {
        let answers = card.show(items, reason).await;
        let mut rows = Vec::with_capacity(items.len());
        for (at, item) in items.iter().enumerate() {
            let row = if answers.get(at).copied().unwrap_or(false) {
                match self.host.install(item).await {
                    Ok(detail) => json!({"status": "installed", "detail": detail}),
                    Err(why) => json!({"status": "failed", "error": why}),
                }
            } else {
                json!({"status": "skipped"})
            };
            let mut row = row;
            row["kind"] = json!(item.kind.as_str());
            row["id"] = json!(item.id);
            rows.push(row);
        }
        json!({"items": rows})
    }
}

fn unknown(object: &Map<String, Value>, allowed: &[&str]) -> Vec<String> {
    object
        .keys()
        .filter(|k| !allowed.contains(&k.as_str()))
        .cloned()
        .collect()
}

fn text<'a>(object: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, String> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

/// Validates a call; unknown parameters and item fields are refused by name.
fn parse(arguments: &Value) -> Result<Request, String> {
    let object = arguments
        .as_object()
        .ok_or_else(|| "arguments must be an object".to_owned())?;
    let extra = unknown(object, PARAMETERS);
    if !extra.is_empty() {
        return Err(format!("unknown parameter(s) {}", extra.join(", ")));
    }
    match text(object, "action")? {
        Some("search") => Ok(Request::Search {
            kind: text(object, "kind")?.map(Kind::parse).transpose()?,
            query: text(object, "query")?.unwrap_or("").to_owned(),
        }),
        Some("install") => parse_install(object),
        Some(other) => Err(format!("action must be search or install, not '{other}'")),
        None => Err("action is required (search or install)".to_owned()),
    }
}

fn parse_install(object: &Map<String, Value>) -> Result<Request, String> {
    let raw = object
        .get("items")
        .and_then(Value::as_array)
        .filter(|items| !items.is_empty())
        .ok_or_else(|| "install needs a non-empty items list of {kind, id}".to_owned())?;
    let mut items = Vec::with_capacity(raw.len());
    for item in raw {
        let fields = item
            .as_object()
            .ok_or_else(|| "each item must be an object {kind, id}".to_owned())?;
        let extra = unknown(fields, ITEM_FIELDS);
        if !extra.is_empty() {
            return Err(format!("unknown item field(s) {}", extra.join(", ")));
        }
        let kind = Kind::parse(text(fields, "kind")?.unwrap_or(""))?;
        let id = text(fields, "id")?
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| "each item needs an id from search".to_owned())?;
        items.push(Item {
            kind,
            id: id.trim().to_owned(),
        });
    }
    let reason = text(object, "reason")?.unwrap_or("").trim().to_owned();
    if reason.contains('\n') || reason.chars().count() > MAX_REASON {
        return Err(format!(
            "reason must be one line of at most {MAX_REASON} characters"
        ));
    }
    if object.contains_key("kind") || object.contains_key("query") {
        return Err("kind and query go with search; install takes items and reason".to_owned());
    }
    Ok(Request::Install { items, reason })
}

/// The tool's descriptor.
#[must_use]
pub fn descriptor() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        NAME,
        crate::agent_level_desktop::CATALOG,
        "Search the plugin and skill catalogs, or ask the user to install items. search: \
         optional kind (plugin or skill) and query; at most 10 results with id, kind, display \
         name, tier and whether it is installed. install: items [{kind, id}] from search and a \
         one-line reason; the user approves each row on a card and Prometheus chooses the \
         source, version, checks and permissions.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["action"],
            "properties": {
                "action": {"type": "string", "enum": ["search", "install"]},
                "kind": {"type": "string", "enum": ["plugin", "skill"]},
                "query": {"type": "string"},
                "items": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["kind", "id"],
                        "properties": {
                            "kind": {"type": "string", "enum": ["plugin", "skill"]},
                            "id": {"type": "string"}
                        }
                    }
                },
                "reason": {"type": "string", "maxLength": MAX_REASON}
            }
        }),
    );
    d.effect = Effect::External;
    d.claims = Some(ResourceClaims::exclusive("catalog_installs"));
    d
}

/// The tool's registry entry (the turn loop runs it: the card is the
/// session's).
#[must_use]
pub fn registered() -> RegisteredTool {
    RegisteredTool::agent_loop(descriptor())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Host {
        installed: Mutex<Vec<String>>,
    }

    impl CatalogHost for Host {
        fn search<'a>(
            &'a self,
            _kind: Option<Kind>,
            _query: &'a str,
        ) -> RemoteFuture<'a, Vec<Hit>> {
            Box::pin(async move {
                (0..15)
                    .map(|n| Hit {
                        kind: Kind::Plugin,
                        id: format!("p{n}"),
                        display_name: format!("P {n}"),
                        tier: "community".to_owned(),
                        installed: false,
                    })
                    .collect()
            })
        }
        fn install<'a>(&'a self, item: &'a Item) -> RemoteFuture<'a, Result<String, String>> {
            Box::pin(async move {
                self.installed.lock().unwrap().push(item.id.clone());
                Ok("installed disabled".to_owned())
            })
        }
    }

    struct Card {
        shown: Mutex<usize>,
        answers: Vec<bool>,
    }

    impl InstallCard for Card {
        fn show<'a>(&'a self, _items: &'a [Item], _reason: &'a str) -> RemoteFuture<'a, Vec<bool>> {
            *self.shown.lock().unwrap() += 1;
            Box::pin(async move { self.answers.clone() })
        }
    }

    fn manager(cards: bool) -> (Manager, Arc<Host>) {
        let host = Arc::new(Host {
            installed: Mutex::new(Vec::new()),
        });
        (Manager::new(Arc::clone(&host) as _, cards), host)
    }

    fn card(answers: &[bool]) -> Card {
        Card {
            shown: Mutex::new(0),
            answers: answers.to_vec(),
        }
    }

    fn block<T>(f: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(f)
    }

    #[test]
    fn the_model_cannot_pick_a_version() {
        // Spec scenario "Model cannot pick a version".
        let (manager, host) = manager(true);
        let card = card(&[true]);
        let result = block(manager.call(
            &json!({"action": "install", "items": [{"kind": "plugin", "id": "weather", "ref": "abc"}]}),
            Some(&card),
        ));
        assert_eq!(result.status, crate::result::Status::Error);
        assert_eq!(result.text_content(), "unknown item field(s) ref");
        assert_eq!(*card.shown.lock().unwrap(), 0);
        assert!(host.installed.lock().unwrap().is_empty());
    }

    #[test]
    fn unknown_parameters_are_named() {
        let (manager, _) = manager(true);
        let result = block(manager.call(
            &json!({"action": "search", "version": "1", "source": "x"}),
            Some(&card(&[])),
        ));
        assert_eq!(
            result.text_content(),
            "unknown parameter(s) source, version"
        );
    }

    #[test]
    fn search_returns_at_most_ten_results() {
        let (manager, _) = manager(true);
        let result =
            block(manager.call(&json!({"action": "search", "query": "x"}), Some(&card(&[]))));
        let answer: Value = serde_json::from_str(&result.text_content()).unwrap();
        assert_eq!(answer["results"].as_array().unwrap().len(), MAX_RESULTS);
    }

    #[test]
    fn each_row_is_installed_or_skipped_as_the_user_answers() {
        let (manager, host) = manager(true);
        let card = card(&[true, false]);
        let result = block(manager.call(
            &json!({"action": "install", "reason": "for the forecast", "items": [
                {"kind": "plugin", "id": "weather"}, {"kind": "skill", "id": "github-pr"}
            ]}),
            Some(&card),
        ));
        let answer: Value = serde_json::from_str(&result.text_content()).unwrap();
        assert_eq!(answer["items"][0]["status"], "installed");
        assert_eq!(answer["items"][1]["status"], "skipped");
        assert_eq!(*host.installed.lock().unwrap(), ["weather"]);
        assert_eq!(*card.shown.lock().unwrap(), 1);
    }

    #[test]
    fn surfaces_without_a_card_get_the_manual_route() {
        let (manager, _) = manager(false);
        let result = block(manager.call(&json!({"action": "search"}), None));
        assert_eq!(result.text_content(), NEEDS_CARD);
    }
}
