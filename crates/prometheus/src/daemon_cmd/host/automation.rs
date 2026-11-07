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
//!   someone's input already waits (a messaging conversation's lane takes
//!   it instead, with the status lines, once it delivered the answer);
//! - one loop per served profile ([`schedule`]), under that profile's own
//!   scope and store, wakes for what is due on idle sessions — a new
//!   goal's first turn, a parked goal's release, loop wakeups and
//!   heartbeats — restoring them after a restart from the stored rows. A
//!   chat-owned one runs in its conversation (the lane, which refuses it
//!   while busy: the reservation is then given back and retried later).

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
use tracing::Instrument as _;

use super::turns::Queued;
use super::{DaemonHost, Loaded, Profile};
use crate::automation::boundary::{self, Ended, Session};
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
}

/// Shows `text` on the session's clients as a `command.notice` event.
fn notice(link: &Link, text: &str) {
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
/// kind that is not set).
pub(super) fn publish(
    store: &pmt_store::Store,
    id: SessionId,
    link: &Link,
    limits: &pmt_automation::Limits,
) {
    let automation = automation::of(store, id);
    let now = automation::now_ms();
    for kind in Kind::ALL {
        link.emit::<AutomationStateChanged>(&payload(&automation, kind, id, now, limits));
    }
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
    automation: &pmt_automation::Automation,
    kind: Kind,
    id: SessionId,
    now: i64,
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
        row: only.rows(now, limits).into_iter().next(),
        count,
        limit,
        next_fire_at,
    }
}

impl DaemonHost {
    /// The boundary after a turn of `session` (before its `turn.finished`):
    /// the goal continuation to queue for a client session; a messaging
    /// session's lane takes its continuation and status lines instead.
    pub(super) async fn automation_after(
        session: &Loaded,
        end: TurnEnd,
        link: &Link,
    ) -> Option<Wakeup> {
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
        let limits = automation::limits(scope.config());
        publish(&session.factory.store, session.live.id, link, &limits);
        if session.gateway {
            let turn = next.wakeup.map(|w| automated(&w));
            *session.after.lock().expect("after poisoned") = AfterTurn {
                notices: next.notices,
                turn,
            };
            return None;
        }
        for text in &next.notices {
            notice(link, text);
        }
        if let Some(text) = next.wakeup.as_ref().and_then(|w| w.notice.as_ref()) {
            notice(link, text);
        }
        next.wakeup
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
        let (reserved, notices) = boundary::due(store, id, Host::Daemon, &limits);
        let owner = automation::load(store, id).unwrap_or_default();
        let link = self.gateway.get().map(|g| g.link(&id.to_string()));
        let name = scope.name().to_string();
        for text in &notices {
            self.tell(&name, id, link.as_ref(), owner.owner(Kind::Goal), text);
        }
        let Some(reserved) = reserved else {
            if let Some(link) = &link {
                publish(store, id, link, &limits);
            }
            return;
        };
        let kind = reserved.wakeup.kind;
        let started = if owner.owner(kind) == OWNER_GATEWAY {
            self.fire_in_chat(&name, id, &reserved.wakeup)
        } else {
            self.fire_for_clients(profile, id, &reserved.wakeup).await
        };
        if let Err(err) = started {
            tracing::debug!(session = %id, kind = kind.as_str(), "automated turn not started: {err}");
            boundary::refund(store, id, reserved);
        }
        if let Some(link) = &link {
            publish(store, id, link, &limits);
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

    /// A chat-owned wakeup runs in its conversation.
    fn fire_in_chat(&self, profile: &str, id: SessionId, wakeup: &Wakeup) -> Result<(), Error> {
        let gateway = self.gateway.get().ok_or_else(no_gateway)?;
        match gateway.automate(profile, &id.to_string(), automated(wakeup)) {
            Admission::Accepted => Ok(()),
            _ => Err(Error::new(&codes::SESSION_BUSY, "the conversation is busy")),
        }
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
                tokio::spawn(profile_loop(Arc::clone(&host), profile).instrument(span));
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
                host.fire(&profile, id)
                    .instrument(super::session_span(
                        profile.scope().name().as_str(),
                        &id.to_string(),
                    ))
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
