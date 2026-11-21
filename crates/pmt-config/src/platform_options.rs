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
#[allow(clippy::struct_excessive_bools)] // independent switches
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
    /// The onboarding service Create with QR asks to create a bot
    /// (`POST <url>/v1/telegram/pairings`, then polls the pairing).
    pub setup_service_url: String,
    /// A self-hosted Bot API server (`http://127.0.0.1:8081`): bots may
    /// then download files up to 2048 MB instead of 20 MB. Log the bot
    /// out of the public server first (`prometheus gateway telegram
    /// logout`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_base_url: Option<String>,
    /// The self-hosted server's file download base; unset:
    /// `<api_base_url>/file`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_base_url: Option<String>,
    /// The self-hosted server runs with `--local`: files are read from the
    /// absolute path it returns (the server's files must be readable here).
    pub local_mode: bool,
}

/// The hosted Telegram onboarding service.
pub const DEFAULT_TELEGRAM_SETUP_URL: &str = "https://setup.hermes-agent.nousresearch.com";

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
            setup_service_url: DEFAULT_TELEGRAM_SETUP_URL.to_owned(),
            api_base_url: None,
            file_base_url: None,
            local_mode: false,
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
    /// The actions the Discord tools offer (`fetch_messages`,
    /// `list_channels`, `add_role`, ...); unset offers every action.
    /// Others are left out of both schemas and refused at call time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_actions: Option<Vec<DiscordAction>>,
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
            server_actions: None,
        }
    }
}

/// An action of the Discord tools (spec: platform-adapters, Discord
/// tools); `gateway.platforms.discord.options.server_actions` lists them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DiscordAction {
    /// `discord`: recent messages of a channel.
    FetchMessages,
    /// `discord`: members of a server matching a name.
    SearchMembers,
    /// `discord`: a thread in a channel.
    CreateThread,
    /// `discord_admin`: the servers the bot is in.
    ListGuilds,
    /// `discord_admin`: one server.
    ServerInfo,
    /// `discord_admin`: a server's channels.
    ListChannels,
    /// `discord_admin`: one channel.
    ChannelInfo,
    /// `discord_admin`: a server's roles.
    ListRoles,
    /// `discord_admin`: one member.
    MemberInfo,
    /// `discord_admin`: a channel's pinned messages.
    ListPins,
    /// `discord_admin`: pins a message.
    PinMessage,
    /// `discord_admin`: unpins a message.
    UnpinMessage,
    /// `discord_admin`: deletes a message.
    DeleteMessage,
    /// `discord_admin`: gives a member a role.
    AddRole,
    /// `discord_admin`: takes a role from a member.
    RemoveRole,
}

impl DiscordAction {
    /// Its configuration name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::FetchMessages => "fetch_messages",
            Self::SearchMembers => "search_members",
            Self::CreateThread => "create_thread",
            Self::ListGuilds => "list_guilds",
            Self::ServerInfo => "server_info",
            Self::ListChannels => "list_channels",
            Self::ChannelInfo => "channel_info",
            Self::ListRoles => "list_roles",
            Self::MemberInfo => "member_info",
            Self::ListPins => "list_pins",
            Self::PinMessage => "pin_message",
            Self::UnpinMessage => "unpin_message",
            Self::DeleteMessage => "delete_message",
            Self::AddRole => "add_role",
            Self::RemoveRole => "remove_role",
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
    /// Reactions that start a turn (`reaction:added:<emoji>`, the reacted
    /// message as reply context): `false` none (reaction events are
    /// acknowledged and dropped), `true` any reaction on the bot's own
    /// messages, or a list of emoji names on any message. The reacting
    /// user passes the same authorization as a message author.
    #[serde(skip_serializing_if = "ReactionTriggers::is_off")]
    pub reaction_triggers: ReactionTriggers,
    /// Where a reaction-triggered turn answers: a channel id, or
    /// `channel:thread_ts`; unset answers beside the reacted message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reaction_trigger_target: Option<String>,
}

/// `gateway.platforms.slack.options.reaction_triggers`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ReactionTriggers {
    /// `false`: none; `true`: reactions on the bot's own messages.
    Switch(bool),
    /// These emoji names (`eyes`, `white_check_mark`) on any message.
    Emoji(Vec<String>),
}

impl Default for ReactionTriggers {
    fn default() -> Self {
        Self::Switch(false)
    }
}

impl ReactionTriggers {
    /// No reaction starts a turn.
    #[must_use]
    pub fn is_off(&self) -> bool {
        matches!(self, Self::Switch(false))
    }

    /// Whether `emoji` added to a message (`on_bot_message`: one the bot
    /// sent) starts a turn.
    #[must_use]
    pub fn triggers(&self, emoji: &str, on_bot_message: bool) -> bool {
        match self {
            Self::Switch(on) => *on && on_bot_message,
            Self::Emoji(names) => names
                .iter()
                .any(|n| n.trim_matches(':').eq_ignore_ascii_case(emoji)),
        }
    }
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
    /// Rooms (internal `!` room ids, not `#` aliases) the bot reads;
    /// checked before any other gating. Two-member rooms (DMs) are exempt;
    /// empty reads every joined room.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allowed_rooms: Vec<String>,
    /// Which invites the bot accepts.
    pub auto_join: MatrixAutoJoin,
    /// How a room maps onto sessions.
    pub session_scope: MatrixSessionScope,
}

/// `gateway.platforms.matrix.options.auto_join`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MatrixAutoJoin {
    /// Invites from authorized users (`allow_users`).
    #[default]
    Authorized,
    /// Every invite.
    All,
    /// None: the bot joins rooms only by hand.
    None,
}

/// `gateway.platforms.matrix.options.session_scope`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MatrixSessionScope {
    /// The room's conversation, each thread its own session.
    #[default]
    Auto,
    /// One session (and workspace binding) per room: threads included.
    Room,
    /// Every top-level message starts its own thread session.
    Thread,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct WebhookOptions {
    /// The GitHub REST API `deliver = "github_comment"` posts to (GitHub
    /// Enterprise: `https://<host>/api/v3`).
    pub github_api_url: String,
    /// The `https://` origin that reaches this daemon's HTTP listener from
    /// outside (a reverse proxy or tunnel; plain `http://` is refused).
    /// Required for MCP event subscriptions, which servers deliver to
    /// `<public_url>/p/<profile>/webhook/<route>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
}

impl Default for WebhookOptions {
    fn default() -> Self {
        Self {
            github_api_url: "https://api.github.com".to_owned(),
            public_url: None,
        }
    }
}
