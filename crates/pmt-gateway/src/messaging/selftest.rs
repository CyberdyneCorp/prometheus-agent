//! `prometheus gateway selftest <platform> [--peer-agent]` (spec:
//! platform-adapters, Peer-agent routing self-test): synthetic inbound
//! events replayed through the configured adapter's admission and mention
//! gating — the same authorization, group gate and bot admission the
//! gateway applies — without network access or credentials. Each failed
//! check is classified `config`, `platform_connectivity`, `bot_identity`
//! or `routing_logic`.

use pmt_config::ProfileScope;
use pmt_config::schema::{AllowBots, PlatformConfig};
use serde::Serialize;
use tokio::time::Instant;

use super::adapter::{AdapterFactory, AdapterSetup, Descriptor, Inbound, SenderVerdict};
use super::auth::{self, Facts};
use super::gating::{self, Bursts, Gate};
use super::key::ChatType;

/// What a failed check is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// The profile's configuration decides against it.
    Config,
    /// The adapter could not say how it reaches its platform.
    PlatformConnectivity,
    /// The adapter cannot know who it is (no credential to learn its id).
    BotIdentity,
    /// The admission or gating logic refused a message it should take.
    RoutingLogic,
}

/// One check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    /// What was checked.
    pub name: String,
    /// Passed.
    pub ok: bool,
    /// The category of a failure (`None` when it passed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<Category>,
    /// Why it failed, or what passed.
    pub detail: String,
}

/// The whole self-test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    /// The platform tested.
    pub platform: String,
    /// The checks, in order.
    pub checks: Vec<Check>,
}

impl Report {
    /// Every check passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.ok)
    }

    fn pass(&mut self, name: &str, detail: impl Into<String>) {
        self.checks.push(Check {
            name: name.to_owned(),
            ok: true,
            category: None,
            detail: detail.into(),
        });
    }

    fn fail(&mut self, name: &str, category: Category, detail: impl Into<String>) {
        self.checks.push(Check {
            name: name.to_owned(),
            ok: false,
            category: Some(category),
            detail: detail.into(),
        });
    }
}

/// What to replay.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Also replay another bot mentioning this agent.
    pub peer_agent: bool,
    /// The peer bot's sender id (default `peer-agent`).
    pub peer_id: Option<String>,
}

const CHAT: &str = "selftest-chat";

/// Runs the self-test of `platform` for `scope`, building its adapter with
/// `factory` (no connection is made).
#[must_use]
pub fn run(
    scope: &ProfileScope,
    platform: &str,
    factory: Option<&AdapterFactory>,
    options: &Options,
) -> Report {
    let mut report = Report {
        platform: platform.to_owned(),
        checks: Vec::new(),
    };
    let gateway = &scope.config().gateway;
    let table = gateway.platform(platform);
    if !gateway.platforms.get(platform).is_some_and(|t| t.enabled) {
        report.fail(
            "platform enabled",
            Category::Config,
            format!("gateway.platforms.{platform}.enabled is not true"),
        );
        return report;
    }
    report.pass("platform enabled", format!("gateway.platforms.{platform}"));
    let Some(descriptor) = adapter(scope, platform, &table, factory, &mut report) else {
        return report;
    };
    identity(scope, platform, &table, &mut report);
    let Some(person) = authorized_person(&table) else {
        report.fail(
            "sender authorization",
            Category::Config,
            format!(
                "nobody is authorized without pairing: list senders in gateway.platforms.{platform}.allow_users or set allow_all"
            ),
        );
        return report;
    };
    let groups = gateway.groups.for_platform(platform);
    let dm = Inbound::text(ChatType::Dm, &person, &person, "selftest");
    routed(
        &mut report,
        "direct message",
        &descriptor,
        &table,
        &dm,
        &groups,
    );
    let mut mention = Inbound::text(ChatType::Group, CHAT, &person, "@agent selftest");
    mention.addressing.mentioned = true;
    routed(
        &mut report,
        "group mention",
        &descriptor,
        &table,
        &mention,
        &groups,
    );
    if options.peer_agent {
        peer(&mut report, platform, gateway, &descriptor, &table, options);
    }
    report
}

/// Builds the adapter (no connection): its descriptor and how it reaches
/// the platform.
fn adapter(
    scope: &ProfileScope,
    platform: &str,
    table: &PlatformConfig,
    factory: Option<&AdapterFactory>,
    report: &mut Report,
) -> Option<Descriptor> {
    let Some(factory) = factory else {
        report.fail(
            "adapter",
            Category::Config,
            format!("no adapter of this build or of an enabled plugin provides '{platform}'"),
        );
        return None;
    };
    let setup = AdapterSetup {
        scope: scope.clone(),
        platform: platform.to_owned(),
        config: table.clone(),
    };
    let adapter = match factory(&setup) {
        Ok(adapter) => adapter,
        Err(err) => {
            report.fail("adapter", Category::Config, err.message().to_owned());
            return None;
        }
    };
    report.pass("adapter", "built from the configuration");
    match adapter.reach() {
        Ok(_) => report.pass(
            "platform reach",
            "endpoints resolved from the configuration",
        ),
        Err(why) => report.fail("platform reach", Category::PlatformConnectivity, why),
    }
    Some(adapter.describe())
}

/// The bot learns its own id with its credential: a named credential that
/// is missing leaves it unable to recognise mentions of itself.
fn identity(scope: &ProfileScope, platform: &str, table: &PlatformConfig, report: &mut Report) {
    let Some(secret) = &table.token_secret else {
        report.pass("bot identity", "the adapter's default credential");
        return;
    };
    match scope.secrets().store().get(&secret.0) {
        Ok(Some(_)) => report.pass("bot identity", format!("credential {} present", secret.0)),
        _ => report.fail(
            "bot identity",
            Category::BotIdentity,
            format!(
                "gateway.platforms.{platform}.token_secret names {}, which the secrets store does not hold: the adapter cannot learn its own id",
                secret.0
            ),
        ),
    }
}

/// A sender the configuration authorizes without pairing.
fn authorized_person(table: &PlatformConfig) -> Option<String> {
    table
        .allow_users
        .iter()
        .find(|u| *u != "*")
        .cloned()
        .or_else(|| table.allow_all.then(|| "selftest-user".to_owned()))
}

fn authorized(descriptor: &Descriptor, table: &PlatformConfig, inbound: &Inbound) -> bool {
    let facts = Facts {
        descriptor,
        config: table,
        inbound,
        paired: false,
        adapter: SenderVerdict::Unknown,
    };
    auth::authorize(&facts).is_some()
}

fn routed(
    report: &mut Report,
    name: &str,
    descriptor: &Descriptor,
    table: &PlatformConfig,
    inbound: &Inbound,
    groups: &pmt_config::schema::EffectiveGroups,
) {
    if !authorized(descriptor, table, inbound) {
        report.fail(
            name,
            Category::RoutingLogic,
            "the authorized sender was refused",
        );
        return;
    }
    match gating::group_gate(groups, inbound) {
        Gate::Turn { .. } => report.pass(name, "starts a turn"),
        Gate::Observe => report.fail(
            name,
            Category::RoutingLogic,
            "kept as context without a turn (gateway.groups.observe_unmentioned)",
        ),
        Gate::Ignore => report.fail(
            name,
            Category::RoutingLogic,
            "ignored by group gating (gateway.groups.ignored_chats)",
        ),
    }
}

/// Another bot mentioning this agent in a group.
fn peer(
    report: &mut Report,
    platform: &str,
    gateway: &pmt_config::schema::GatewayConfig,
    descriptor: &Descriptor,
    table: &PlatformConfig,
    options: &Options,
) {
    const NAME: &str = "peer agent mention";
    let bots = gateway.bots.for_platform(platform);
    if bots.allow_bots == AllowBots::None {
        report.fail(
            NAME,
            Category::Config,
            format!(
                "gateway.bots.allow_bots is none for {platform}: messages of other bots are ignored (set it to mentions or all, globally or under gateway.bots.platforms.{platform})"
            ),
        );
        return;
    }
    let id = options.peer_id.as_deref().unwrap_or("peer-agent");
    let mut message = Inbound::text(ChatType::Group, CHAT, id, "@agent hand-off");
    message.is_bot = true;
    message.addressing.mentioned = true;
    if !authorized(descriptor, table, &message) {
        report.fail(
            NAME,
            Category::Config,
            format!(
                "the peer bot '{id}' is not authorized: add it to gateway.platforms.{platform}.allow_users (or name it with --peer-id)"
            ),
        );
        return;
    }
    let key = format!("selftest\u{1f}{platform}\u{1f}{CHAT}\u{1f}{id}");
    if !Bursts::default().admit(&bots, &key, &message, Instant::now()) {
        report.fail(
            NAME,
            Category::RoutingLogic,
            "the bot admission refused a mention",
        );
        return;
    }
    let groups = gateway.groups.for_platform(platform);
    routed(report, NAME, descriptor, table, &message, &groups);
}
