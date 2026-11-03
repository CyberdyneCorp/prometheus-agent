//! The `options` table of each first-party adapter (spec:
//! platform-adapters, Platform configuration shape and the per-adapter
//! requirements). Each struct is the schema the table is validated
//! against; unknown keys are refused. Cross-key rules (a key required
//! while another is set) are [`crate::gateway_check`]'s.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::schema::SecretName;

/// `gateway.platforms.telegram.options`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TelegramOptions {
    /// Seconds one long-poll `getUpdates` call waits for updates.
    #[schemars(range(min = 1, max = 50))]
    pub poll_timeout_secs: u64,
    /// How updates arrive: long polling, or webhook posts on the shared
    /// listener at `/p/<profile>/telegram/`.
    pub mode: TelegramMode,
    /// The public HTTPS URL Telegram posts updates to in webhook mode (it
    /// must reach `/p/<profile>/telegram/` on the shared listener).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook_public_url: Option<String>,
    /// The secret token Telegram sends with every webhook update
    /// (required in webhook mode).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook_secret_secret: Option<SecretName>,
    /// Register the gateway's commands and skills as the bot's command
    /// menu; `false` on a follower daemon sharing one bot.
    pub slash_commands: bool,
    /// How the command menu is cut to the platform's cap.
    pub command_menu: CommandMenu,
    /// Private chats in threaded mode: multi-session topics (`enabled`)
    /// and the topics declared per chat id.
    #[serde(skip_serializing_if = "DmTopics::is_unset")]
    pub dm_topics: DmTopics,
    /// Topics of forum groups, by chat id.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub group_topics: BTreeMap<String, Vec<TopicSpec>>,
    /// Ignore ordinary messages in the root of a private chat (slash
    /// commands are still accepted).
    pub ignore_root_dm: bool,
    /// Send replies as Telegram rich messages (Bot API 10.1
    /// `sendRichMessage`: the raw Markdown, up to 32768 characters), falling
    /// back to `MarkdownV2` when Telegram refuses one.
    pub rich_messages: bool,
}

impl Default for TelegramOptions {
    fn default() -> Self {
        Self {
            poll_timeout_secs: 30,
            mode: TelegramMode::Polling,
            webhook_public_url: None,
            webhook_secret_secret: None,
            slash_commands: true,
            command_menu: CommandMenu::default(),
            dm_topics: DmTopics::default(),
            group_topics: BTreeMap::new(),
            ignore_root_dm: false,
            rich_messages: false,
        }
    }
}

/// How Telegram updates arrive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TelegramMode {
    /// `getUpdates` long polling.
    #[default]
    Polling,
    /// Webhook posts on the shared listener.
    Webhook,
}

/// `options.command_menu`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CommandMenu {
    /// Entries registered at most.
    #[schemars(range(min = 1, max = 100))]
    pub max_commands: u32,
    /// Commands placed first (or last, or alone) whatever the cap.
    pub priority: Vec<String>,
    /// Where the priority entries go.
    pub priority_mode: PriorityMode,
}

impl Default for CommandMenu {
    fn default() -> Self {
        Self {
            max_commands: 60,
            priority: Vec::new(),
            priority_mode: PriorityMode::Prepend,
        }
    }
}

/// Where a command menu's priority entries go.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PriorityMode {
    /// First, then the rest in order.
    #[default]
    Prepend,
    /// Last, after as many others as fit.
    Append,
    /// Only the priority entries.
    Replace,
}

/// `options.dm_topics`: `enabled` turns the private chat into a lobby of
/// topics, each its own session (spec: messaging-gateway, Multi-session DM
/// topics); every other key is a chat id with its declared topics.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DmTopics {
    /// Each DM topic is its own session and the root DM is a lobby that
    /// accepts only commands (`/topic new|list|rename|restore`).
    #[serde(default)]
    pub enabled: bool,
    /// Declared topics by chat id.
    #[serde(flatten)]
    pub chats: BTreeMap<String, Vec<TopicSpec>>,
}

impl DmTopics {
    /// Nothing set (left out of a written config).
    #[must_use]
    pub fn is_unset(&self) -> bool {
        !self.enabled && self.chats.is_empty()
    }
}

/// One declared topic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TopicSpec {
    /// The topic's name.
    pub name: String,
    /// Its thread id when it exists already; unset, the adapter creates
    /// it (in a private chat) and remembers the id in profile state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<i64>,
    /// The icon color Telegram shows (RGB as an integer).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_color: Option<u32>,
    /// A skill preloaded into every new session of the topic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
}

/// `gateway.platforms.discord.options`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)] // independent switches
pub struct DiscordOptions {
    /// Answer only messages that mention the bot in server channels
    /// (direct messages are always answered).
    pub require_mention: bool,
    /// Channels answered without a mention even when `require_mention`
    /// is on.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub free_response_channels: Vec<String>,
    /// Register the gateway's commands as application commands; `false`
    /// on a follower daemon sharing one application.
    pub slash_commands: bool,
    /// Answer each mention in a text channel in a new thread.
    pub auto_thread: bool,
    /// Also open threads in free-response channels.
    pub free_response_auto_thread: bool,
    /// Channels where answers always stay inline.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub no_thread_channels: Vec<String>,
}

impl Default for DiscordOptions {
    fn default() -> Self {
        Self {
            require_mention: false,
            free_response_channels: Vec::new(),
            slash_commands: true,
            auto_thread: true,
            free_response_auto_thread: false,
            no_thread_channels: Vec::new(),
        }
    }
}

/// `gateway.platforms.slack.options`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SlackOptions {
    /// The secret holding the Socket Mode app-level token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_token_secret: Option<SecretName>,
}

/// `gateway.platforms.email.options`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)] // independent switches
pub struct EmailOptions {
    /// The mailbox address the agent answers as.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// IMAP server host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imap_host: Option<String>,
    /// IMAP server port.
    pub imap_port: u16,
    /// IMAP transport security. Unset: `tls`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub imap_security: Option<MailSecurity>,
    /// The login name, when it is not the address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// SMTP server host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smtp_host: Option<String>,
    /// SMTP server port.
    pub smtp_port: u16,
    /// SMTP transport security. Unset: `tls` on port 465, else
    /// `starttls`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smtp_security: Option<MailSecurity>,
    /// Seconds between two IMAP polls.
    #[schemars(range(min = 1, max = 3600))]
    pub poll_interval_secs: u64,
    /// Verify the servers' certificates (a WARN at every start when off
    /// for a non-loopback host).
    pub tls_verify: bool,
    /// Skip attachment parts without decoding them.
    pub skip_attachments: bool,
    /// The receiving server whose `Authentication-Results` header is
    /// trusted (for example `mx.google.com`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authserv_id: Option<String>,
    /// Drop mail whose From domain the trusted `Authentication-Results`
    /// header does not authenticate.
    pub require_authenticated_sender: bool,
}

impl Default for EmailOptions {
    fn default() -> Self {
        Self {
            address: None,
            imap_host: None,
            imap_port: 993,
            imap_security: None,
            username: None,
            smtp_host: None,
            smtp_port: 587,
            smtp_security: None,
            poll_interval_secs: 15,
            tls_verify: true,
            skip_attachments: false,
            authserv_id: None,
            require_authenticated_sender: true,
        }
    }
}

/// How a mail connection is secured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MailSecurity {
    /// TLS from the first byte.
    Tls,
    /// Plain, upgraded with STARTTLS before logging in.
    Starttls,
    /// No encryption (loopback hosts only).
    Plain,
}

impl EmailOptions {
    /// The IMAP security in effect.
    #[must_use]
    pub fn imap_security(&self) -> MailSecurity {
        self.imap_security.unwrap_or(MailSecurity::Tls)
    }

    /// The SMTP security in effect.
    #[must_use]
    pub fn smtp_security(&self) -> MailSecurity {
        self.smtp_security.unwrap_or(if self.smtp_port == 465 {
            MailSecurity::Tls
        } else {
            MailSecurity::Starttls
        })
    }
}

/// `gateway.platforms.matrix.options`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MatrixOptions {
    /// The homeserver URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub homeserver: Option<String>,
}

/// `gateway.platforms.signal.options`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SignalOptions {
    /// The signal-cli HTTP daemon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_url: Option<String>,
    /// The account number, in E.164.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// Groups admitted (`["*"]`: every group). Unset: no group.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_groups: Option<Vec<String>>,
}

/// `gateway.platforms.whatsapp.options`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct WhatsappOptions {
    /// A `WhatsApp` bridge already running (`http://127.0.0.1:<port>`).
    /// Unset: the adapter starts the managed bridge runtime itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bridge_url: Option<String>,
    /// Whose number the linked device is.
    pub mode: WhatsappMode,
    /// Which groups are admitted.
    pub group_policy: GroupPolicy,
}

/// Whose number a linked `WhatsApp` device is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WhatsappMode {
    /// A number dedicated to the agent.
    #[default]
    Bot,
    /// The user's own number: only the user's chat with themselves is
    /// admitted.
    SelfChat,
}

/// Which groups a `WhatsApp` adapter admits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GroupPolicy {
    /// None until the owner approves the group's pairing code.
    #[default]
    Pairing,
    /// Only groups in the platform's `allow_chats`.
    Allowlist,
    /// Every group (members still pass the sender checks).
    Open,
}

/// `gateway.platforms.webhook.options`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct WebhookOptions {}
