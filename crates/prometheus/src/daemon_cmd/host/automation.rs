//! Session automation in the daemon (spec: session-automation, Idle-
//! boundary arbitration; Heartbeat ownership, recovery and silence on the
//! gateway; Automation status on every surface).
//!
//! The daemon drives the automation of every session it serves, whoever
//! is attached:
//!
//! - when a turn ends ([`DaemonHost::automation_after`], inside the
//!   session's turn task, before `turn.finished`), the loop and goal
//!   boundaries run; a goal continuation is queued behind the turn unless
//!   someone's input already waits. A messaging conversation's lane takes
//!   the continuation of its own turn, with the status lines, once it
//!   delivered the answer; after a turn an attached client submitted to a
//!   conversation's session, the chat hears the status lines at once and
//!   its lane gets the continuation when the session is idle;
//! - one loop per served profile ([`schedule`]), under that profile's own
//!   scope and store, wakes for what is due on idle sessions — a new
//!   goal's first turn, a parked goal's release, loop wakeups and
//!   heartbeats — restoring them after a restart from the stored rows. A
//!   chat-owned one runs in its conversation (the lane, which refuses it
//!   while busy, also after admitting it: the reservation is then given
//!   back and retried later).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use pmt_agent::turn::TurnInput;
use pmt_automation::Kind;
use pmt_automation::arbiter::Wakeup;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_gateway::Link;
use pmt_gateway::messaging::adapter::Admission;
use pmt_gateway::messaging::automation::{AfterTurn, Automated};
use pmt_rpc::contracts::{AutomationStateChanged, AutomationStatePayload, COMMAND_NOTICE};
use pmt_rpc::contracts::{DispatchParams, Dispatched, SessionEvent};
use tokio::sync::oneshot;
use tracing::Instrument as _;

use super::turns::Queued;
use super::{DaemonHost, Loaded, Profile};
use crate::automation::boundary::{self, Ended, Reserved, Session};
use crate::automation::{self, Host, OWNER_GATEWAY};

/// How often new served profiles are looked for.
const PROFILES_EVERY: Duration = Duration::from_secs(30);
/// The longest a profile's loop sleeps between looks.
const LOOK_EVERY: Duration = Duration::from_secs(30);
/// The shortest (a fire that keeps being refused is retried this often).
const LOOK_AT_LEAST: Duration = Duration::from_millis(500);

/// How a turn of a loaded session ended, for its boundary.
pub(super) struct TurnEnd {
    pub(super) origin: Option<Kind>,
    pub(super) reply: String,
    pub(super) interrupted: bool,
    /// The conversation's lane submitted the turn.
    pub(super) from_lane: bool,
}

/// The goal continuation a turn's boundary asks for.
pub(super) enum Follow {
    /// Queued as the session's next turn.
    Here(Wakeup),
    /// Run by the conversation's lane once the session is idle (the turn
    /// came from an attached client, not from the chat).
    InChat(Wakeup),
}

/// Shows `text` on the session's clients as a `command.notice` event.
pub(super) fn notice(link: &Link, text: &str) {
    link.emit::<SessionEvent>(&serde_json::json!({
        "type": COMMAND_NOTICE,
        "ts": jiff::Timestamp::now().to_string(),
        "turn_id": null,
        "span_id": null,
        "parent_span_id": null,
        "payload": {"text": text},
    }));
}

/// Publishes one `automation.state` event per kind of `id` (`none` for a
/// kind that is not set) of a session of `scope`, rows in its language.
pub(super) fn publish(
    scope: &pmt_config::ProfileScope,
    store: &pmt_store::Store,
    id: SessionId,
    link: &Link,
) {
    let limits = automation::limits(scope.config());
    let strings = pmt_config::language::strings(scope.dir(), pmt_i18n::Surface::Core);
    let automation = automation::of(store, id);
    let now = automation::now_ms();
    for kind in Kind::ALL {
        let state = State { kind, id, now };
        link.emit::<AutomationStateChanged>(&payload(&*strings, &automation, state, &limits));
    }
}

/// Which kind of which session, when.
struct State {
    kind: Kind,
    id: SessionId,
    now: i64,
}

fn at(ms: i64) -> Option<String> {
    jiff::Timestamp::from_millisecond(ms)
        .ok()
        .map(|t| t.to_string())
}

/// `(status, count, limit, next_fire_at)` of `kind`; `None` when unset.
fn facts(
    automation: &pmt_automation::Automation,
    kind: Kind,
    limits: &pmt_automation::Limits,
) -> Option<(String, u32, Option<u32>, Option<String>)> {
    match kind {
        Kind::Goal => automation.goal.as_ref().map(|goal| {
            let status = if goal.barrier.is_some() {
                "parked"
            } else {
                goal.status.as_str()
            };
            let next = goal.barrier.as_ref().and_then(|b| at(b.until_ms()));
            (
                status.to_owned(),
                goal.turns_used,
                Some(limits.goal_max_turns),
                next,
            )
        }),
        Kind::Loop => automation.looping.as_ref().map(|looping| {
            let status = looping.status.as_str().to_owned();
            (status, looping.fired, None, at(looping.next_fire_ms))
        }),
        Kind::Heartbeat => automation.heartbeat.as_ref().map(|heartbeat| {
            let status = heartbeat.status.as_str().to_owned();
            (status, heartbeat.fired, None, at(heartbeat.next_fire_ms))
        }),
    }
}

fn payload(
    text: &dyn pmt_core::text::Catalog,
    automation: &pmt_automation::Automation,
    State { kind, id, now }: State,
    limits: &pmt_automation::Limits,
) -> AutomationStatePayload {
    let only = pmt_automation::Automation {
        goal: automation.goal.clone().filter(|_| kind == Kind::Goal),
        looping: automation.looping.clone().filter(|_| kind == Kind::Loop),
        heartbeat: automation
            .heartbeat
            .clone()
            .filter(|_| kind == Kind::Heartbeat),
    };
    let (status, count, limit, next_fire_at) =
        facts(automation, kind, limits).unwrap_or_else(|| ("none".to_owned(), 0, None, None));
    AutomationStatePayload {
        session_id: id.to_string(),
        kind: kind.as_str().to_owned(),
        status,
        row: only.rows(text, now, limits).into_iter().next(),
        count,
        limit,
        next_fire_at,
    }
}

impl DaemonHost {
    /// The boundary after a turn of `session` (before its `turn.finished`):
    /// the goal continuation to follow it. A turn the conversation's lane
    /// submitted leaves its continuation and status lines for the lane.
    pub(super) async fn automation_after(
        session: &Loaded,
        end: TurnEnd,
        link: &Link,
    ) -> Option<Follow> {
        let boundary = Session {
            factory: &session.factory,
            live: &session.live,
            host: Host::Daemon,
        };
        let ended = Ended {
            origin: end.origin,
            reply: &end.reply,
            interrupted: end.interrupted,
        };
        let next = boundary::after_turn(&boundary, ended).await;
        let scope = session.live.config.get();
        publish(&scope, &session.factory.store, session.live.id, link);
        if session.chat.is_some() && end.from_lane {
            let turn = next.wakeup.map(|w| automated(&w));
            *session.after.lock().expect("after poisoned") = AfterTurn {
                notices: next.notices,
                turn,
            };
            return None;
        }
        for text in &next.notices {
            notice(link, text);
            Self::say_in_chat(session, text);
        }
        if let Some(text) = next.wakeup.as_ref().and_then(|w| w.notice.as_ref()) {
            notice(link, text);
        }
        let follow = if session.chat.is_some() {
            Follow::InChat
        } else {
            Follow::Here
        };
        next.wakeup.map(follow)
    }

    /// Says `text` in the chat of a conversation's `session`.
    fn say_in_chat(session: &Loaded, text: &str) {
        let Some(gateway) = session.chat.as_ref().and_then(|cell| cell.get()) else {
            return;
        };
        let profile = session.live.config.get().name().to_string();
        gateway.say_in_session(&profile, &session.live.id.to_string(), text);
    }

    /// Hands the goal continuation of a client's turn to the idle
    /// conversation's lane (it says the continuation's status line). A
    /// busy chat refuses it: nothing was reserved, and the judge looks
    /// again after the chat's own turn.
    pub(super) fn continue_in_chat(session: &Loaded, wakeup: &Wakeup) {
        let Some(gateway) = session.chat.as_ref().and_then(|cell| cell.get()) else {
            return;
        };
        let profile = session.live.config.get().name().to_string();
        let id = session.live.id.to_string();
        if gateway.automate(&profile, &id, automated(wakeup)) != Admission::Accepted {
            tracing::debug!(
                session = id,
                "goal continuation not started: the chat is busy"
            );
        }
    }

    /// What a messaging session's lane runs after its turn (taken once).
    pub(super) async fn take_after(&self, profile: &str, session_id: &str) -> AfterTurn {
        let (Ok(profile), Ok(id)) = (self.profile(profile), super::session_id(session_id)) else {
            return AfterTurn::default();
        };
        let loaded = profile.loaded.lock().await.get(&id).cloned();
        loaded.map_or_else(AfterTurn::default, |session| {
            std::mem::take(&mut *session.after.lock().expect("after poisoned"))
        })
    }

    /// Fires what is due on the idle session `id` of `profile`.
    async fn fire(&self, profile: &Arc<Profile>, id: SessionId) {
        let busy = profile
            .loaded
            .lock()
            .await
            .get(&id)
            .is_some_and(|s| s.busy.load(Ordering::Acquire));
        if busy {
            // Its turn's end runs the boundary; a timer waits for idle.
            return;
        }
        let scope = profile.scope();
        let limits = automation::limits(scope.config());
        let store = &profile.store;
        let strings = pmt_config::language::strings(scope.dir(), pmt_i18n::Surface::Core);
        let (reserved, notices) = boundary::due(&*strings, store, id, Host::Daemon, &limits);
        let owner = automation::load(store, id).unwrap_or_default();
        let link = self.gateway.get().map(|g| g.link(&id.to_string()));
        let name = scope.name().to_string();
        for text in &notices {
            self.tell(&name, id, link.as_ref(), owner.owner(Kind::Goal), text);
        }
        let Some(reserved) = reserved else {
            if let Some(link) = &link {
                publish(&scope, store, id, link);
            }
            return;
        };
        let kind = reserved.wakeup.kind;
        if owner.owner(kind) == OWNER_GATEWAY {
            self.fire_in_chat(profile, id, reserved);
        } else if let Err(err) = self.fire_for_clients(profile, id, &reserved.wakeup).await {
            tracing::debug!(session = %id, kind = kind.as_str(), "automated turn not started: {err}");
            boundary::refund(store, id, reserved);
        }
        if let Some(link) = &link {
            publish(&scope, store, id, link);
        }
    }

    /// Says a status line where the session's owner reads it.
    fn tell(&self, profile: &str, id: SessionId, link: Option<&Link>, owner: &str, text: &str) {
        if let Some(link) = link {
            notice(link, text);
        }
        if owner == OWNER_GATEWAY
            && let Some(gateway) = self.gateway.get()
        {
            gateway.say_in_session(profile, &id.to_string(), text);
        }
    }

    /// A chat-owned wakeup runs in its conversation. Its reservation is
    /// given back when the chat refuses it, at once or when its lane drops
    /// it (a person's input reached the chat after the admission).
    fn fire_in_chat(&self, profile: &Arc<Profile>, id: SessionId, reserved: Reserved) {
        let store = &profile.store;
        let Some(gateway) = self.gateway.get() else {
            tracing::debug!(session = %id, "automated turn not started: no gateway yet");
            boundary::refund(store, id, reserved);
            return;
        };
        let name = profile.scope().name().to_string();
        let (started, lane) = oneshot::channel();
        let wakeup = Automated {
            started: Some(started),
            ..automated(&reserved.wakeup)
        };
        if gateway.automate(&name, &id.to_string(), wakeup) != Admission::Accepted {
            tracing::debug!(session = %id, "automated turn not started: the chat is busy");
            boundary::refund(store, id, reserved);
            return;
        }
        let profile = Arc::clone(profile);
        let link = gateway.link(&id.to_string());
        pmt_core::spawn::spawn(
            async move {
                // A lane that went away without answering started nothing.
                if lane.await.unwrap_or(false) {
                    return;
                }
                tracing::debug!(session = %id, "automated turn dropped by the chat; refunded");
                boundary::refund(&profile.store, id, reserved);
                publish(&profile.scope(), &profile.store, id, &link);
                profile.automation_wake.notify_one();
            }
            .in_current_span(),
        );
    }

    /// A client session's wakeup runs as a turn its attached clients see
    /// (they need not be attached: the replies stay in the session).
    async fn fire_for_clients(
        &self,
        profile: &Arc<Profile>,
        id: SessionId,
        wakeup: &Wakeup,
    ) -> Result<(), Error> {
        pmt_core::pause::check(profile.scope().home())?;
        Self::check_budget(profile, id)?;
        let link = self
            .gateway
            .get()
            .ok_or_else(no_gateway)?
            .link(&id.to_string());
        if let Some(text) = &wakeup.notice {
            notice(&link, text);
        }
        if wakeup.prompt.starts_with('/') {
            return self.loop_command(profile, id, wakeup, link).await;
        }
        let session = self.load(profile, id, &link).await?;
        let input = TurnInput {
            origin: Some(wakeup.kind.as_str().to_owned()),
            ..TurnInput::text(wakeup.prompt.clone())
        };
        self.start_owned(profile, id, &session, input, link)
    }

    /// Starts `input` on the idle `session` (refused while it is busy).
    fn start_owned(
        &self,
        profile: &Arc<Profile>,
        id: SessionId,
        session: &Arc<Loaded>,
        input: TurnInput,
        link: Link,
    ) -> Result<(), Error> {
        if session.busy.swap(true, Ordering::AcqRel) {
            return Err(super::busy(id));
        }
        let job = Queued {
            input,
            turn_id: pmt_agent::turn::TurnId::new().to_string(),
            run_budget: None,
            link,
            from_lane: false,
            resume: None,
        };
        Self::start_with(
            Arc::clone(&self.turns),
            self.slots.clone(),
            Arc::clone(profile),
            id,
            Arc::clone(session),
            job,
        )
    }

    /// A loop whose wakeup is a slash command runs it; a command that
    /// sends a message starts it as the wakeup's turn.
    async fn loop_command(
        &self,
        profile: &Arc<Profile>,
        id: SessionId,
        wakeup: &Wakeup,
        link: Link,
    ) -> Result<(), Error> {
        let (name, args) = crate::slash::split_line(&wakeup.prompt);
        let params = DispatchParams {
            session_id: id.to_string(),
            name,
            args,
        };
        match self.dispatch_command(profile, params, link.clone()).await? {
            Dispatched::Send { message, .. } => {
                let session = self.load(profile, id, &link).await?;
                let input = TurnInput {
                    origin: Some(Kind::Loop.as_str().to_owned()),
                    ..TurnInput::text(message)
                };
                self.start_owned(profile, id, &session, input, link)
            }
            Dispatched::Done { notice: text } | Dispatched::Refused { reason: text } => {
                if !text.is_empty() {
                    notice(&link, &text);
                }
                Ok(())
            }
        }
    }

    /// Whether the session's automation keeps it from the idle reaper (an
    /// active loop or heartbeat).
    pub(super) fn automation_keeps(profile: &Profile, id: SessionId) -> bool {
        automation::keeps_alive(&profile.store, id)
    }
}

fn no_gateway() -> Error {
    Error::new(
        &codes::SESSION_BUSY,
        "the daemon's gateway is not running yet",
    )
}

/// A wakeup as the gateway runs it.
fn automated(wakeup: &Wakeup) -> Automated {
    Automated {
        origin: wakeup.kind.as_str().to_owned(),
        text: wakeup.prompt.clone(),
        notice: wakeup.notice.clone(),
        started: None,
    }
}

/// Starts a loop for every served profile (and for profiles a reload
/// serves later) until the daemon stops.
pub(in crate::daemon_cmd) async fn schedule(host: Arc<DaemonHost>) {
    let mut started = BTreeSet::new();
    loop {
        for (name, profile) in host.all_profiles() {
            if started.insert(name.clone()) {
                let span =
                    tracing::info_span!("automation", component = "agent", profile = name.as_str());
                pmt_core::spawn::spawn(profile_loop(Arc::clone(&host), profile).instrument(span));
            }
        }
        tokio::time::sleep(PROFILES_EVERY).await;
    }
}

/// One profile's automation timer: fires what is due on its sessions,
/// then sleeps until the earliest next check (a command wakes it early).
async fn profile_loop(host: Arc<DaemonHost>, profile: Arc<Profile>) {
    loop {
        let sessions: BTreeSet<SessionId> = profile
            .store
            .all_automation()
            .inspect_err(|err| tracing::warn!(error = %err, "automation rows not read; retrying"))
            .unwrap_or_default()
            .into_iter()
            .map(|row| row.session_id)
            .collect();
        let mut earliest: Option<i64> = None;
        for id in sessions {
            let Some(at) = boundary::next_check(&profile.store, id, Host::Daemon) else {
                continue;
            };
            if at <= automation::now_ms() {
                Box::pin(host.fire(&profile, id).instrument(super::session_span(
                    profile.scope().name().as_str(),
                    &id.to_string(),
                )))
                .await;
                earliest = Some(earliest.map_or(at, |e| e.min(at)));
                continue;
            }
            earliest = Some(earliest.map_or(at, |e| e.min(at)));
        }
        let wait = earliest.map_or(LOOK_EVERY, |at| {
            Duration::from_millis(u64::try_from(at - automation::now_ms()).unwrap_or(0))
        });
        tokio::select! {
            () = tokio::time::sleep(wait.clamp(LOOK_AT_LEAST, LOOK_EVERY)) => {}
            () = profile.automation_wake.notified() => {}
        }
    }
}
