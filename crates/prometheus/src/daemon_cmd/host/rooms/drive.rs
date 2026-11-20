//! The durable room driver (spec: bot-mode, Group chats; Durable room
//! driver; Hosted rooms across gateways). A room whose log ends with a
//! user message not yet settled has a discussion to run; the daemon that
//! takes the room's lease in `state.db` runs it, so closing every client
//! never stops it and a restarted daemon picks it up from the log.
//!
//! One user message drives up to `hosted_rooms.discussion.max_rounds`
//! serial rounds and `max_messages` member messages; the members it
//! mentions (all when none) take turns unless held, each replying or
//! passing, and a fully silent round settles the room. A member turn runs
//! in the member's own room session (`Group: <room> · main`), on its own
//! machine for a member of another gateway, under that gateway's room
//! grant. A turn is kept while it shows progress, up to 10800 seconds,
//! and expires after 180 seconds without any.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use pmt_core::error::Error;
use pmt_gateway::Author;
use pmt_rpc::contracts::SubmitParams;
use pmt_store::Store;
use pmt_store::rooms::{MemberChange, MemberRow, RoomRow};
use serde_json::json;

use crate::bot_host::reasons;
use crate::bot_host::rooms::{Said, addressed, context, passes, wants_user};
use crate::daemon_cmd::host::{DaemonHost, Profile};

/// A member turn is kept at most this long.
const TURN_CAP: Duration = Duration::from_hours(3);
/// A member turn without progress for this long expires.
const NO_PROGRESS: Duration = Duration::from_mins(3);
/// How long the driver lease lasts between renewals.
const LEASE_MS: i64 = 60_000;

/// The rooms this process drives now, with their stop flags.
static DRIVING: LazyLock<Mutex<HashMap<String, Arc<AtomicBool>>>> = LazyLock::new(Mutex::default);

/// Whether this process drives room `id` now.
pub(super) fn running(id: &str) -> bool {
    DRIVING.lock().expect("driving poisoned").contains_key(id)
}

/// Asks the discussion of room `id` to stop; `true` when one ran here.
pub(super) fn cancel(id: &str) -> bool {
    DRIVING
        .lock()
        .expect("driving poisoned")
        .get(id)
        .inspect(|stop| stop.store(true, Ordering::Release))
        .is_some()
}

/// Whether room `id`'s log ends with a user message not yet settled.
pub(super) fn pending(store: &Store, id: &str) -> bool {
    !running(id) && unsettled(store, id).is_some()
}

/// The unsettled user message of room `id`.
fn unsettled(store: &Store, id: &str) -> Option<u64> {
    let log = store.room_log(id, 0, u32::MAX).ok()?;
    let user = log
        .iter()
        .rev()
        .find(|e| e.kind == "message" && e.author.as_deref() == Some("user"))?
        .seq;
    let settled = log
        .iter()
        .any(|e| e.seq > user && matches!(e.kind.as_str(), "settled" | "stopped"));
    (!settled).then_some(user)
}

/// The session tag of a member's room session.
fn tag(room: &str) -> String {
    format!("room-{room}")
}

/// The member's room session, if it has one.
pub(super) fn member_session_id(store: &Store, room: &str) -> Result<Option<String>, Error> {
    let all = pmt_store::ListOptions {
        all: true,
        ..pmt_store::ListOptions::default()
    };
    Ok(store
        .list_sessions(&all)?
        .into_iter()
        .find(|s| s.source_tag.as_deref() == Some(tag(room).as_str()))
        .map(|s| s.id.to_string()))
}

/// The member's room session, created as `Group: <room> · main`.
pub(super) fn member_session(store: &Store, room: &RoomRow) -> Result<String, Error> {
    let mut new = pmt_store::NewSession::new(pmt_store::Source::Desktop);
    new.source_tag = Some(tag(&room.id));
    new.hidden = true;
    let (session, created) = store.ensure_tagged_session(&new)?;
    if created {
        let title = format!("Group: {} · main", room.name);
        store.set_title(session.id, Some(&title), pmt_store::TitleSource::User)?;
    }
    Ok(session.id.to_string())
}

/// Starts room `id`'s discussion when this process gets its lease.
pub(super) fn start(host: &Arc<DaemonHost>, store: &Arc<Store>, id: &str) {
    let holder = format!("{}:{}", std::process::id(), host.launch_profile);
    if !store.room_lease(id, &holder, LEASE_MS).unwrap_or(false) {
        return;
    }
    let stop = Arc::new(AtomicBool::new(false));
    DRIVING
        .lock()
        .expect("driving poisoned")
        .insert(id.to_owned(), Arc::clone(&stop));
    let (host, store, id) = (Arc::clone(host), Arc::clone(store), id.to_owned());
    pmt_core::spawn::spawn(async move {
        let driver = Driver {
            host: &host,
            store: &store,
            id: &id,
            holder: &holder,
            stop: &stop,
        };
        if let Err(err) = Box::pin(driver.discuss()).await {
            tracing::warn!(room = id, "room discussion: {}", err.message());
        }
        let _ = store.room_release(&id, &holder);
        DRIVING.lock().expect("driving poisoned").remove(&id);
    });
}

/// How one member turn ended.
enum Turn {
    Said(String),
    Passed,
    Failed(String),
}

struct Driver<'a> {
    host: &'a Arc<DaemonHost>,
    store: &'a Arc<Store>,
    id: &'a str,
    holder: &'a str,
    stop: &'a AtomicBool,
}

impl Driver<'_> {
    async fn discuss(&self) -> Result<(), Error> {
        let Some(start) = unsettled(self.store, self.id) else {
            return Ok(());
        };
        let bounds = self
            .host
            .profile(&self.host.launch_profile)?
            .scope()
            .config()
            .hosted_rooms
            .discussion
            .clone();
        let log = self.store.room_log(self.id, start, u32::MAX)?;
        let mut rounds =
            u32::try_from(log.iter().filter(|e| e.kind == "round").count()).unwrap_or(0);
        let mut messages =
            u32::try_from(log.iter().filter(|e| e.kind == "message").count()).unwrap_or(0);
        let user = self.store.room_log(self.id, start - 1, 1)?;
        let text = user
            .first()
            .and_then(|e| e.text.clone())
            .unwrap_or_default();
        while rounds < bounds.max_rounds && messages < bounds.max_messages {
            rounds += 1;
            let round = json!({"round": rounds}).to_string();
            self.store
                .room_append(self.id, "round", None, (None, Some(&round)))?;
            let spoke = Box::pin(self.round(&text, &mut messages, bounds.max_messages)).await?;
            if self.stop.load(Ordering::Acquire) {
                self.store
                    .room_append(self.id, "stopped", None, (None, None))?;
                return Ok(());
            }
            if !spoke {
                break;
            }
        }
        let summary = json!({"rounds": rounds, "messages": messages}).to_string();
        self.store
            .room_append(self.id, "settled", None, (None, Some(&summary)))?;
        Ok(())
    }

    /// One round; whether any member spoke.
    async fn round(&self, text: &str, messages: &mut u32, max: u32) -> Result<bool, Error> {
        let room = self.store.room(self.id)?;
        let names: Vec<String> = room.members.iter().map(|m| m.member.clone()).collect();
        let wanted = addressed(text, &names, room.detect_stop_directives);
        let mut spoke = false;
        for member in &room.members {
            let addressed_here =
                wanted.members.is_empty() || wanted.members.contains(&member.member);
            if !addressed_here || *messages >= max || self.stop.load(Ordering::Acquire) {
                continue;
            }
            // A stop directive may hold it while the round runs.
            let current = self.store.room(self.id)?;
            if current
                .members
                .iter()
                .any(|m| m.member == member.member && m.held)
            {
                continue;
            }
            self.store.room_lease(self.id, self.holder, LEASE_MS)?;
            if Box::pin(self.member_turn(&current, member)).await? {
                spoke = true;
                *messages += 1;
            }
        }
        Ok(spoke)
    }

    /// One member's turn, logged; whether it said something.
    async fn member_turn(&self, room: &RoomRow, member: &MemberRow) -> Result<bool, Error> {
        let seen = self.store.room_last_seq(self.id)?;
        let said: Vec<Said> = self
            .store
            .room_log(self.id, member.seen_seq, u32::MAX)?
            .into_iter()
            .filter(|e| e.kind == "message")
            .map(|e| Said {
                author: e.author.unwrap_or_default(),
                text: e.text.unwrap_or_default(),
            })
            .collect();
        let prompt = context(&room.name, &member.member, &said);
        self.store
            .room_append(self.id, "turn.started", Some(&member.member), (None, None))?;
        let turn = match &member.gateway {
            None => match self.host.profile(&member.profile) {
                Ok(profile) => read(local_turn(self.host, &profile, room, &prompt).await),
                Err(err) => Turn::Failed(format!("missing_config: {}", err.message())),
            },
            Some(peer) => Box::pin(self.remote_turn(room, member, peer, &prompt)).await,
        };
        let change = MemberChange {
            seen_seq: Some(seen),
            ..MemberChange::default()
        };
        self.store.room_member(self.id, &member.member, &change)?;
        self.settle(member, &turn)
    }

    /// Logs how a member turn ended; whether it said something.
    fn settle(&self, member: &MemberRow, turn: &Turn) -> Result<bool, Error> {
        let name = Some(member.member.as_str());
        let (outcome, said) = match turn {
            Turn::Said(text) => {
                self.store
                    .room_append(self.id, "message", name, (Some(text), None))?;
                if wants_user(text) {
                    self.store.room_needs_you(self.id, true)?;
                }
                (json!({"outcome": "replied"}), true)
            }
            Turn::Passed => {
                self.store
                    .room_append(self.id, "pass", name, (None, None))?;
                (json!({"outcome": "passed"}), false)
            }
            Turn::Failed(failure) => {
                let redactor = self
                    .host
                    .profile(&member.profile)
                    .ok()
                    .and_then(|p| p.scope().secrets().redactor().ok())
                    .unwrap_or_default();
                let reason = reasons::of_failure(failure);
                let error = reasons::first_line(failure, &redactor);
                (
                    json!({"outcome": "failed", "reason": reason, "error": error}),
                    false,
                )
            }
        };
        let data = outcome.to_string();
        self.store
            .room_append(self.id, "turn.settled", name, (None, Some(&data)))?;
        Ok(said)
    }

    /// A member of another gateway: its authority run there, never driven
    /// again while a previous one still runs.
    async fn remote_turn(
        &self,
        room: &RoomRow,
        member: &MemberRow,
        peer: &str,
        prompt: &str,
    ) -> Turn {
        let launch = match self.host.fresh_scope(&self.host.launch_profile) {
            Ok(launch) => launch,
            Err(err) => return Turn::Failed(format!("missing_config: {}", err.message())),
        };
        let client = match crate::daemon_cmd::peers::client::Peer::of(&launch, peer) {
            Ok(client) => client,
            Err(err) => return Turn::Failed(format!("missing_config: {}", err.message())),
        };
        let position = room
            .members
            .iter()
            .position(|m| m.member == member.member)
            .unwrap_or(0);
        let grant = launch
            .secrets()
            .resolve(&super::authority::grant_secret(&room.id, position))
            .ok();
        let run = if let Some(run) = &member.pending_run {
            run.clone()
        } else {
            let run = format!("run_{}", ulid::Ulid::generate().to_string().to_lowercase());
            let body = json!({"member": member.profile, "text": prompt, "run_id": run, "room_name": room.name});
            let started = client
                .room_turn(
                    &room.id,
                    &body,
                    grant.as_ref().map(|g| g.expose().to_owned()),
                )
                .await;
            if let Err(refused) = started {
                return self.unreachable(member, &refused);
            }
            self.pending_run(member, Some(run.clone()));
            run
        };
        let ended = poll_run(&client, &room.id, &run).await;
        if !matches!(ended, Turn::Failed(ref f) if f.starts_with("runtime_offline")) {
            self.pending_run(member, None);
        }
        ended
    }

    fn pending_run(&self, member: &MemberRow, run: Option<String>) {
        let change = MemberChange {
            pending_run: Some(run),
            ..MemberChange::default()
        };
        let _ = self.store.room_member(self.id, &member.member, &change);
    }

    /// A member gateway that refused the authority: unreachable.
    fn unreachable(
        &self,
        member: &MemberRow,
        refused: &crate::daemon_cmd::peers::client::PeerError,
    ) -> Turn {
        let reason = match refused {
            crate::daemon_cmd::peers::client::PeerError::KeyRejected => "missing_config",
            crate::daemon_cmd::peers::client::PeerError::Unreachable(_) => "runtime_offline",
            _ => "unknown",
        };
        let change = MemberChange {
            state: Some(("unreachable".to_owned(), Some(reason.to_owned()))),
            ..MemberChange::default()
        };
        let _ = self.store.room_member(self.id, &member.member, &change);
        Turn::Failed(format!(
            "{reason}: the member's gateway refused the room ({refused:?})"
        ))
    }
}

fn read(ended: Result<String, String>) -> Turn {
    match ended {
        Ok(reply) if passes(&reply) => Turn::Passed,
        Ok(reply) => Turn::Said(reply),
        Err(failure) => Turn::Failed(failure),
    }
}

/// Waits for a member gateway's run.
async fn poll_run(client: &crate::daemon_cmd::peers::client::Peer, room: &str, run: &str) -> Turn {
    let path = format!("/api/rooms/{room}/turns/{run}");
    let started = Instant::now();
    while started.elapsed() < TURN_CAP {
        match client.call(reqwest::Method::GET, None, &path, None).await {
            Ok(state) if state["state"] == "done" => {
                return read(Ok(state["reply"].as_str().unwrap_or_default().to_owned()));
            }
            Ok(state) if state["state"] == "failed" => {
                return Turn::Failed(state["reason"].as_str().unwrap_or("unknown").to_owned());
            }
            Ok(_) => tokio::time::sleep(Duration::from_secs(1)).await,
            Err(err) => return Turn::Failed(format!("runtime_offline: {err:?}")),
        }
    }
    Turn::Failed("delivery_timeout: the member's turn outlived its cap".to_owned())
}

/// A member turn of this home in its room session: the reply, or
/// `<code>: <message>`. Kept while it makes progress (new messages in the
/// session), up to [`TURN_CAP`]; expired after [`NO_PROGRESS`] without.
pub(super) async fn local_turn(
    host: &DaemonHost,
    profile: &Arc<Profile>,
    room: &RoomRow,
    text: &str,
) -> Result<String, String> {
    let session = member_session(&profile.store, room).map_err(|e| e.message().to_owned())?;
    let gateway = host
        .gateway
        .get()
        .ok_or("runtime_offline: the gateway is not running")?;
    let params = SubmitParams {
        session_id: session.clone(),
        text: text.to_owned(),
        images: Vec::new(),
        files: Vec::new(),
        run_budget_secs: None,
        input_id: None,
        author: None,
        voice: false,
    };
    let author = Author {
        client_kind: "room".to_owned(),
        client_id: room.id.clone(),
        principal: profile.scope().name().to_string(),
        sent_at: Some(Timestamp::now().to_string()),
        prefix: None,
        origin: None,
    };
    // A submit that fails is never resubmitted: it may have started.
    let submitted = Box::pin(host.submit_turn(
        profile,
        params,
        (author, gateway.link(&session)),
        Some(pmt_config::schema::BusyInputMode::Queue),
        None,
    ))
    .await
    .map_err(|e| format!("{}: {}", e.code().code, e.message()))?;
    let id: pmt_core::id::SessionId = session
        .parse()
        .map_err(|_| "unknown: bad session".to_owned())?;
    let started = Instant::now();
    let mut progress = (Instant::now(), 0);
    loop {
        if let Some(ended) =
            super::super::turn_results::wait(&submitted.turn_id, Duration::from_secs(1)).await
        {
            return ended;
        }
        let count = profile.store.session(id).map_or(0, |s| s.message_count);
        if count != progress.1 {
            progress = (Instant::now(), count);
        }
        if progress.0.elapsed() > NO_PROGRESS || started.elapsed() > TURN_CAP {
            return Err("delivery_timeout: the member's turn stopped making progress".to_owned());
        }
    }
}
