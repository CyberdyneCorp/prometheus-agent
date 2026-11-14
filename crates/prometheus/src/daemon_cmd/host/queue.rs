//! The session's prompt queue as its clients see it (spec: desktop-app,
//! Composer history and queue editing): `queue.list`, `queue.edit`,
//! `queue.delete`, `queue.pause` and `queue.resume`. The queue is held by
//! the daemon, so every attached window shows the same list, and it is
//! kept in `<profile>/queues/<session>.json` so it survives app and
//! daemon restarts.
//!
//! A turn that was running when the daemon stopped (or died) has lost
//! its outcome: when the session loads again it is listed first as
//! `lost`, and it blocks the queue until a client deletes it (Discard),
//! which never sends it again. A turn a crash left in the turn journal
//! belongs to crash recovery instead (`crash.rs`), which resumes or asks.
//!
//! Only clients' own inputs are listed and kept: a messaging lane's,
//! automation's and a resumed crash's turns run through the same queue
//! but have their own recovery.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_gateway::Link;
use pmt_gateway::host::QueueOp;
use pmt_rpc::contracts::{QueueChanged, QueueEntry, QueueState, SubmitParams};
use serde::{Deserialize, Serialize};

use super::turns::Queued;
use super::{DaemonHost, Loaded, Profile, session_id, setup};

/// A client's input as kept on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Saved {
    pub(super) id: String,
    pub(super) text: String,
    #[serde(default)]
    pub(super) files: Vec<String>,
}

/// What the queue holds besides its jobs.
#[derive(Debug, Default)]
pub(super) struct Meta {
    paused: bool,
    /// The turn whose outcome a daemon stop lost; it blocks the queue.
    lost: Option<Saved>,
    /// The client input whose turn runs now.
    running: Option<Saved>,
    /// The daemon is stopping: nothing more starts.
    stopping: bool,
}

/// `<profile>/queues/<session>.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Kept {
    #[serde(default)]
    entries: Vec<Saved>,
    #[serde(default)]
    paused: bool,
    #[serde(default)]
    running: Option<Saved>,
    #[serde(default)]
    lost: Option<Saved>,
}

fn kept_path(session: &Loaded) -> PathBuf {
    session
        .live
        .config
        .get()
        .dir()
        .join("queues")
        .join(format!("{}.json", session.live.id))
}

impl Loaded {
    /// The queue as clients see it.
    pub(super) fn queue_state(&self) -> QueueState {
        let queue = self.queue.lock().expect("queue poisoned");
        let meta = self.queue_meta.lock().expect("queue meta poisoned");
        let lost = meta.lost.iter().map(|s| entry(s, true));
        let queued = queue.iter().filter_map(|job| job.entry.as_ref());
        QueueState {
            session_id: self.live.id.to_string(),
            entries: lost.chain(queued.map(|s| entry(s, false))).collect(),
            paused: meta.paused,
        }
    }

    /// Whether queued input must wait: a lost turn blocks it, a pause or
    /// a stopping daemon holds it.
    pub(super) fn queue_held(&self) -> bool {
        let meta = self.queue_meta.lock().expect("queue meta poisoned");
        meta.lost.is_some() || meta.paused || meta.stopping
    }

    /// Whether a new client input must queue even when no turn runs: a
    /// lost turn blocks the queue, a stopping daemon starts nothing, and
    /// a paused queue keeps its order (input arriving at an empty paused
    /// queue runs).
    pub(super) fn queue_blocks_input(&self) -> bool {
        let queue = self.queue.lock().expect("queue poisoned");
        let meta = self.queue_meta.lock().expect("queue meta poisoned");
        meta.lost.is_some() || meta.stopping || (meta.paused && !queue.is_empty())
    }

    /// Keeps the queue on disk, then tells the session's clients.
    pub(super) fn queue_changed(&self, link: &Link) {
        self.keep_queue();
        link.emit::<QueueChanged>(&self.queue_state());
    }

    /// Keeps the queue on disk (and removes the file once it is empty).
    fn keep_queue(&self) {
        let kept = {
            let queue = self.queue.lock().expect("queue poisoned");
            let meta = self.queue_meta.lock().expect("queue meta poisoned");
            Kept {
                entries: queue.iter().filter_map(|j| j.entry.clone()).collect(),
                paused: meta.paused,
                running: meta.running.clone(),
                lost: meta.lost.clone(),
            }
        };
        if let Err(err) = keep(&kept_path(self), &kept) {
            tracing::warn!(error = %err, "session queue not kept");
        }
    }

    /// The client input `job` starts running.
    pub(super) fn queue_running(&self, job: Option<&Saved>, link: &Link) {
        if job.is_none()
            && self
                .queue_meta
                .lock()
                .expect("queue meta poisoned")
                .running
                .is_none()
        {
            return;
        }
        self.queue_meta.lock().expect("queue meta poisoned").running = job.cloned();
        self.queue_changed(link);
    }

    /// The daemon is stopping: nothing queued starts, and the running
    /// client input (if any) becomes the lost turn once it is stopped.
    pub(super) fn queue_stopping(&self) -> Option<Saved> {
        let mut meta = self.queue_meta.lock().expect("queue meta poisoned");
        meta.stopping = true;
        meta.running.clone()
    }

    /// Records the turn a stop interrupted as lost (it stays on disk for
    /// the next start; the clients are going away with the daemon).
    pub(super) fn queue_lost(&self, running: Saved) {
        self.queue_meta.lock().expect("queue meta poisoned").lost = Some(running);
        self.keep_queue();
    }
}

fn entry(saved: &Saved, lost: bool) -> QueueEntry {
    QueueEntry {
        id: saved.id.clone(),
        text: saved.text.clone(),
        lost,
    }
}

fn keep(path: &std::path::Path, kept: &Kept) -> std::io::Result<()> {
    if kept.entries.is_empty() && kept.running.is_none() && kept.lost.is_none() && !kept.paused {
        return match std::fs::remove_file(path) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        };
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let bytes = serde_json::to_vec_pretty(kept).map_err(std::io::Error::other)?;
    pmt_core::private_fs::write_atomic(path, &bytes)
}

impl DaemonHost {
    /// Restores the kept queue of a session that just loaded: its inputs
    /// queue again and a turn that was running becomes the lost turn
    /// (unless the turn journal has it: crash recovery owns that one).
    pub(super) async fn restore_queue(
        &self,
        profile: &Arc<Profile>,
        id: SessionId,
        session: &Arc<Loaded>,
        link: &Link,
    ) {
        let path = kept_path(session);
        let Ok(bytes) = std::fs::read(&path) else {
            return;
        };
        let kept: Kept = match serde_json::from_slice(&bytes) {
            Ok(kept) => kept,
            Err(err) => return tracing::warn!(error = %err, "kept session queue unreadable"),
        };
        let journaled = session
            .live
            .agent
            .unfinished_turn()
            .ok()
            .flatten()
            .is_some();
        {
            let mut meta = session.queue_meta.lock().expect("queue meta poisoned");
            meta.paused = kept.paused;
            meta.lost = kept.lost.or(kept.running).filter(|_| !journaled);
        }
        for saved in kept.entries {
            match job(session, saved, link).await {
                Ok(job) => session.queue.lock().expect("queue poisoned").push_back(job),
                Err(err) => tracing::warn!(error = %err, "kept queued input dropped"),
            }
        }
        session.queue_changed(link);
        self.run_queue(profile, id, session);
    }

    /// The daemon is stopping: no queued input of any session starts.
    pub(super) async fn hold_queues(&self) {
        for (_, profile) in self.all_profiles() {
            for session in profile.loaded.lock().await.values() {
                session.queue_stopping();
            }
        }
    }

    /// Starts the first queued input when the session is idle and its
    /// queue is not held.
    pub(super) fn run_queue(&self, profile: &Arc<Profile>, id: SessionId, session: &Arc<Loaded>) {
        if session.queue_held() {
            return;
        }
        let first = {
            let mut queue = session.queue.lock().expect("queue poisoned");
            if queue.is_empty() || session.busy.swap(true, Ordering::AcqRel) {
                return;
            }
            queue.pop_front()
        };
        if let Some(first) = first
            && let Err(err) = self.start(Arc::clone(profile), id, Arc::clone(session), first)
        {
            tracing::warn!(error = %err, "queued input did not start");
        }
    }

    /// One `queue.*` call on a session of `profile`.
    pub(super) async fn queue_op(
        &self,
        profile: &Arc<Profile>,
        op: QueueOp,
        link: &Link,
    ) -> Result<QueueState, Error> {
        let id = session_id(op.session_id())?;
        let session = self.load(profile, id, link).await?;
        match op {
            QueueOp::List(_) => return Ok(session.queue_state()),
            QueueOp::Edit {
                id: entry, text, ..
            } => edit(&session, &entry, text, link).await?,
            QueueOp::Delete { id: entry, .. } => delete(&session, &entry)?,
            QueueOp::Pause(_) => {
                session
                    .queue_meta
                    .lock()
                    .expect("queue meta poisoned")
                    .paused = true;
            }
            QueueOp::Resume(_) => {
                session
                    .queue_meta
                    .lock()
                    .expect("queue meta poisoned")
                    .paused = false;
            }
        }
        session.queue_changed(link);
        self.run_queue(profile, id, &session);
        Ok(session.queue_state())
    }
}

/// A queued job for a client's input, its turn input built now.
async fn job(session: &Loaded, saved: Saved, link: &Link) -> Result<Queued, Error> {
    let params = SubmitParams {
        session_id: session.live.id.to_string(),
        text: saved.text.clone(),
        images: Vec::new(),
        files: saved.files.clone(),
        run_budget_secs: None,
        input_id: None,
        author: None,
    };
    let input = setup::input(&session.factory, &session.live, &params).await?;
    Ok(Queued {
        input,
        turn_id: saved.id.clone(),
        run_budget: None,
        link: link.clone(),
        from_lane: false,
        resume: None,
        api: None,
        entry: Some(saved),
    })
}

fn not_queued(id: &str) -> Error {
    Error::new(
        &codes::QUEUE_ENTRY_NOT_FOUND,
        format!("{id} is no longer queued"),
    )
}

async fn edit(session: &Loaded, id: &str, text: String, link: &Link) -> Result<(), Error> {
    let files = {
        let queue = session.queue.lock().expect("queue poisoned");
        let saved = queue
            .iter()
            .find_map(|j| j.entry.as_ref().filter(|s| s.id == id));
        saved.ok_or_else(|| not_queued(id))?.files.clone()
    };
    let saved = Saved {
        id: id.to_owned(),
        text,
        files,
    };
    let edited = job(session, saved, link).await?;
    let mut queue = session.queue.lock().expect("queue poisoned");
    let slot = queue
        .iter_mut()
        .find(|j| j.entry.as_ref().is_some_and(|s| s.id == id))
        .ok_or_else(|| not_queued(id))?;
    // The job keeps the link of the client that queued it.
    let link = slot.link.clone();
    *slot = Queued { link, ..edited };
    Ok(())
}

fn delete(session: &Loaded, id: &str) -> Result<(), Error> {
    {
        let mut meta = session.queue_meta.lock().expect("queue meta poisoned");
        if meta.lost.as_ref().is_some_and(|s| s.id == id) {
            meta.lost = None;
            return Ok(());
        }
    }
    let mut queue = session.queue.lock().expect("queue poisoned");
    let before = queue.len();
    queue.retain(|j| j.entry.as_ref().is_none_or(|s| s.id != id));
    if queue.len() == before {
        return Err(not_queued(id));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Kept, Saved, keep};

    #[test]
    fn an_empty_queue_leaves_no_file_and_a_paused_one_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("queues/ses.json");
        keep(&path, &Kept::default()).unwrap();
        assert!(!path.exists());
        let kept = Kept {
            entries: vec![Saved {
                id: "t1".into(),
                text: "next".into(),
                files: Vec::new(),
            }],
            paused: true,
            ..Kept::default()
        };
        keep(&path, &kept).unwrap();
        let read: Kept = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(read.entries[0].text, "next");
        assert!(read.paused);
        keep(&path, &Kept::default()).unwrap();
        assert!(!path.exists());
    }
}
