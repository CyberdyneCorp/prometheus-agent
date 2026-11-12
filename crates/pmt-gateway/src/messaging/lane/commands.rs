//! Slash commands from a chat (spec: messaging-gateway, Gateway slash
//! commands, quick commands and hooks; Slash command access tiers; Command
//! busy policy declared in the registry; Queued turns run in FIFO order;
//! Stop preserves the session). A line resolves against the shared
//! registry for the gateway surface (unknown commands are ordinary text);
//! the sender's tier, then the `pre_command` hooks, then the command's
//! busy policy decide whether and when it runs.

use jiff::Timestamp;
use pmt_rpc::contracts::Dispatched;
use pmt_store::gateway::routes::ResetReason;

use super::{Deferred, Lane, Origin, Pending};
use crate::messaging::adapter::Inbound;
use crate::messaging::agents::{BusyPolicy, Lookup};
use crate::messaging::auth;
use crate::messaging::hooks::PreCommand;
use crate::messaging::key::ChatType;
use crate::messaging::key::Routing;

/// A command the lane runs.
struct Call<'a> {
    seq: u64,
    routing: Routing,
    line: &'a str,
    name: String,
    args: String,
    busy: BusyPolicy,
}

/// The English refusal of `/login` outside a direct message (spec:
/// model-providers, Subscription account commands); the lane says it in
/// the profile's language (`gateway.login_in_dm`).
pub const LOGIN_IN_DM: &str =
    "Sign-in runs only in a direct message: DM the bot and send /login there.";

impl Lane {
    /// The refusal of a `reject` command mid-turn.
    pub(super) fn rejected(&self, name: &str) -> String {
        self.strings()
            .fmt("gateway.busy_rejected", &[("command", &name)])
    }

    /// An authorized event of the conversation.
    pub(super) async fn inbound(&mut self, seq: u64, routing: Routing, inbound: Inbound) {
        self.routing = routing.clone();
        self.touch();
        if let Some(data) = &inbound.callback {
            self.callback(&routing, data);
            return;
        }
        if self.answer_numbered(&routing, &inbound.text)
            || self.answer_clarify(&routing, &inbound.text)
        {
            return;
        }
        self.dispatch(seq, routing, inbound, 0).await;
    }

    async fn dispatch(&mut self, seq: u64, routing: Routing, inbound: Inbound, depth: u8) {
        let line = inbound.text.trim().to_owned();
        // Plain text is a turn unless the session waits for it (the `yes`
        // to a shown echo, the answer to a question): the host says which.
        if line.is_empty() {
            self.text(routing, &inbound).await;
            return;
        }
        let session = self.session.clone().unwrap_or_default();
        let lookup = self
            .shared
            .agents
            .lookup(&self.profile, &session, &line)
            .await;
        match lookup {
            Ok(Lookup::Text) => self.text(routing, &inbound).await,
            Ok(Lookup::Reply(text)) => self.say(&text),
            Ok(Lookup::Command { name, args, busy }) => {
                let call = Call {
                    seq,
                    routing,
                    line: &line,
                    name,
                    args,
                    busy,
                };
                if let Some(rewritten) = self.command(call).await
                    && depth == 0
                {
                    let routing = self.routing.clone();
                    let inbound = Inbound {
                        text: rewritten,
                        ..inbound
                    };
                    Box::pin(self.dispatch(seq, routing, inbound, 1)).await;
                }
            }
            Err(err) => self.say(err.message()),
        }
    }

    /// Runs a resolved command; `Some(text)` when a hook rewrote it.
    async fn command(&mut self, call: Call<'_>) -> Option<String> {
        let table = self
            .config()
            .map(|s| s.config().gateway.platform(&call.routing.platform))
            .unwrap_or_default();
        let allowed = match goal_gate(&table, &call) {
            Some(allowed) => allowed,
            None => auth::may_run(
                &table,
                call.routing.chat_type,
                &call.routing.sender_id,
                &call.name,
            ),
        };
        if !allowed {
            self.say(self.strings().get("gateway.admin_only"));
            return None;
        }
        let hooks = &self.shared.options.hooks;
        match hooks
            .pre_command(&self.profile, &call.name, &call.args)
            .await
        {
            PreCommand::Continue => {}
            PreCommand::Deny { message } => {
                self.say(&message);
                return None;
            }
            PreCommand::Handled => return None,
            PreCommand::Rewrite { text } => return Some(text),
        }
        match call.name.as_str() {
            "stop" => self.stop().await,
            "new" => {
                let reset = call.line.trim_start_matches('/').starts_with("reset");
                self.new_session(call.seq, call.routing, reset).await;
            }
            "queue" => self.queue_command(call.routing, &call.args).await,
            "approve" => self.approve(&call.routing, &call.args),
            "deny" => self.deny(&call.routing, &call.args),
            "whoami" => self.whoami(&call.routing, &table),
            "pair" => self.pair(&call.routing, &table, &call.args),
            "pause" => self.pause_command(&call.routing, &call.args),
            "sethome" => self.set_home(&call.routing),
            "policy" => self.policy_command(&call.routing, &call.args).await,
            "commands" => self.commands_command(&call.routing, &call.args),
            "busy" => self.busy_command(&call.routing, &call.args),
            "discard" => self.discard_command().await,
            "platforms" => self.say(&crate::messaging::operate::listing(
                &self.shared,
                &self.profile,
            )),
            "platform" => self.platform_command(&call.routing, &call.args).await,
            "restart" => self.restart_command(&call.routing),
            "verbose" => self.verbose_command(),
            "footer" => self.footer_command(&call.routing, &call.args),
            "voice" => self.voice_command(&call.routing, &call.args),
            "model" => self.model_command(&call.routing, &call.args).await,
            "sessions" => self.sessions_command(&call.routing, &call.args),
            "resume" => self.resume_command(&call.routing, &call.args),
            "fork" => self.branch_command(&call.routing, &call.args).await,
            "topic" => self.topic_command(&call.routing, &call.args).await,
            "background" => {
                let policy = self.session_policy(&call.routing);
                let text = crate::messaging::tasks::start(
                    &self.shared,
                    &call.routing,
                    policy,
                    &call.args,
                    self.session.as_deref(),
                )
                .await;
                self.say(&text);
            }
            "update" => self.update_command(&call.routing, &call.args).await,
            // A sign-in's device code never goes to a group or channel.
            "login" if call.routing.chat_type != ChatType::Dm => {
                self.say(self.strings().get("gateway.login_in_dm"));
            }
            "retry" => {
                self.retry_releases();
                self.registry(call).await;
            }
            _ => self.registry(call).await,
        }
        None
    }

    /// A registry command, by its busy policy while a turn runs.
    async fn registry(&mut self, call: Call<'_>) {
        if self.running.is_none() {
            self.run_now(call.routing, call.line).await;
            return;
        }
        let pending = Pending {
            origin: Origin::Command,
            text: call.line.to_owned(),
            images: Vec::new(),
            files: Vec::new(),
            routing: call.routing.clone(),
            reply_to: None,
            voice: false,
            billed: Vec::new(),
            sent_at: jiff::Timestamp::now(),
            addressed: true,
        };
        match call.busy {
            BusyPolicy::Reject => self.say(&self.rejected(&call.name)),
            BusyPolicy::Queue => self.pending.push_back(pending),
            BusyPolicy::InterruptThenDispatch => {
                self.pending.push_front(pending);
                self.interrupt().await;
            }
            BusyPolicy::Immediate => self.run_now(call.routing, call.line).await,
        }
    }

    /// Runs a registry command in the session now.
    pub(super) async fn run_now(&mut self, routing: Routing, line: &str) {
        let session = match self.ensure_session(&routing).await {
            Ok(session) => session,
            Err(err) => {
                self.say(err.message());
                return;
            }
        };
        let session_link = self.shared.server.link(&session);
        let outcome = self
            .shared
            .agents
            .run_command(&self.profile, &session, line, session_link)
            .await;
        match outcome {
            Ok(Dispatched::Done { notice }) if !notice.is_empty() => self.say(&notice),
            Ok(Dispatched::Done { .. }) => {}
            Ok(Dispatched::Send { message, .. }) => {
                let pending = Pending {
                    origin: Origin::Human,
                    text: message,
                    images: Vec::new(),
                    files: Vec::new(),
                    routing,
                    reply_to: None,
                    voice: false,
                    billed: Vec::new(),
                    sent_at: jiff::Timestamp::now(),
                    addressed: true,
                };
                if self.running.is_some() {
                    self.queue_human(pending);
                } else {
                    self.start_turn(pending).await;
                }
            }
            Ok(Dispatched::Refused { reason }) => self.say(&reason),
            Err(err) => self.say(err.message()),
        }
    }

    /// `/stop`: the running turn stops, waiting human input is dropped,
    /// the session stays.
    async fn stop(&mut self) {
        let Some(running) = self.running.as_mut() else {
            self.say(self.strings().get("gateway.nothing_to_stop"));
            return;
        };
        running.stopped = true;
        self.pending.retain(|p| p.origin == Origin::Internal);
        self.buffer = None;
        self.deferred = None;
        self.interrupt().await;
        self.say(self.strings().get("gateway.stopped"));
    }

    /// `/new` and `/reset`: a new session for the key once the running
    /// turn (if any) has stopped. Queued human input is dropped; queued
    /// internal events stay. A `/new` admitted before the previous
    /// transition completed shares its result.
    async fn new_session(&mut self, seq: u64, routing: Routing, reset: bool) {
        if seq <= self.horizon && self.session.is_some() {
            self.say(self.strings().get("gateway.new_session"));
            return;
        }
        self.pending.retain(|p| p.origin == Origin::Internal);
        self.buffer = None;
        if let Some(running) = self.running.as_mut() {
            running.stopped = true;
            self.deferred = Some(Deferred::New { reset, routing });
            self.interrupt().await;
            return;
        }
        self.renew(&routing, reset).await;
    }

    /// Replaces the key's session (the old one ends in `transition`).
    pub(super) async fn renew(&mut self, routing: &Routing, reset: bool) {
        let reason = if reset {
            ResetReason::Reset
        } else {
            ResetReason::New
        };
        let previous = self.current_session();
        match self.transition(routing, Some(reason), previous).await {
            Ok(_) => self.say(self.strings().get("gateway.new_session")),
            Err(err) => self.say(err.message()),
        }
    }

    /// `/queue <text>`: its own turn after the running one (and the
    /// entries before it), never merged.
    async fn queue_command(&mut self, routing: Routing, text: &str) {
        if text.trim().is_empty() {
            self.say(self.strings().get("gateway.queue_usage"));
            return;
        }
        let pending = Pending {
            origin: Origin::Queued,
            text: text.trim().to_owned(),
            images: Vec::new(),
            files: Vec::new(),
            routing,
            reply_to: None,
            voice: false,
            billed: Vec::new(),
            sent_at: jiff::Timestamp::now(),
            addressed: true,
        };
        if self.running.is_none() {
            self.start_turn(pending).await;
            return;
        }
        let max = self.busy().max_queued;
        let queued = self
            .pending
            .iter()
            .filter(|p| p.origin == Origin::Queued)
            .count();
        if queued >= usize::try_from(max).unwrap_or(usize::MAX) {
            self.say(&pmt_i18n::t!(
                self.strings(),
                "gateway.queue_full",
                max = max
            ));
            return;
        }
        self.pending.push_back(pending);
        let count = queued + 1;
        self.say(&pmt_i18n::t!(
            self.strings(),
            "gateway.queued",
            count = count
        ));
    }

    fn whoami(&self, routing: &Routing, table: &pmt_config::schema::PlatformConfig) {
        let strings = self.strings();
        let tier = strings.get(
            match auth::is_admin(table, routing.chat_type, &routing.sender_id) {
                Some(true) => "gateway.tier_admin",
                Some(false) => "gateway.tier_user",
                None => "gateway.tier_open",
            },
        );
        let session = self
            .session
            .as_deref()
            .unwrap_or_else(|| strings.get("gateway.no_session_yet"));
        self.say(&pmt_i18n::t!(
            strings,
            "gateway.whoami",
            sender = routing.sender_id,
            platform = routing.platform,
            profile = self.profile,
            tier = tier,
            session = session
        ));
    }

    /// `/pair approve <code>` (admins only, even where no admin list
    /// restricts other commands).
    fn pair(&self, routing: &Routing, table: &pmt_config::schema::PlatformConfig, args: &str) {
        if auth::is_admin(table, routing.chat_type, &routing.sender_id) != Some(true) {
            self.say(self.strings().get("gateway.admin_only"));
            return;
        }
        let Some(code) = args.trim().strip_prefix("approve") else {
            self.say(self.strings().get("gateway.pair_usage"));
            return;
        };
        let Some(store) = self.shared.store(&self.profile) else {
            return;
        };
        match store.approve_pairing(&routing.platform, code, Timestamp::now()) {
            Ok(pairing) => {
                let strings = self.strings();
                self.shared.deliver(
                    &self.profile,
                    &crate::messaging::paired_notice(&pairing, &strings),
                );
                self.say(&pmt_i18n::t!(
                    strings,
                    "gateway.pair_approved",
                    who = pairing.sender_name.as_deref().unwrap_or(&pairing.sender_id),
                    platform = pairing.platform
                ));
            }
            Err(err) => self.say(err.message()),
        }
    }
}

/// `/goal gate …` (spec: session-automation, Quality gates): adding a gate
/// (a shell command run at every boundary) needs a configured admin of the
/// chat's scope; listing, removing and clearing stay open to the session's
/// users. `None` for any other command.
fn goal_gate(table: &pmt_config::schema::PlatformConfig, call: &Call<'_>) -> Option<bool> {
    let mut words = call.args.split_whitespace();
    if call.name != "goal" || words.next() != Some("gate") {
        return None;
    }
    if words.next() != Some("add") {
        return Some(true);
    }
    let admin = auth::is_admin(table, call.routing.chat_type, &call.routing.sender_id);
    Some(admin == Some(true))
}
