//! The catalog index: every client-protocol name with the capability that
//! owns it (spec: workspace-architecture, Catalog index of capability-owned
//! methods and events). It is the authority for RPC names: a contract or a
//! handler for a name missing here fails at startup, and a capability uses
//! exactly these names for its operations.

/// What a catalog name is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// A client-to-daemon call.
    Method,
    /// A daemon-to-client call (approval, clarify, secret, sudo).
    ServerRequest,
    /// A daemon-to-client notification.
    Event,
}

impl Kind {
    /// The `schema/rpc.json` section of this kind.
    #[must_use]
    pub const fn section(self) -> &'static str {
        match self {
            Self::Method => "methods",
            Self::ServerRequest => "server_requests",
            Self::Event => "events",
        }
    }
}

/// One indexed name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexEntry {
    /// The wire name.
    pub name: &'static str,
    /// Method, server request or event.
    pub kind: Kind,
    /// The capability whose spec defines its params, result or payload.
    pub owner: &'static str,
    /// The protocol minor version that introduced it; a client with a
    /// lower minor receives an event only when it advertised the name.
    pub since: u16,
    /// A method that may run long: it runs as its own task on the worker
    /// pool and may answer out of order; every other request of a
    /// connection is answered in arrival order (spec: tui, Long requests
    /// do not block the connection).
    pub long: bool,
}

const fn method(name: &'static str, owner: &'static str) -> IndexEntry {
    IndexEntry {
        name,
        kind: Kind::Method,
        owner,
        since: 0,
        long: false,
    }
}

const fn long(name: &'static str, owner: &'static str) -> IndexEntry {
    IndexEntry {
        long: true,
        ..method(name, owner)
    }
}

const fn server_request(name: &'static str, owner: &'static str) -> IndexEntry {
    IndexEntry {
        name,
        kind: Kind::ServerRequest,
        owner,
        since: 0,
        long: false,
    }
}

const fn event(name: &'static str, owner: &'static str) -> IndexEntry {
    IndexEntry {
        name,
        kind: Kind::Event,
        owner,
        since: 0,
        long: false,
    }
}

const WA: &str = "workspace-architecture";

/// The index. Names whose owning capability has not landed are declared
/// here without a contract and answered `rpc.not_served`.
pub const INDEX: &[IndexEntry] = &[
    // workspace-architecture
    method("rpc.hello", WA),
    method("rpc.ping", WA),
    long("session.create", WA),
    method("session.attach", WA),
    method("session.detach", WA),
    long("session.list", WA),
    long("session.resume", WA),
    long("session.compact", WA),
    long("session.transcript", WA),
    method("session.events.since", WA),
    method("session.context_usage", WA),
    method("prompt.submit", WA),
    method("session.interrupt", WA),
    long("session.resolve", WA),
    long("command.dispatch", WA),
    long("daemon.update", WA),
    // session-storage
    long("session.fork", "session-storage"),
    // tui
    method("commands.catalog", "tui"),
    method("complete.slash", "tui"),
    long("complete.path", "tui"),
    long("model.options", "tui"),
    method("subagent.list", "tui"),
    method("subagent.subscribe", "tui"),
    method("config.get", "tui"),
    long("session.banner", "tui"),
    // tool-system
    method("approval.respond", "tool-system"),
    method("tools.list", "tool-system"),
    method("tools.call", "tool-system"),
    // observability
    method("system.health", "observability"),
    method("system.metrics", "observability"),
    // desktop-app
    method("ui.prefs.get", "desktop-app"),
    method("ui.prefs.patch", "desktop-app"),
    // messaging-gateway
    method("pairing.approve", "messaging-gateway"),
    // security-boundaries
    method("grants.list", "security-boundaries"),
    method("grants.revoke", "security-boundaries"),
    method("egress.set", "security-boundaries"),
    method("vault.list", "security-boundaries"),
    method("vault.add", "security-boundaries"),
    method("vault.remove", "security-boundaries"),
    method("vault.sources", "security-boundaries"),
    // execution-environments
    method("files.undo", "execution-environments"),
    method("files.rollback", "execution-environments"),
    method("screen.takeover", "execution-environments"),
    method("screen.handback", "execution-environments"),
    // git-worktrees
    method("worktree.new", "git-worktrees"),
    method("worktree.status", "git-worktrees"),
    method("worktree.list", "git-worktrees"),
    method("worktree.prune", "git-worktrees"),
    // lsp-diagnostics, memory, scheduled-jobs
    method("lsp.status", "lsp-diagnostics"),
    method("journey.list", "memory"),
    method("cron.blueprints.list", "scheduled-jobs"),
    method("cron.blueprints.fill", "scheduled-jobs"),
    // mixture-of-agents
    method("moa.presets.list", "mixture-of-agents"),
    method("moa.presets.put", "mixture-of-agents"),
    method("moa.presets.delete", "mixture-of-agents"),
    // voice-mode
    method("voice.client_config", "voice-mode"),
    method("wake.feed", "voice-mode"),
    // bot-mode
    method("bot.create", "bot-mode"),
    method("bot.chat.ensure", "bot-mode"),
    method("bot.roster.list", "bot-mode"),
    method("bot.meta.get", "bot-mode"),
    method("bot.meta.update", "bot-mode"),
    method("groups.capabilities", "bot-mode"),
    method("groups.log", "bot-mode"),
    method("groups.replicate", "bot-mode"),
    method("groups.replica_state", "bot-mode"),
    // agent-interop
    method("bot.relay.drain", "agent-interop"),
    method("bot.relay.deliver", "agent-interop"),
    // harness-evolution
    method("evolve.start", "harness-evolution"),
    method("evolve.status", "harness-evolution"),
    method("evolve.list", "harness-evolution"),
    method("evolve.cancel", "harness-evolution"),
    method("evolve.choose", "harness-evolution"),
    method("evolve.proposals.list", "harness-evolution"),
    method("evolve.proposals.adopt", "harness-evolution"),
    method("evolve.proposals.reject", "harness-evolution"),
    method("harness.log", "harness-evolution"),
    method("harness.diff", "harness-evolution"),
    method("harness.rollback", "harness-evolution"),
    // experience-library
    method("experience.list", "experience-library"),
    method("experience.show", "experience-library"),
    method("experience.pin", "experience-library"),
    method("experience.forget", "experience-library"),
    method("experience.restore", "experience-library"),
    method("experience.domains", "experience-library"),
    // server-to-client requests (workspace-architecture)
    server_request("approval.request", WA),
    server_request("clarify.request", WA),
    server_request("secret.request", WA),
    server_request("sudo.request", WA),
    // events
    event("usage.updated", "observability"),
    event("health.changed", "observability"),
    event("message.delivery", "messaging-gateway"),
    event("adapter.state", "messaging-gateway"),
    event("session.presence", "messaging-gateway"),
    event("request.cancel", WA),
    event("request.resolved", WA),
    event("automation.state", "session-automation"),
    event("message.reaction", "desktop-app"),
    event("achievement.unlocked", "pets"),
    event("wake.detected", "voice-mode"),
    event("local_model.context_grown", "local-models"),
    // Turn output: each trajectory event of the session, and the end of
    // each submitted turn (the stream every attached client renders).
    event("session.event", WA),
    event("turn.finished", WA),
];
