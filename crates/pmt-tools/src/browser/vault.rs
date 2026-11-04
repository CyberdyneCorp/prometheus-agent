//! `browser_vault` (spec: security-boundaries, `browser_vault` fills by
//! handle; Vault codes, cards and addresses): the model names vault items
//! by handle and the host puts their values into the current page of the
//! session's browser. The model sees metadata and `{filled_fields,
//! origin}`, never a value; every value filled is also redacted from the
//! session's later `browser_exec` output (`[REDACTED:vault]`).
//!
//! A fill happens only on a page whose origin is one of a login's saved
//! origins (cards and addresses fill on any page, a card only after a
//! human approval each time: the tool's call guard escalates every card
//! fill as a once-only rule, which unattended contexts deny). Saving a
//! login and typing a code without a TOTP seed ask the human through a
//! masked prompt; where no human can be asked they answer
//! `prompt_unavailable`.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use pmt_config::schema::VaultKeyStorage;
use pmt_core::capability::Capability;
use pmt_core::secret::SecretString;
use pmt_vault::{Item, Kind, Login, Secret, Vault};
use serde_json::{Value, json};

use super::exec::BrowserExec;
use super::{TOOLSET, vault_js};
use crate::descriptor::{
    CallContext, HandlerFuture, Owner, RegisteredTool, ResourceClaims, ToolDescriptor, ToolHandler,
};
use crate::dispatch::{Escalation, SeamFuture};
use crate::environments::{Environments, part};
use crate::result::{Status, ToolResult};

/// The rule key a card fill is approved under (never remembered).
pub const CARD_RULE: &str = "vault_card_fill";

/// Asks the human for a hidden value on their surface.
pub trait MaskedPrompt: Send + Sync {
    /// `question` with the answer hidden; `None` when nobody answers.
    fn ask<'a>(&'a self, name: &'a str, question: &'a str) -> SeamFuture<'a, Option<SecretString>>;
}

/// One session's vault access.
pub struct VaultSession {
    /// The profile's vault.
    pub vault: Arc<Vault>,
    /// `vault.key_storage` for a vault created by `save_login`.
    pub storage: VaultKeyStorage,
    /// The session's browser.
    pub browser: Arc<BrowserExec>,
    /// The human's masked prompt (`None`: cron, webhook, API, single-query).
    pub prompt: Option<Arc<dyn MaskedPrompt>>,
}

struct Handler(Arc<Environments>);

/// `browser_vault`.
#[must_use]
pub fn tool(environments: &Arc<Environments>) -> RegisteredTool {
    RegisteredTool::new(
        descriptor(),
        Owner::Builtin,
        Arc::new(Handler(Arc::clone(environments))),
    )
    .with_guard(Arc::new(|_, arguments: &Value| card_fill(arguments)))
}

/// Every card fill needs a human approval, each time.
fn card_fill(arguments: &Value) -> Option<Escalation> {
    let handle = arguments.get("handle").and_then(Value::as_str)?;
    let fills = arguments.get("action").and_then(Value::as_str) == Some("fill");
    (fills && Kind::of_handle(handle) == Some(Kind::Card)).then(|| Escalation {
        rule_key: CARD_RULE.to_owned(),
        reason: format!("fill payment card {handle} into the current page"),
        once_only: true,
    })
}

impl ToolHandler for Handler {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let session = part(&self.0, context, "vault", |env| env.vault.clone())?;
            Ok::<ToolResult, crate::descriptor::ToolFailure>(
                session.call(&context.session_id, &arguments).await,
            )
        })
    }
}

impl VaultSession {
    /// Runs one `browser_vault` call of session `session_id` (the card
    /// approval is the dispatcher's, before this runs).
    pub async fn call(&self, session_id: &str, arguments: &Value) -> ToolResult {
        let action = arguments["action"].as_str().unwrap_or_default();
        let page = arguments["session"].as_str().unwrap_or("default");
        let call = Call {
            session: self,
            key: format!("{session_id}/{page}"),
            arguments,
        };
        match action {
            "list" => call.list(),
            "fill" => call.fill().await,
            "save_login" => call.save_login().await,
            "enter_code" => call.enter_code().await,
            "unlock" => unlock(arguments),
            other => ToolResult::error(format!("unknown action '{other}'")),
        }
    }
}

/// One call's view of the session.
struct Call<'a> {
    session: &'a VaultSession,
    /// The browser session key (`<session id>/<name>`).
    key: String,
    arguments: &'a Value,
}

impl Call<'_> {
    fn list(&self) -> ToolResult {
        match self.session.vault.list() {
            Ok(items) => json_result(&json!({ "items": items })),
            Err(err) => ToolResult::error(err.message()),
        }
    }

    fn handle(&self) -> Result<Item, ToolResult> {
        let handle = self.arguments["handle"].as_str().unwrap_or_default();
        self.session
            .vault
            .get(handle)
            .map_err(|err| ToolResult::error(err.message()))
    }

    /// The current page's URL and origin.
    async fn page(&self) -> Result<(Arc<super::BrowserSession>, String), ToolResult> {
        let browser = self
            .session
            .browser
            .browser(&self.key)
            .await
            .map_err(|why| ToolResult::error(format!("browser_vault: {why}")))?;
        let url = browser
            .evaluate("location.href")
            .await
            .map_err(|why| ToolResult::error(format!("browser_vault: {why}")))?;
        Ok((browser, url.as_str().unwrap_or_default().to_owned()))
    }

    async fn fill(&self) -> ToolResult {
        let item = match self.handle() {
            Ok(item) => item,
            Err(refused) => return refused,
        };
        let (browser, url) = match self.page().await {
            Ok(page) => page,
            Err(failed) => return failed,
        };
        let Some(origin) = pmt_vault::origin::of(&url) else {
            return refused(&format!("the current page ({url}) has no web origin"));
        };
        let script = match &item.secret {
            Secret::Login(login) => {
                if !pmt_vault::origin::admits(&login.origins, &url) {
                    return refused(&format!(
                        "{} is saved for {}, not {origin}; nothing was typed",
                        item.handle,
                        login.origins.join(", ")
                    ));
                }
                vault_js::login(&login.identifier, &login.password)
            }
            Secret::Card(card) => {
                vault_js::card(&card.holder, &card.number, &card.expiry, &card.cvc)
            }
            Secret::Address(a) => vault_js::address(&[
                &a.name,
                &a.line1,
                &a.line2,
                &a.city,
                &a.region,
                &a.postal_code,
                &a.country,
            ]),
        };
        self.session.browser.redact_filled(&item.secret_values());
        self.type_into(&browser, &script, &origin).await
    }

    /// Runs a fill script; the result is `{filled_fields, origin}`.
    async fn type_into(
        &self,
        browser: &super::BrowserSession,
        script: &str,
        origin: &str,
    ) -> ToolResult {
        match browser.evaluate(script).await {
            Ok(filled) => json_result(&json!({
                "filled_fields": filled.as_u64().unwrap_or(0),
                "origin": origin,
            })),
            // A page exception never carries the values: they are
            // arguments, not part of the message.
            Err(why) => {
                ToolResult::error(format!("browser_vault: the page refused the fill: {why}"))
            }
        }
    }

    async fn save_login(&self) -> ToolResult {
        let Some(prompt) = &self.session.prompt else {
            return prompt_unavailable();
        };
        let identifier = self.arguments["identifier"].as_str().unwrap_or_default();
        if identifier.is_empty() {
            return ToolResult::error("save_login needs the identifier (user name or email)");
        }
        let origin = match self.arguments["origin"].as_str() {
            Some(origin) => pmt_vault::origin::of(origin),
            None => self
                .page()
                .await
                .ok()
                .and_then(|(_, url)| pmt_vault::origin::of(&url)),
        };
        let Some(origin) = origin else {
            return ToolResult::error("save_login needs the site's origin (https://host)");
        };
        let question = format!("Password for {identifier} on {origin} (saved to the vault)");
        let Some(password) = prompt.ask("VAULT_PASSWORD", &question).await else {
            return refused("the person did not enter a password; nothing was saved");
        };
        let label = self.arguments["label"]
            .as_str()
            .unwrap_or(&origin)
            .to_owned();
        let item = Item {
            handle: Kind::Login.new_handle(),
            label,
            secret: Secret::Login(Login {
                origins: vec![origin.clone()],
                identifier: identifier.to_owned(),
                password: password.expose().to_owned(),
                totp: None,
            }),
        };
        match self.session.vault.add(item, self.session.storage) {
            Ok(meta) => json_result(&json!({ "saved": meta })),
            Err(err) => ToolResult::error(err.message()),
        }
    }

    async fn enter_code(&self) -> ToolResult {
        if let Some(method @ ("passkey" | "app")) = self.arguments["method"].as_str() {
            return json_result(&json!({
                "status": "needs_user",
                "message": format!("This step needs a {method} on the user's device: ask the user to complete it there, then continue."),
            }));
        }
        let item = match self.handle() {
            Ok(item) => item,
            Err(refused) => return refused,
        };
        let Secret::Login(login) = &item.secret else {
            return ToolResult::error("enter_code needs a login handle");
        };
        let (browser, url) = match self.page().await {
            Ok(page) => page,
            Err(failed) => return failed,
        };
        let Some(origin) = pmt_vault::origin::of(&url) else {
            return refused(&format!("the current page ({url}) has no web origin"));
        };
        let code = match self.code(&item.handle, login, &url, &origin).await {
            Ok(code) => code,
            Err(stopped) => return stopped,
        };
        self.session.browser.redact_filled(&[code.expose()]);
        self.type_into(&browser, &vault_js::code(code.expose()), &origin)
            .await
    }
}

impl Call<'_> {
    /// The code to type: the TOTP code of a seeded login (on its origins
    /// only), else what the person types into a masked prompt.
    async fn code(
        &self,
        handle: &str,
        login: &Login,
        url: &str,
        origin: &str,
    ) -> Result<SecretString, ToolResult> {
        if let Some(seed) = &login.totp {
            if !pmt_vault::origin::admits(&login.origins, url) {
                return Err(refused(&format!(
                    "{handle} is saved for {}, not {origin}; no code was typed",
                    login.origins.join(", ")
                )));
            }
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            return pmt_vault::totp::code(seed, now)
                .map(SecretString::new)
                .ok_or_else(|| ToolResult::error("the item's TOTP seed is not base32"));
        }
        let Some(prompt) = &self.session.prompt else {
            return Err(prompt_unavailable());
        };
        let question = format!("Verification code for {origin}");
        prompt
            .ask("VAULT_CODE", &question)
            .await
            .ok_or_else(|| refused("the person did not enter a code; nothing was typed"))
    }
}

/// Password-manager sources (1Password, Bitwarden) are not part of this
/// build: every source is reported unavailable.
fn unlock(arguments: &Value) -> ToolResult {
    let source = arguments["source"].as_str().unwrap_or_default();
    json_result(&json!({
        "source": source,
        "status": "unavailable_in_this_session",
        "message": "password manager sources are not available in this build; use the Prometheus vault",
    }))
}

fn json_result(value: &Value) -> ToolResult {
    ToolResult::ok(value.to_string())
}

fn refused(why: &str) -> ToolResult {
    ToolResult::text(Status::Blocked, format!("browser_vault: {why}"))
}

fn prompt_unavailable() -> ToolResult {
    ToolResult::text(
        Status::Error,
        json!({
            "status": "prompt_unavailable",
            "message": "no person can answer a masked prompt in this session (cron, webhook, API or single-query run)",
        })
        .to_string(),
    )
}

fn descriptor() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        "browser_vault",
        TOOLSET,
        "Use the user's saved logins, payment cards and addresses on the current browser page \
         without seeing them. action=list shows handles with label, kind, origins and \
         identifier; fill(handle) types an item into the page (logins only on their saved \
         origins; every card fill asks the user) and returns {filled_fields, origin}; \
         save_login(identifier[, origin, label]) asks the user for the password; \
         enter_code(handle) types the current one-time code (method=passkey|app: tells you \
         to ask the user to approve on their device); unlock(source) reports password \
         manager sources. Never ask the user to type a password into the chat.",
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["list", "fill", "save_login", "enter_code", "unlock"]},
                "handle": {"type": "string", "description": "A handle from action=list."},
                "identifier": {"type": "string", "description": "save_login: the user name or email."},
                "origin": {"type": "string", "description": "save_login: the site (default: the current page's origin)."},
                "label": {"type": "string", "description": "save_login: a name for the item."},
                "method": {"type": "string", "enum": ["totp", "passkey", "app"], "description": "enter_code: how the site verifies."},
                "source": {"type": "string", "description": "unlock: onepassword or bitwarden."},
                "session": {"type": "string", "description": "Named browser session (default: default)."}
            },
            "required": ["action"],
            "additionalProperties": false
        }),
    );
    d.capabilities = ["process.spawn:*", "net.connect:*"]
        .iter()
        .map(|c| Capability::parse(c).expect("valid capability"))
        .collect();
    d.claims = Some(ResourceClaims::exclusive(TOOLSET));
    d
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::card_fill;

    #[test]
    fn only_card_fills_escalate_and_never_are_remembered() {
        let card = card_fill(&json!({"action": "fill", "handle": "card_00aa11bb22cc"})).unwrap();
        assert!(card.once_only);
        assert!(card_fill(&json!({"action": "fill", "handle": "login_00aa11bb22cc"})).is_none());
        assert!(card_fill(&json!({"action": "fill", "handle": "address_00aa11bb22cc"})).is_none());
        assert!(card_fill(&json!({"action": "list"})).is_none());
    }
}
