//! Borrowed CLI logins (spec: model-providers, Borrowed CLI logins;
//! design D16).
//!
//! With `auth.adopt_external_logins = true` a request may use the login of
//! another installed agent CLI when the profile owns no usable sign-in for
//! the provider: Claude Code's for `anthropic`, the Codex CLI's for
//! `openai-codex`. The login is read in place at every use and never
//! copied into the secrets store; it is refreshed only after a request
//! that used it failed with HTTP 401, and the rotated tokens are written
//! back to the owning CLI's store (only the token fields; everything else
//! in it is kept) so that CLI keeps working. With the setting `false`
//! nothing here is ever constructed, so no other CLI's store is read.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use pmt_core::private_fs;
use pmt_core::secret::SecretString;
use serde_json::{Map, Value, json};

use super::jwt;
use super::token::TokenSet;
use crate::resolve::{Credential, CredentialSource, SignedIn};

/// Claude Code (`anthropic`).
pub const CLAUDE_CODE: &str = "claude-code";
/// The Codex CLI (`openai-codex`).
pub const CODEX_CLI: &str = "codex-cli";

/// The macOS keychain service of Claude Code's login.
pub const CLAUDE_CODE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

/// Where another CLI keeps its login.
pub trait LoginStore: Send + Sync + std::fmt::Debug {
    /// The stored text; `None` when there is no login.
    ///
    /// # Errors
    ///
    /// The store cannot be read.
    fn read(&self) -> Result<Option<String>, Error>;

    /// Replaces the stored text.
    ///
    /// # Errors
    ///
    /// The store cannot be written.
    fn write(&self, text: &str) -> Result<(), Error>;
}

/// A login file, replaced atomically on write (owner-only).
#[derive(Debug, Clone)]
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    /// The login file at `path`.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl LoginStore for FileStore {
    fn read(&self) -> Result<Option<String>, Error> {
        match private_fs::read_no_follow(&self.path) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(store_error(&format!(
                "cannot read {}: {e}",
                self.path.display()
            ))),
        }
    }

    fn write(&self, text: &str) -> Result<(), Error> {
        private_fs::write_atomic(&self.path, text.as_bytes())
            .map_err(|e| store_error(&format!("cannot write {}: {e}", self.path.display())))
    }
}

/// Claude Code's store: `<user home>/.claude/.credentials.json`; on macOS,
/// where Claude Code keeps the login in the login keychain, the keychain
/// item is used when that file is absent and Claude Code's directory
/// exists (an installed Claude Code; a home without it is never looked up
/// in the keychain).
#[derive(Debug, Clone)]
pub struct ClaudeCodeStore {
    /// `<user home>/.claude/.credentials.json`.
    file: FileStore,
}

impl ClaudeCodeStore {
    /// Claude Code's store for `user_home`.
    #[must_use]
    pub fn new(user_home: &Path) -> Self {
        let file = FileStore::new(user_home.join(".claude").join(".credentials.json"));
        Self { file }
    }

    /// The keychain account Claude Code uses: the user name, which is the
    /// last component of the user's home directory on macOS.
    #[cfg(target_os = "macos")]
    fn keychain_account(&self) -> Option<String> {
        let home = self.file.path.parent()?.parent()?;
        Some(home.file_name()?.to_string_lossy().into_owned())
    }

    /// No credentials file, but Claude Code's directory exists.
    #[cfg(target_os = "macos")]
    fn use_keychain(&self) -> bool {
        !self.file.path.exists() && self.file.path.parent().is_some_and(Path::is_dir)
    }
}

impl LoginStore for ClaudeCodeStore {
    fn read(&self) -> Result<Option<String>, Error> {
        #[cfg(target_os = "macos")]
        if self.use_keychain() {
            let Some(account) = self.keychain_account() else {
                return Ok(None);
            };
            return pmt_config::secrets::keychain::read_item(
                CLAUDE_CODE_KEYCHAIN_SERVICE,
                &account,
            );
        }
        self.file.read()
    }

    fn write(&self, text: &str) -> Result<(), Error> {
        #[cfg(target_os = "macos")]
        if self.use_keychain() {
            let account = self
                .keychain_account()
                .ok_or_else(|| store_error("cannot name the Claude Code keychain account"))?;
            return pmt_config::secrets::keychain::write_item(
                CLAUDE_CODE_KEYCHAIN_SERVICE,
                &account,
                text,
            );
        }
        self.file.write(text)
    }
}

/// The token layout of a CLI's store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// `{"claudeAiOauth": {"accessToken", "refreshToken", "expiresAt" (ms),
    /// "subscriptionType"}}`.
    ClaudeCode,
    /// `{"tokens": {"access_token", "refresh_token", "id_token",
    /// "account_id"}, "last_refresh"}`.
    CodexCli,
}

/// Tokens read from another CLI's store.
#[derive(Debug, Clone)]
pub struct BorrowedTokens {
    /// The access token.
    pub access: SecretString,
    /// The refresh token.
    pub refresh: Option<SecretString>,
    /// Access token expiry.
    pub expires_at: Option<Timestamp>,
    /// The account id.
    pub account_id: Option<String>,
    /// The plan label.
    pub plan: Option<String>,
}

/// One other CLI's login for one provider.
#[derive(Debug)]
pub struct ExternalLogin {
    cli: &'static str,
    provider: &'static str,
    layout: Layout,
    store: Box<dyn LoginStore>,
}

impl ExternalLogin {
    /// Claude Code's login in `store`, for `anthropic`.
    #[must_use]
    pub fn claude_code(store: Box<dyn LoginStore>) -> Self {
        Self {
            cli: CLAUDE_CODE,
            provider: "anthropic",
            layout: Layout::ClaudeCode,
            store,
        }
    }

    /// The Codex CLI's login in `store`, for `openai-codex`.
    #[must_use]
    pub fn codex_cli(store: Box<dyn LoginStore>) -> Self {
        Self {
            cli: CODEX_CLI,
            provider: "openai-codex",
            layout: Layout::CodexCli,
            store,
        }
    }

    /// The CLI's name (`claude-code`, `codex-cli`).
    #[must_use]
    pub fn cli(&self) -> &'static str {
        self.cli
    }

    /// The provider the login is for.
    #[must_use]
    pub fn provider(&self) -> &'static str {
        self.provider
    }

    /// The label listings show: `borrowed:<cli>`.
    #[must_use]
    pub fn label(&self) -> String {
        format!("borrowed:{}", self.cli)
    }

    /// The login's tokens; `None` when the CLI holds no readable login.
    ///
    /// # Errors
    ///
    /// The store cannot be read.
    pub fn read(&self) -> Result<Option<BorrowedTokens>, Error> {
        let Some(text) = self.store.read()? else {
            return Ok(None);
        };
        let Ok(json) = serde_json::from_str::<Value>(&text) else {
            tracing::warn!(cli = self.cli, "the borrowed login is not JSON; ignored");
            return Ok(None);
        };
        Ok(match self.layout {
            Layout::ClaudeCode => claude_tokens(&json),
            Layout::CodexCli => codex_tokens(&json),
        })
    }

    /// The login as a request credential's sign-in.
    ///
    /// # Errors
    ///
    /// The store cannot be read.
    pub fn signed_in(&self) -> Result<Option<SignedIn>, Error> {
        Ok(self.read()?.map(|tokens| self.to_signed_in(tokens)))
    }

    pub(crate) fn to_signed_in(&self, tokens: BorrowedTokens) -> SignedIn {
        SignedIn {
            label: self.label(),
            token: tokens.access,
            account_id: tokens.account_id,
            ineligible: false,
            borrowed: Some(self.cli.to_owned()),
        }
    }

    /// Writes refreshed `tokens` back into the CLI's store, changing only
    /// the token fields (a refresh token that was not rotated is kept).
    ///
    /// # Errors
    ///
    /// The store cannot be read or written.
    pub fn write_back(&self, tokens: &TokenSet) -> Result<(), Error> {
        let text = self.store.read()?.unwrap_or_default();
        // A store that is not a JSON object is replaced by one.
        let mut root: Map<String, Value> = serde_json::from_str(&text).unwrap_or_default();
        let mut fields = vec![];
        let key = match self.layout {
            Layout::ClaudeCode => {
                fields.push(("accessToken", tokens.access_token.expose().into()));
                if let Some(refresh) = &tokens.refresh_token {
                    fields.push(("refreshToken", refresh.expose().into()));
                }
                if let Some(at) = tokens.expires_at {
                    fields.push(("expiresAt", at.as_millisecond().into()));
                }
                "claudeAiOauth"
            }
            Layout::CodexCli => {
                fields.push(("access_token", tokens.access_token.expose().into()));
                if let Some(refresh) = &tokens.refresh_token {
                    fields.push(("refresh_token", refresh.expose().into()));
                }
                if let Some(id_token) = &tokens.id_token {
                    fields.push(("id_token", id_token.expose().into()));
                }
                root.insert("last_refresh".into(), Timestamp::now().to_string().into());
                "tokens"
            }
        };
        let entry = root.entry(key).or_insert_with(|| json!({}));
        if !entry.is_object() {
            *entry = json!({});
        }
        if let Some(entry) = entry.as_object_mut() {
            for (name, value) in fields {
                entry.insert(name.to_owned(), value);
            }
        }
        self.store.write(&format!("{:#}", Value::Object(root)))
    }
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn claude_tokens(json: &Value) -> Option<BorrowedTokens> {
    let entry = json.get("claudeAiOauth")?;
    Some(BorrowedTokens {
        access: SecretString::new(text(entry, "accessToken")?),
        refresh: text(entry, "refreshToken").map(SecretString::new),
        expires_at: entry
            .get("expiresAt")
            .and_then(Value::as_i64)
            .and_then(|ms| Timestamp::from_millisecond(ms).ok()),
        account_id: None,
        plan: text(entry, "subscriptionType"),
    })
}

fn codex_tokens(json: &Value) -> Option<BorrowedTokens> {
    let tokens = json.get("tokens")?;
    let access = text(tokens, "access_token")?;
    let id_token = text(tokens, "id_token").unwrap_or_default();
    let account = jwt::account(&[&id_token, &access]);
    let expires_at = jwt::claims(&access)
        .and_then(|c| c.get("exp").and_then(Value::as_i64))
        .and_then(|exp| Timestamp::from_second(exp).ok());
    Some(BorrowedTokens {
        account_id: account.account_id.or_else(|| text(tokens, "account_id")),
        plan: account.plan,
        refresh: text(tokens, "refresh_token").map(SecretString::new),
        access: SecretString::new(access),
        expires_at,
    })
}

fn store_error(message: &str) -> Error {
    Error::new(&codes::AUTH_REFRESH_FAILED, message)
}

/// The other CLIs' logins a profile may borrow.
#[derive(Debug, Default)]
pub struct Borrowed {
    logins: Vec<ExternalLogin>,
}

impl Borrowed {
    /// No borrowing (`auth.adopt_external_logins = false`).
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// The logins of the CLIs installed for `user_home` when `adopt`;
    /// nothing is read until a request needs a login.
    #[must_use]
    pub fn discover(adopt: bool, user_home: Option<&Path>) -> Self {
        let Some(home) = user_home.filter(|_| adopt) else {
            return Self::none();
        };
        Self::with(vec![
            ExternalLogin::claude_code(Box::new(ClaudeCodeStore::new(home))),
            ExternalLogin::codex_cli(Box::new(FileStore::new(super::codex::default_path(home)))),
        ])
    }

    /// Exactly `logins`.
    #[must_use]
    pub fn with(logins: Vec<ExternalLogin>) -> Self {
        Self { logins }
    }

    /// The login that may stand in for `provider`'s sign-in.
    #[must_use]
    pub fn login(&self, provider: &str) -> Option<&ExternalLogin> {
        self.logins.iter().find(|l| l.provider == provider)
    }

    /// The login of `cli`.
    #[must_use]
    pub fn of_cli(&self, cli: &str) -> Option<&ExternalLogin> {
        self.logins.iter().find(|l| l.cli == cli)
    }
}

/// The once-per-session notice of a borrowed login's first use.
#[derive(Debug, Default)]
pub struct Announcements {
    shown: BTreeSet<String>,
}

impl Announcements {
    /// The notice to show when `credential` is a borrowed login used for
    /// the first time in this session.
    pub fn first_use(&mut self, credential: &Credential) -> Option<String> {
        let CredentialSource::Borrowed { provider, cli } = &credential.source else {
            return None;
        };
        if !self.shown.insert(format!("{provider}/{cli}")) {
            return None;
        }
        let name = match cli.as_str() {
            CLAUDE_CODE => "Claude Code",
            CODEX_CLI => "the Codex CLI",
            other => other,
        };
        Some(format!(
            "Using the {provider} sign-in of {name} (borrowed:{cli}), read in place; set auth.adopt_external_logins = false to stop."
        ))
    }
}
