//! Feishu and Lark setup and document comment rules (spec:
//! platform-adapters, Feishu and Lark adapter; Feishu document comments
//! and meeting invitations).
//!
//! `prometheus gateway setup feishu` creates the bot app by QR scan: the
//! accounts service's registration device flow (`init`, `begin`, then
//! `poll` until the user scans and approves in Feishu or Lark) answers the
//! new app's id and secret; the secret goes to the secrets store as
//! `FEISHU_APP_SECRET`, the id and the tenant's domain (a Lark tenant
//! switches it to `lark`) to `[gateway.platforms.feishu]` options `app_id`
//! and `domain`. Options `accounts_url` and `api_base` point elsewhere.
//!
//! `prometheus gateway feishu comment-rules status|check|pairing` shows
//! and tests the comment rules the adapter applies (the plugin's file
//! `comment_rules.json` and its pairing store, both in the profile's
//! `plugin-data/feishu/`), resolved exactly as the adapter does: the exact
//! document rule, else the wildcard `*` rule, else the top level, missing
//! fields from the top level.

use std::fmt::Write as _;
use std::time::Duration;

use clap::{Args, Subcommand};
use pmt_core::error::{Error, codes};
use pmt_core::secret::SecretString;
use pmt_plugins::host::state::StateDir;
use serde_json::{Value, json};

use crate::context::Context;

const PLATFORM: &str = "feishu";
const SECRET: &str = "FEISHU_APP_SECRET";
const RULES_FILE: &str = "comment_rules.json";
const PAIRING_KEY: &str = "comment_pairing";
const REGISTRATION: &str = "/oauth/v1/app/registration";

/// `prometheus gateway feishu`.
#[derive(Debug, Clone, Args)]
pub struct FeishuArgs {
    /// The subcommand.
    #[command(subcommand)]
    pub command: FeishuCommand,
}

/// `gateway feishu` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum FeishuCommand {
    /// Document comment access rules.
    #[command(name = "comment-rules", subcommand)]
    CommentRules(RulesCommand),
}

/// `gateway feishu comment-rules` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum RulesCommand {
    /// Print the rules file, its rules and the paired commenters.
    Status,
    /// Show which rule applies to a commenter on a document and whether
    /// the bot would answer them.
    Check {
        /// The document as `<file_type>:<file_token>` (`docx:doxcn...`).
        document: String,
        /// The commenter's open id.
        user: String,
    },
    /// The commenters approved under the `pairing` policy.
    #[command(subcommand)]
    Pairing(PairingCommand),
}

/// `comment-rules pairing` subcommands.
#[derive(Debug, Clone, Subcommand)]
pub enum PairingCommand {
    /// Approve a commenter.
    Add {
        /// Their open id.
        user: String,
    },
    /// Withdraw an approval.
    Remove {
        /// Their open id.
        user: String,
    },
    /// List the approved commenters.
    List,
}

/// A resolved comment rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Resolved {
    /// Comments are answered.
    pub enabled: bool,
    /// `allowlist` or `pairing`.
    pub policy: String,
    /// Listed commenters.
    pub allow_from: Vec<String>,
    /// `exact:<key>`, `wildcard` or `top`.
    pub source: String,
}

fn strings(value: &Value) -> Option<Vec<String>> {
    value.as_array().map(|list| {
        list.iter()
            .filter_map(Value::as_str)
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

fn policy(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_ascii_lowercase)
        .filter(|p| p == "allowlist" || p == "pairing")
}

/// The rule of `file_type:file_token` in `config` (as the adapter's
/// `rules::resolve`).
#[must_use]
pub(super) fn resolve(config: &Value, file_type: &str, file_token: &str) -> Resolved {
    let exact = format!("{file_type}:{file_token}");
    let documents = &config["documents"];
    let (rule, source) = if documents[&exact].is_object() {
        (&documents[&exact], format!("exact:{exact}"))
    } else if documents["*"].is_object() {
        (&documents["*"], "wildcard".to_owned())
    } else {
        (config, "top".to_owned())
    };
    Resolved {
        enabled: rule["enabled"]
            .as_bool()
            .or(config["enabled"].as_bool())
            .unwrap_or(true),
        policy: policy(&rule["policy"])
            .or_else(|| policy(&config["policy"]))
            .unwrap_or_else(|| "pairing".to_owned()),
        allow_from: strings(&rule["allow_from"])
            .or_else(|| strings(&config["allow_from"]))
            .unwrap_or_default(),
        source,
    }
}

fn state_error(e: &impl std::fmt::Debug) -> Error {
    Error::new(&codes::HOME_IO, format!("Feishu plugin data: {e:?}"))
}

/// The profile's comment rules and pairing store.
struct Store {
    state: StateDir,
}

impl Store {
    fn rules(&self) -> Value {
        self.state
            .read_file(RULES_FILE)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}))
    }

    fn pairing(&self) -> Result<Value, Error> {
        let raw = self.state.get(PAIRING_KEY).map_err(|e| state_error(&e))?;
        Ok(raw
            .and_then(|b| serde_json::from_slice(&b).ok())
            .filter(|v: &Value| v["approved"].is_object())
            .unwrap_or_else(|| json!({"approved": {}})))
    }

    fn approved(&self) -> Result<Vec<String>, Error> {
        Ok(self.pairing()?["approved"]
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default())
    }

    /// Adds or removes `user`; whether the store changed.
    fn pair(&self, user: &str, add: bool) -> Result<bool, Error> {
        let mut pairing = self.pairing()?;
        let approved = pairing["approved"]
            .as_object_mut()
            .ok_or_else(|| state_error(&"pairing store"))?;
        let changed = if add {
            let at = jiff::Timestamp::now().as_second();
            approved
                .insert(user.to_owned(), json!({"approved_at": at}))
                .is_none()
        } else {
            approved.remove(user).is_some()
        };
        if changed {
            self.state
                .set(PAIRING_KEY, pairing.to_string().as_bytes())
                .map_err(|e| state_error(&e))?;
        }
        Ok(changed)
    }
}

fn list(values: &[String]) -> String {
    if values.is_empty() {
        "[]".to_owned()
    } else {
        format!("[{}]", values.join(", "))
    }
}

fn status(store: &Store) -> Result<String, Error> {
    let path = store.state.root().join("files").join(RULES_FILE);
    let rules = store.rules();
    let top = resolve(
        &json!({"enabled": rules["enabled"], "policy": rules["policy"], "allow_from": rules["allow_from"]}),
        "",
        "",
    );
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Rules file: {} ({})",
        path.display(),
        if path.exists() {
            "present"
        } else {
            "absent: defaults apply"
        }
    );
    let _ = writeln!(
        out,
        "Top level: enabled={} policy={} allow_from={}",
        top.enabled,
        top.policy,
        list(&top.allow_from)
    );
    let documents = rules["documents"].as_object().cloned().unwrap_or_default();
    if documents.is_empty() {
        out.push_str("Document rules: none\n");
    }
    for (key, rule) in &documents {
        let _ = writeln!(out, "Document rule [{key}]: {rule}");
    }
    let approved = store.approved()?;
    let _ = writeln!(
        out,
        "Paired commenters ({}): {}",
        approved.len(),
        list(&approved)
    );
    Ok(out)
}

fn check(store: &Store, document: &str, user: &str) -> Result<String, Error> {
    let (file_type, token) = document.split_once(':').ok_or_else(|| {
        Error::new(
            &codes::CLI_USAGE,
            format!("'{document}' is not <file_type>:<file_token>"),
        )
    })?;
    let rule = resolve(&store.rules(), file_type, token);
    let allowed = rule.enabled
        && (rule.allow_from.iter().any(|u| u == user)
            || (rule.policy == "pairing" && store.approved()?.iter().any(|u| u == user)));
    Ok(format!(
        "Document: {document}\nUser: {user}\nRule: {} (enabled={} policy={} allow_from={})\nResult: {}\n",
        rule.source,
        rule.enabled,
        rule.policy,
        list(&rule.allow_from),
        if allowed { "ALLOWED" } else { "DENIED" }
    ))
}

fn pairing(store: &Store, command: &PairingCommand) -> Result<String, Error> {
    Ok(match command {
        PairingCommand::Add { user } => {
            if store.pair(user, true)? {
                format!("Approved {user}\n")
            } else {
                format!("{user} was already approved\n")
            }
        }
        PairingCommand::Remove { user } => {
            if store.pair(user, false)? {
                format!("Removed {user}\n")
            } else {
                format!("{user} was not approved\n")
            }
        }
        PairingCommand::List => {
            let approved = store.approved()?;
            if approved.is_empty() {
                "No paired commenters\n".to_owned()
            } else {
                let mut text = approved.join("\n");
                text.push('\n');
                text
            }
        }
    })
}

/// Runs `gateway feishu ...`.
///
/// # Errors
///
/// A malformed document key; plugin data that cannot be read or written.
pub(super) fn run(args: &FeishuArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let scope = ctx.scope()?;
    let store = Store {
        state: StateDir::new(scope.dir(), PLATFORM),
    };
    let FeishuCommand::CommentRules(command) = &args.command;
    let text = match command {
        RulesCommand::Status => status(&store)?,
        RulesCommand::Check { document, user } => check(&store, document, user)?,
        RulesCommand::Pairing(command) => pairing(&store, command)?,
    };
    crate::output::print_raw(&text)?;
    Ok(0)
}

/// The endpoints setup talks to.
struct Endpoints {
    accounts: String,
    api: String,
}

fn endpoints(scope: &pmt_config::ProfileScope, domain: &str) -> Endpoints {
    let options = scope
        .config()
        .gateway
        .platforms
        .get(PLATFORM)
        .map(|p| p.options.clone())
        .unwrap_or_default();
    let text = |key: &str| {
        options
            .get(key)
            .and_then(Value::as_str)
            .map(|v| v.trim_end_matches('/').to_owned())
    };
    let lark = domain == "lark";
    Endpoints {
        accounts: text("accounts_url").unwrap_or_else(|| {
            if lark {
                "https://accounts.larksuite.com"
            } else {
                "https://accounts.feishu.cn"
            }
            .to_owned()
        }),
        api: text("api_base").unwrap_or_else(|| {
            if lark {
                "https://open.larksuite.com"
            } else {
                "https://open.feishu.cn"
            }
            .to_owned()
        }),
    }
}

fn failed(what: &str, why: impl std::fmt::Display) -> Error {
    Error::new(&codes::NET_REQUEST, format!("Feishu {what}: {why}"))
}

/// Posts a registration step; the answer is JSON even on 4xx (a pending
/// poll is a 400).
async fn register(
    client: &reqwest::Client,
    accounts: &str,
    form: &[(&str, &str)],
) -> Result<Value, Error> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(form)
        .finish();
    let response = client
        .post(format!("{accounts}{REGISTRATION}"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| failed("registration", e))?;
    Ok(response.json().await.unwrap_or(Value::Null))
}

/// The app's id, secret and tenant domain once the user scanned and
/// approved.
async fn scan(
    client: &reqwest::Client,
    accounts: &str,
    domain: &str,
) -> Result<(String, String, String), Error> {
    let init = register(client, accounts, &[("action", "init")]).await?;
    let methods = init["supported_auth_methods"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !methods.iter().any(|m| m == "client_secret") {
        return Err(failed(
            "registration",
            format!("client_secret is not offered ({init})"),
        ));
    }
    let begin = register(
        client,
        accounts,
        &[
            ("action", "begin"),
            ("archetype", "PersonalAgent"),
            ("auth_method", "client_secret"),
            ("request_user_info", "open_id"),
        ],
    )
    .await?;
    let device = begin["device_code"]
        .as_str()
        .ok_or_else(|| failed("registration", "no device_code in the answer"))?
        .to_owned();
    let url = begin["verification_uri_complete"]
        .as_str()
        .unwrap_or_default();
    let qr = super::whatsapp_cmd::qr_text(url)?;
    crate::output::print_raw(&format!(
        "Scan this QR code with the Feishu or Lark app (or open {url}):\n{qr}\n"
    ))?;
    let interval = Duration::from_secs(begin["interval"].as_u64().unwrap_or(5).max(1));
    let deadline =
        std::time::Instant::now() + Duration::from_secs(begin["expire_in"].as_u64().unwrap_or(600));
    let mut domain = domain.to_owned();
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(interval).await;
        let Ok(answer) = register(
            client,
            accounts,
            &[
                ("action", "poll"),
                ("device_code", &device),
                ("tp", "ob_app"),
            ],
        )
        .await
        else {
            continue;
        };
        if answer["user_info"]["tenant_brand"] == "lark" {
            "lark".clone_into(&mut domain);
        }
        if let (Some(id), Some(secret)) = (
            answer["client_id"].as_str(),
            answer["client_secret"].as_str(),
        ) {
            return Ok((id.to_owned(), secret.to_owned(), domain));
        }
        match answer["error"].as_str().unwrap_or_default() {
            "access_denied" => {
                return Err(Error::new(
                    &codes::AUTH_LOGIN_FAILED,
                    "the Feishu app creation was denied",
                ));
            }
            "expired_token" => break,
            _ => {}
        }
    }
    Err(Error::new(
        &codes::AUTH_LOGIN_EXPIRED,
        "the Feishu QR code expired before it was approved; run `prometheus gateway setup feishu` again",
    ))
}

/// The bot's name, when the new credentials work.
async fn bot_name(client: &reqwest::Client, api: &str, id: &str, secret: &str) -> Option<String> {
    let token: Value = client
        .post(format!(
            "{api}/open-apis/auth/v3/tenant_access_token/internal"
        ))
        .json(&json!({"app_id": id, "app_secret": secret}))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let token = token["tenant_access_token"].as_str()?;
    let info: Value = client
        .get(format!("{api}/open-apis/bot/v3/info"))
        .bearer_auth(token)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let bot = if info["bot"].is_object() {
        &info["bot"]
    } else {
        &info["data"]["bot"]
    };
    bot["app_name"].as_str().map(str::to_owned)
}

/// Runs `gateway setup feishu`: the app is created, its secret stored and
/// its id and domain written to the platform options.
///
/// # Errors
///
/// A refused, denied or expired registration; the secrets store or the
/// config file.
pub(super) async fn setup(scope: &pmt_config::ProfileScope, domain: &str) -> Result<String, Error> {
    let start = endpoints(scope, domain);
    let client = pmt_net::http::client_builder(&pmt_net::OutboundPolicy::default())?
        .build()
        .map_err(|e| Error::new(&codes::NET_CLIENT_INIT, e.to_string()))?;
    let (id, secret, domain) = scan(&client, &start.accounts, domain).await?;
    scope
        .secrets()
        .store()
        .set(SECRET, &SecretString::new(&secret))?;
    let key = |k: &str| format!("gateway.platforms.{PLATFORM}.options.{k}");
    scope.profile().config_file().set_values(&[
        (key("app_id"), toml_edit::Value::from(id.as_str())),
        (key("domain"), toml_edit::Value::from(domain.as_str())),
    ])?;
    let api = endpoints(scope, &domain).api;
    let mut out = format!("Created the {domain} app {id}; its secret is stored as {SECRET}.\n");
    match bot_name(&client, &api, &id, &secret).await {
        Some(name) => {
            let _ = writeln!(out, "Bot: {name}");
        }
        None => out.push_str(
            "The bot could not be reached yet; check the app in the developer console.\n",
        ),
    }
    let _ = writeln!(
        out,
        "Enable it with: prometheus config set gateway.platforms.{PLATFORM}.enabled true"
    );
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_rule_tier_that_exists_decides() {
        let config = json!({
            "policy": "allowlist", "allow_from": ["ou_top"],
            "documents": {"docx:d1": {"allow_from": ["ou_doc"]}, "*": {"policy": "pairing"}},
        });
        let exact = resolve(&config, "docx", "d1");
        assert_eq!(
            (exact.source.as_str(), exact.policy.as_str()),
            ("exact:docx:d1", "allowlist")
        );
        assert_eq!(exact.allow_from, ["ou_doc"]);
        let wild = resolve(&config, "sheet", "s");
        assert_eq!(
            (wild.source.as_str(), wild.policy.as_str()),
            ("wildcard", "pairing")
        );
        assert_eq!(resolve(&json!({}), "docx", "x").source, "top");
    }
}
