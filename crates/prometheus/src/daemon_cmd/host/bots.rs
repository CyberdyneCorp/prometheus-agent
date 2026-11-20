//! Bot Mode in the daemon (spec: bot-mode): `bot.*` metadata, creation,
//! duplication and deletion, routines and sections, and how a local bot
//! shows on the roster. `groups.*` go to the room driver
//! ([`super::rooms`]); deliveries run in [`super::bot_delivery`].

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use pmt_config::{Home, ProfileName};
use pmt_core::error::{Error, codes};
use pmt_gateway::bots::BotRequest;
use pmt_rpc::contracts::{
    BotCreateParams, BotCreated, BotDeleteParams, BotDeleted, BotDone, BotDuplicateParams, BotMeta,
    BotMetaParams, BotMetaUpdateParams, BotRoutine, BotRoutines, BotSectionDeleteParams,
    BotSectionDeleted, RosterAgent, RosterChat,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{DaemonHost, Profile};
use crate::bot_host::{create, meta};

/// The roster's `active` window after a bot's last message.
const ACTIVE_WINDOW: Duration = Duration::from_secs(90);
/// How long a new bot's introduction turn may take.
const INTRODUCTION_WAIT: Duration = Duration::from_mins(2);
/// The store preference holding a Bot Chat's read marker.
const READ_PREF: &str = "bot.chat.read";
/// The prefix of a routine's job name.
fn routine_prefix(profile: &str) -> String {
    format!("[bot:{profile}] ")
}

fn params<T: DeserializeOwned>(value: Value) -> Result<T, Error> {
    serde_json::from_value(value)
        .map_err(|e| Error::new(&codes::RPC_INVALID_PARAMS, format!("invalid params: {e}")))
}

fn to_value<T: serde::Serialize>(value: &T) -> Result<Value, Error> {
    serde_json::to_value(value).map_err(|e| Error::new(&codes::RPC_INTERNAL, e.to_string()))
}

fn blocking_error(e: impl std::fmt::Display) -> Error {
    Error::new(&codes::TASK_PANICKED, e.to_string())
}

impl DaemonHost {
    /// One `bot.*` or `groups.*` call made for `profile`.
    pub(super) async fn bot_call(
        &self,
        profile: &str,
        request: BotRequest,
    ) -> Result<Value, Error> {
        let p = request.params;
        match request.method {
            "bot.meta.get" => self.meta_get(&params::<BotMetaParams>(p)?.profile),
            "bot.meta.update" => Box::pin(self.meta_update(params(p)?)).await,
            "bot.create" => Box::pin(self.bot_create(params(p)?)).await,
            "bot.duplicate" => Box::pin(self.bot_duplicate(params(p)?)).await,
            "bot.delete" => self.bot_delete(&params(p)?).await,
            "bot.routines.list" => self.routines(&params::<BotMetaParams>(p)?.profile),
            "bot.chat.read" => self.mark_read(&params::<BotMetaParams>(p)?.profile),
            "bot.sections.delete" => self.delete_section(&params(p)?),
            "voice.speak" => Box::pin(self.speak(params(p)?)).await,
            method => Box::pin(self.room_call(profile, method, p)).await,
        }
    }

    fn meta_get(&self, profile: &str) -> Result<Value, Error> {
        let served = self.profile(profile)?;
        let mut found = meta::read(&served.store)?;
        found.groups = self.groups_of(profile);
        to_value(&found)
    }

    async fn meta_update(&self, params: BotMetaUpdateParams) -> Result<Value, Error> {
        let served = self.profile(&params.profile)?;
        let image = match params.changes.get("avatar") {
            Some(avatar) if avatar["kind"] == "generated" => {
                let prompt = avatar["prompt"].as_str().unwrap_or_default().to_owned();
                Some(Box::pin(self.generate_avatar(&served, &prompt)).await?)
            }
            _ => None,
        };
        let generate = move |_: &str| {
            image
                .clone()
                .ok_or_else(|| Error::new(&codes::RPC_INVALID_PARAMS, "no generated image"))
        };
        let mut updated = meta::update(&served.store, &params.changes, &generate)?;
        updated.groups = self.groups_of(&params.profile);
        to_value(&updated)
    }

    /// Draws a generated avatar with the profile's image backend (through
    /// the media session of its Bot Chat): its bytes and type.
    async fn generate_avatar(
        &self,
        profile: &Arc<Profile>,
        prompt: &str,
    ) -> Result<(Vec<u8>, String), Error> {
        let (session, _) = self.ensure_bot_chat(profile).await?;
        let link = self
            .gateway
            .get()
            .map(|g| g.link(&session))
            .ok_or_else(|| Error::new(&codes::RPC_NOT_SERVED, "the gateway is not running"))?;
        self.load(profile, super::session_id(&session)?, &link)
            .await?;
        let media = pmt_media::MediaSessions::process()
            .session(&session)
            .ok_or_else(|| {
                Error::new(&codes::RPC_NOT_SERVED, "image generation is not available")
            })?;
        let result = media
            .generate_image(&json!({"prompt": prompt, "aspect_ratio": "square"}))
            .await;
        let text = result.text_content();
        let path = text
            .strip_prefix("MEDIA:")
            .and_then(|rest| rest.lines().next())
            .ok_or_else(|| Error::new(&codes::RPC_INVALID_PARAMS, text.clone()))?;
        let bytes = std::fs::read(path)
            .map_err(|e| Error::io(&codes::HOME_IO, std::path::Path::new(path), &e))?;
        Ok((bytes, String::new()))
    }

    fn home(&self) -> Result<Home, Error> {
        Home::open(self.profile(&self.launch_profile)?.scope().home())
    }

    async fn bot_create(&self, params: BotCreateParams) -> Result<Value, Error> {
        if let Some(peer) = params.on.clone().filter(|p| !p.is_empty()) {
            return self.create_on_peer(&peer, &params).await;
        }
        self.check_multiplex()?;
        let home = self.home()?;
        let asking = params.profile.clone();
        let made = pmt_core::spawn::blocking(move || create::create(&home, &asking, &params))
            .await
            .map_err(blocking_error)??;
        let created = Box::pin(self.adopt_bot(&made.profile, made.copied_secrets)).await?;
        to_value(&created)
    }

    async fn bot_duplicate(&self, params: BotDuplicateParams) -> Result<Value, Error> {
        self.check_multiplex()?;
        let home = self.home()?;
        let made = pmt_core::spawn::blocking(move || {
            create::duplicate(&home, &params.profile, &params.name)
        })
        .await
        .map_err(blocking_error)??;
        let created = Box::pin(self.adopt_bot(&made.profile, made.copied_secrets)).await?;
        to_value(&created)
    }

    /// Bots are profiles this daemon serves: it must serve every profile.
    fn check_multiplex(&self) -> Result<(), Error> {
        let launch = self.profile(&self.launch_profile)?.scope();
        if launch.config().gateway.multiplex {
            return Ok(());
        }
        Err(Error::new(
            &codes::RPC_INVALID_PARAMS,
            "this daemon serves only its launch profile, so a new bot would not be served",
        )
        .with_hint("prometheus config set gateway.multiplex true, then restart the daemon"))
    }

    /// Serves the new bot, creates its canonical Bot Chat and runs its
    /// introduction turn.
    async fn adopt_bot(&self, name: &str, copied: Vec<String>) -> Result<BotCreated, Error> {
        // Served as `gateway reload` serves a profile created since start.
        match self.gateway.get() {
            Some(gateway) => gateway.reload(),
            None => drop(self.open_new_profiles()),
        }
        let served = self.profile(name)?;
        let (session_id, _) = self.ensure_bot_chat(&served).await?;
        let introduction = Box::pin(self.introduce(&served, &session_id)).await;
        Ok(BotCreated {
            profile: name.to_owned(),
            handle: format!("@{name}"),
            session_id,
            device: "local".to_owned(),
            copied_secrets: copied,
            introduction,
        })
    }

    /// The new bot's first message: it introduces itself.
    async fn introduce(&self, profile: &Arc<Profile>, session: &str) -> Option<String> {
        let gateway = self.gateway.get()?;
        let params = pmt_rpc::contracts::SubmitParams {
            session_id: session.to_owned(),
            text: "You were just created as a bot in Bot Mode. Introduce yourself in two or \
                   three sentences: who you are and what you can help with."
                .to_owned(),
            images: Vec::new(),
            files: Vec::new(),
            run_budget_secs: None,
            input_id: None,
            author: None,
            voice: false,
        };
        let author = pmt_gateway::Author {
            client_kind: "bot-create".to_owned(),
            client_id: profile.scope().name().to_string(),
            principal: profile.scope().name().to_string(),
            sent_at: Some(Timestamp::now().to_string()),
            prefix: None,
            origin: None,
        };
        let submitted = Box::pin(self.submit_turn(
            profile,
            params,
            (author, gateway.link(session)),
            Some(pmt_config::schema::BusyInputMode::Queue),
            None,
        ))
        .await
        .ok()?;
        super::turn_results::wait(&submitted.turn_id, INTRODUCTION_WAIT)
            .await?
            .ok()
    }

    /// `bot.create` with `on`: the peer creates the bot through its API.
    async fn create_on_peer(&self, peer: &str, params: &BotCreateParams) -> Result<Value, Error> {
        let asking = self.fresh_scope(&params.profile)?;
        let client = crate::daemon_cmd::peers::client::Peer::of(&asking, peer)?;
        let mut body = to_value(params)?;
        body["on"] = Value::Null;
        let created = client
            .call(reqwest::Method::POST, None, "/api/bots", Some(&body))
            .await
            .map_err(|e| e.into_error(peer))?;
        let mut created: BotCreated = params_of(created)?;
        let local_twin = self.served().contains_key(&created.profile);
        created.handle = if local_twin {
            format!("@{}-{peer}", created.profile)
        } else {
            format!("@{}", created.profile)
        };
        peer.clone_into(&mut created.device);
        to_value(&created)
    }

    /// A Bot Mode REST call: `POST /api/bots`, or the member side of a
    /// hosted room.
    pub(super) async fn bots_api(
        &self,
        profile: &str,
        call: pmt_api::BotsCall,
    ) -> Result<(u16, Value), Error> {
        let path: Vec<&str> = call.path.iter().map(String::as_str).collect();
        match (call.method.as_str(), path.as_slice()) {
            ("POST", ["bots"]) => Ok((
                201,
                Box::pin(self.create_for_api(profile, call.body)).await?,
            )),
            (_, ["rooms", ..]) => self.room_api(profile, &call),
            _ => Ok((404, json!({"error": {"message": "no such bots route"}}))),
        }
    }

    /// `POST /api/bots` on this machine (a peer's `bot.create ... on`):
    /// the bot is created here, cloning only this machine's profiles.
    async fn create_for_api(&self, profile: &str, body: Value) -> Result<Value, Error> {
        let mut params: BotCreateParams = params(body)?;
        params.profile = profile.to_owned();
        params.on = None;
        Box::pin(self.bot_create(params)).await
    }

    async fn bot_delete(&self, params: &BotDeleteParams) -> Result<Value, Error> {
        if !params.confirm {
            return Err(Error::new(
                &codes::RPC_INVALID_PARAMS,
                "deleting a bot deletes its profile: pass confirm: true",
            ));
        }
        let name = ProfileName::parse(&params.profile)?;
        if name.as_str() == "default" {
            return Err(Error::new(
                &codes::PROFILE_PROTECTED,
                "the default profile cannot be deleted",
            ));
        }
        let removed = self
            .profiles
            .write()
            .expect("profiles poisoned")
            .remove(name.as_str());
        if let Some(served) = &removed {
            let ids: Vec<_> = served.loaded.lock().await.keys().copied().collect();
            for id in ids {
                Self::unload(served, id).await;
            }
        }
        drop(removed);
        let home = self.home()?;
        home.delete_profile_held_by(&name, Some(std::process::id()))?;
        to_value(&BotDeleted {
            profile: name.to_string(),
        })
    }

    fn routines(&self, profile: &str) -> Result<Value, Error> {
        let served = self.profile(profile)?;
        let prefix = routine_prefix(profile);
        let routines = served
            .store
            .cron_jobs()?
            .into_iter()
            .filter_map(|job| {
                let routine = job.name.strip_prefix(&prefix)?.to_owned();
                let schedule = pmt_cron::job::Job::from_row(job.clone())
                    .map(|parsed| parsed.spec.schedule.canonical())
                    .unwrap_or_default();
                Some(BotRoutine {
                    job_id: job.id,
                    routine,
                    schedule,
                    state: job.state,
                    next_run_at: job
                        .next_run_at
                        .and_then(|ms| Timestamp::from_millisecond(ms).ok())
                        .map(|t| t.to_string()),
                    last_status: job.last_status,
                })
            })
            .collect();
        to_value(&BotRoutines { routines })
    }

    /// Reads text of a session aloud with the owning profile's `tts.*`
    /// settings (spec: bot-mode, Bot voices): each bot sounds as its own
    /// profile says, on the daemon that owns it.
    async fn speak(&self, params: pmt_rpc::contracts::VoiceSpeakParams) -> Result<Value, Error> {
        use base64::Engine as _;
        let served = self.profile(&params.profile)?;
        served
            .store
            .session(super::session_id(&params.session_id)?)?;
        let scope = served.scope();
        let speech = super::speech::of_profile(&scope).ok_or_else(|| {
            Error::new(
                &codes::RPC_NOT_SERVED,
                "no speech providers for this profile",
            )
        })?;
        let dir = scope.dir().join("cache").join("audio");
        std::fs::create_dir_all(&dir).map_err(|e| Error::io(&codes::HOME_IO, &dir, &e))?;
        let path = speech
            .synthesize(&params.text, &dir)
            .await
            .map_err(|why| Error::new(&codes::RPC_NOT_SERVED, format!("speech failed: {why}")))?;
        let bytes = std::fs::read(&path).map_err(|e| Error::io(&codes::HOME_IO, &path, &e))?;
        let _ = std::fs::remove_file(&path);
        let mime = pmt_media::speech::audio::Format::sniff(&bytes)
            .mime()
            .to_owned();
        to_value(&pmt_rpc::contracts::VoiceSpoken {
            mime,
            audio: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
    }

    fn mark_read(&self, profile: &str) -> Result<Value, Error> {
        let served = self.profile(profile)?;
        if let Some(session) = served.store.bot_chat_id()? {
            let seq = served
                .store
                .latest_text(&session)?
                .map_or(0, |(_, _, seq)| seq);
            served
                .store
                .set_pref(READ_PREF, &format!("{session} {seq}"))?;
        }
        to_value(&BotDone {})
    }

    fn delete_section(&self, params: &BotSectionDeleteParams) -> Result<Value, Error> {
        let mut moved = Vec::new();
        for (name, served) in self.all_profiles() {
            let mut found: BotMeta = meta::read(&served.store)?;
            if found
                .section
                .as_ref()
                .is_some_and(|s| s.id == params.section_id)
            {
                found.section = None;
                let mut changes = serde_json::Map::new();
                changes.insert("section".to_owned(), Value::Null);
                meta::update(&served.store, &changes, &|_| {
                    Err(Error::new(&codes::RPC_INVALID_PARAMS, "no image here"))
                })?;
                moved.push(name);
            }
        }
        to_value(&BotSectionDeleted { moved })
    }

    /// A local bot as the roster shows it; `None` when Bot Mode is off for
    /// it (it leaves every roster).
    ///
    /// `heartbeats` are the profiles with a live kanban worker
    /// ([`worker_heartbeats`]): they count as active too.
    pub(super) async fn local_bot(
        &self,
        profile: &Profile,
        heartbeats: &HashSet<String>,
    ) -> Option<(RosterAgent, RosterChat)> {
        let scope = profile.scope();
        let config = scope.config();
        if !config.bot_mode.enabled {
            return None;
        }
        let name = scope.name().to_string();
        let found = meta::read(&profile.store).unwrap_or_default();
        let agent = RosterAgent {
            title: Some(meta::title_of(&name, &found)),
            description: found
                .description
                .clone()
                .or_else(|| config.a2a.server.description.clone()),
            avatar: Some(meta::resolved(&name, &found.avatar)),
            model: config.model.default.clone(),
            accepts_bot_messages: true,
            name,
        };
        let running = profile
            .loaded
            .lock()
            .await
            .values()
            .any(|s| s.busy.load(std::sync::atomic::Ordering::Acquire));
        let mut chat = chat_of(&profile.store).unwrap_or_default();
        chat.active |= running || heartbeats.contains(&agent.name);
        chat.hidden = found.hidden;
        chat.section = found.section;
        Some((agent, chat))
    }
}

fn params_of<T: DeserializeOwned>(value: Value) -> Result<T, Error> {
    serde_json::from_value(value)
        .map_err(|e| Error::new(&codes::PEER_REJECTED, format!("the peer's answer: {e}")))
}

/// The Bot Chat part of a roster row: preview, time, unread count and
/// whether it wrote within the active window.
fn chat_of(store: &pmt_store::Store) -> Result<RosterChat, Error> {
    let Some(session) = store.bot_chat_id()? else {
        return Ok(RosterChat::default());
    };
    let Some((text, at, _)) = store.latest_text(&session)? else {
        return Ok(RosterChat::default());
    };
    let read_seq = store
        .pref(READ_PREF)?
        .and_then(|marker| {
            let (id, seq) = marker.split_once(' ')?;
            (id == session).then(|| seq.parse::<u64>().ok()).flatten()
        })
        .unwrap_or(0);
    let age = Timestamp::now().duration_since(at);
    Ok(RosterChat {
        preview: text.map(|t| preview(&t)),
        last_message_at: Some(at.to_string()),
        unread: store.replies_after(&session, read_seq)?,
        active: age.as_secs() >= 0 && age.unsigned_abs() <= ACTIVE_WINDOW,
        ..RosterChat::default()
    })
}

/// How recent a worker heartbeat keeps a bot active: two beats, so one
/// late beat does not flicker the flag.
const HEARTBEAT_WINDOW_MS: i64 = 2 * pmt_kanban::dispatch::HEARTBEAT_MS;

/// The profiles of `home` running a kanban worker that heartbeat within
/// [`HEARTBEAT_WINDOW_MS`] (spec: bot-mode, Roster presentation: active
/// when the bot has a worker heartbeat). A home without boards has none;
/// listing never creates them.
pub(super) fn worker_heartbeats(home: &Path) -> HashSet<String> {
    if !home.join("kanban").is_dir() {
        return HashSet::new();
    }
    let Ok(boards) = pmt_kanban::Boards::open(home) else {
        return HashSet::new();
    };
    let since = pmt_kanban::now_ms() - HEARTBEAT_WINDOW_MS;
    boards
        .live()
        .unwrap_or_default()
        .iter()
        .filter_map(|slug| boards.board(slug).ok())
        .flat_map(|board| board.open_runs().unwrap_or_default())
        .filter(|run| run.role == "worker" && run.heartbeat_at.is_some_and(|at| at >= since))
        .map(|run| run.profile)
        .collect()
}

/// The first line of `text`, at most 120 characters.
fn preview(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or_default();
    match line.char_indices().nth(120) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use pmt_kanban::{Actor, Boards, ClaimOutcome, NewTask, Role};

    use super::*;

    /// Scenario part (Roster presentation): a bot whose kanban worker
    /// heartbeats is active; a silent one is not.
    #[test]
    fn a_recent_worker_heartbeat_marks_its_profile_active() {
        let home = tempfile::tempdir().unwrap();
        assert!(worker_heartbeats(home.path()).is_empty());
        assert!(
            !home.path().join("kanban").exists(),
            "listing creates no boards"
        );
        let mut board = Boards::open(home.path()).unwrap().board("default").unwrap();
        let now = pmt_kanban::now_ms();
        let exists = |_: &str| true;
        let mut claim = |profile: &str| {
            let mut task = NewTask::titled("digest");
            task.assignee = Some(profile.to_owned());
            let id = board
                .create(&Actor::user("cli"), &task, &exists, now)
                .unwrap()
                .id;
            let claimed = board
                .claim(
                    &Actor::dispatcher(),
                    &id,
                    profile,
                    Role::Worker,
                    900_000,
                    now,
                )
                .unwrap();
            let ClaimOutcome::Claimed(claim) = claimed else {
                panic!("not claimed: {claimed:?}");
            };
            claim.run_id
        };
        let (busy, silent) = (claim("research"), claim("scribe"));
        board
            .heartbeat(&Actor::worker("research", &busy), &busy, None, 900_000, now)
            .unwrap();
        let stale = now - HEARTBEAT_WINDOW_MS - 1;
        board
            .heartbeat(
                &Actor::worker("scribe", &silent),
                &silent,
                None,
                900_000,
                stale,
            )
            .unwrap();
        let active = worker_heartbeats(home.path());
        assert_eq!(active, HashSet::from(["research".to_owned()]));
    }
}
