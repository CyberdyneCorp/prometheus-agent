//! Session records and their typed lineage (spec: session-storage, Session
//! record and source enum; First end reason wins; Typed session lineage).

use jiff::Timestamp;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::error::{corrupt, sql};
use crate::model::{Archived, EndReason, LineageKind, Origin, Source, TitleSource};
use crate::store::Store;
use crate::titles;

/// A session to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSession {
    /// The creating surface.
    pub source: Source,
    /// Integration tag (`chat --source <tag>`); `oneshot` and `tool`
    /// sessions are hidden from pickers and continue.
    pub source_tag: Option<String>,
    /// Typed origin (gateway chat, cron job, delegating parent).
    pub origin: Origin,
    /// Model name.
    pub model: Option<String>,
    /// Provider name.
    pub provider: Option<String>,
    /// Working directory.
    pub cwd: Option<String>,
    /// Enclosing git repository root.
    pub git_repo_root: Option<String>,
    /// Current git branch.
    pub git_branch: Option<String>,
    /// Initial title (sanitised); a titled parent numbers compression and
    /// fork children automatically when this is `None`.
    pub title: Option<String>,
    /// Provenance of `title`.
    pub title_source: TitleSource,
    /// Hidden from listings.
    pub hidden: bool,
    /// The canonical Bot Chat marker (bot-mode).
    pub bot_chat: bool,
    /// Hash of the system prompt the session starts with.
    pub system_prompt_hash: Option<String>,
    /// The parent and how this session relates to it.
    pub parent: Option<(SessionId, LineageKind)>,
}

impl NewSession {
    /// A root session from `source` with every optional field empty.
    #[must_use]
    pub fn new(source: Source) -> Self {
        Self {
            source,
            source_tag: None,
            origin: Origin::None,
            model: None,
            provider: None,
            cwd: None,
            git_repo_root: None,
            git_branch: None,
            title: None,
            title_source: TitleSource::User,
            hidden: false,
            bot_chat: false,
            system_prompt_hash: None,
            parent: None,
        }
    }

    /// A child of `parent` related by `kind`.
    #[must_use]
    pub fn child(source: Source, parent: SessionId, kind: LineageKind) -> Self {
        Self {
            parent: Some((parent, kind)),
            ..Self::new(source)
        }
    }
}

/// A stored session.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // mirrors the record's flags
pub struct Session {
    /// `ses_<ULID>`.
    pub id: SessionId,
    /// The owning profile's id.
    pub profile_id: String,
    /// The creating surface.
    pub source: Source,
    /// Integration tag.
    pub source_tag: Option<String>,
    /// Typed origin.
    pub origin: Origin,
    /// Model name.
    pub model: Option<String>,
    /// Provider name.
    pub provider: Option<String>,
    /// Working directory.
    pub cwd: Option<String>,
    /// Git repository root.
    pub git_repo_root: Option<String>,
    /// Git branch.
    pub git_branch: Option<String>,
    /// Title, unique per profile.
    pub title: Option<String>,
    /// Title provenance.
    pub title_source: Option<TitleSource>,
    /// Creation time.
    pub started_at: Timestamp,
    /// Last write to the session.
    pub last_activity_at: Timestamp,
    /// When it ended (first end wins).
    pub ended_at: Option<Timestamp>,
    /// Why it ended.
    pub end_reason: Option<EndReason>,
    /// Pinned by the user.
    pub pinned: bool,
    /// Archive state.
    pub archived: Archived,
    /// Hidden from listings.
    pub hidden: bool,
    /// Bot Chat marker.
    pub bot_chat: bool,
    /// System prompt hash.
    pub system_prompt_hash: Option<String>,
    /// Parent session.
    pub parent_session_id: Option<SessionId>,
    /// Relation to the parent (kept when the parent was deleted).
    pub lineage_kind: Option<LineageKind>,
    /// The parent was deleted (the child was re-parented to none).
    pub parent_deleted: bool,
    /// Root of the lineage (the session itself for a root).
    pub root_session_id: SessionId,
    /// Messages stored (all rows, including inactive ones).
    pub message_count: u64,
    /// Tool calls stored.
    pub tool_call_count: u64,
    /// Committed in-place compactions (the latest epoch).
    pub compaction_count: u32,
    /// Rewinds so far (the latest rewind batch).
    pub rewind_count: u32,
    /// When the retention sweep deleted the session's trajectory.
    pub trajectory_purged_at: Option<Timestamp>,
}

/// One descendant in [`Store::tree`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode {
    /// The descendant.
    pub id: SessionId,
    /// Its parent.
    pub parent: SessionId,
    /// How it relates to the parent.
    pub kind: LineageKind,
    /// Distance from the root (children of the root are 1).
    pub depth: u32,
}

pub(crate) const COLUMNS: &str = "id, profile_id, source, origin, model, provider, cwd, \
    git_repo_root, git_branch, title, title_source, started_at, last_activity_at, ended_at, \
    end_reason, pinned, archived, hidden, bot_chat, system_prompt_hash, parent_session_id, \
    lineage_kind, root_session_id, message_count, tool_call_count, compaction_count, source_tag, \
    parent_deleted, rewind_count, trajectory_purged_at";

pub(crate) fn now_ms() -> i64 {
    Timestamp::now().as_millisecond()
}

fn timestamp(ms: i64) -> Timestamp {
    Timestamp::from_millisecond(ms).unwrap_or(Timestamp::UNIX_EPOCH)
}

pub(crate) fn session_from_row(row: &Row<'_>) -> Result<Session, Error> {
    let get_text = |i: usize| row.get::<_, Option<String>>(i).map_err(sql);
    let get_int = |i: usize| row.get::<_, i64>(i).map_err(sql);
    let get_flag = |i: usize| row.get::<_, bool>(i).map_err(sql);
    let required = |i: usize| row.get::<_, String>(i).map_err(sql);
    let origin = required(3)?;
    Ok(Session {
        id: SessionId::parse(&required(0)?)?,
        profile_id: required(1)?,
        source: required(2)?.parse()?,
        origin: serde_json::from_str(&origin).map_err(|e| corrupt("origin", e))?,
        model: get_text(4)?,
        provider: get_text(5)?,
        cwd: get_text(6)?,
        git_repo_root: get_text(7)?,
        git_branch: get_text(8)?,
        title: get_text(9)?,
        title_source: get_text(10)?.as_deref().map(str::parse).transpose()?,
        started_at: timestamp(get_int(11)?),
        last_activity_at: timestamp(get_int(12)?),
        ended_at: row.get::<_, Option<i64>>(13).map_err(sql)?.map(timestamp),
        end_reason: get_text(14)?.as_deref().map(str::parse).transpose()?,
        pinned: get_flag(15)?,
        archived: required(16)?.parse()?,
        hidden: get_flag(17)?,
        bot_chat: get_flag(18)?,
        system_prompt_hash: get_text(19)?,
        parent_session_id: get_text(20)?.as_deref().map(SessionId::parse).transpose()?,
        lineage_kind: get_text(21)?.as_deref().map(str::parse).transpose()?,
        root_session_id: SessionId::parse(&required(22)?)?,
        message_count: u64::try_from(get_int(23)?).unwrap_or(0),
        tool_call_count: u64::try_from(get_int(24)?).unwrap_or(0),
        compaction_count: u32::try_from(get_int(25)?).unwrap_or(0),
        source_tag: get_text(26)?,
        parent_deleted: get_flag(27)?,
        rewind_count: u32::try_from(get_int(28)?).unwrap_or(0),
        trajectory_purged_at: row.get::<_, Option<i64>>(29).map_err(sql)?.map(timestamp),
    })
}

/// Tags whose sessions are integrations (one-shot runs, tool callers):
/// hidden from pickers, `-c` and `--resume latest`.
pub const HIDDEN_SOURCE_TAGS: &[&str] = &["oneshot", "tool"];

/// The tag of one-shot runs (`source = cli`).
pub const ONESHOT_TAG: &str = "oneshot";

/// Checks `^[a-z][a-z0-9_-]{0,31}$`.
///
/// # Errors
///
/// `session.invalid_source_tag`.
pub fn check_source_tag(tag: &str) -> Result<(), Error> {
    let mut chars = tag.chars();
    let valid = chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && tag.len() <= 32
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if valid {
        return Ok(());
    }
    Err(Error::new(
        &codes::SESSION_INVALID_SOURCE_TAG,
        format!(
            "invalid source tag '{tag}': use 1-32 of a-z, 0-9, '_' and '-', starting with a letter"
        ),
    ))
}

pub(crate) fn not_found(id: SessionId) -> Error {
    Error::new(
        &codes::SESSION_NOT_FOUND,
        format!("no session {id} in this profile"),
    )
}

pub(crate) fn load(conn: &Connection, id: SessionId) -> Result<Option<Session>, Error> {
    let mut stmt = conn
        .prepare_cached(&format!("SELECT {COLUMNS} FROM sessions WHERE id = ?1"))
        .map_err(sql)?;
    let mut rows = stmt.query([id.to_string()]).map_err(sql)?;
    rows.next().map_err(sql)?.map(session_from_row).transpose()
}

impl Store {
    /// Creates a session: a root, or a child whose `root_session_id` is
    /// its parent's root. A compression or fork child of a titled parent
    /// gets the next `<base> #<n>` title unless one is given.
    ///
    /// # Errors
    ///
    /// `session.not_found` (parent), `store.title_too_long`,
    /// `store.title_in_use`, `store.schema_too_new`, `store.sqlite`.
    ///
    /// # Panics
    ///
    /// Never: origins always serialize.
    pub fn create_session(&self, new: &NewSession) -> Result<Session, Error> {
        if let Some(tag) = &new.source_tag {
            check_source_tag(tag)?;
        }
        let given_title = new
            .title
            .as_deref()
            .map(titles::sanitize)
            .transpose()?
            .flatten();
        let id = SessionId::new();
        let origin = serde_json::to_string(&new.origin).expect("origins serialize");
        let profile_id = self.profile_id().to_string();
        self.write(|tx| {
            let parent = new
                .parent
                .map(|(parent, kind)| {
                    load(tx, parent)?
                        .map(|session| (session, kind))
                        .ok_or_else(|| not_found(parent))
                })
                .transpose()?;
            let (title, title_source) = match (&given_title, &parent) {
                (Some(title), _) => (Some(title.clone()), Some(new.title_source)),
                (None, Some((parent, kind))) => titles::inherited(tx, parent, *kind)?,
                (None, None) => (None, None),
            };
            if let Some(title) = &title {
                titles::ensure_unused(tx, title, None)?;
            }
            let root = parent.as_ref().map_or(id, |(p, _)| p.root_session_id);
            let now = now_ms();
            tx.execute(
                "INSERT INTO sessions (id, profile_id, source, origin, model, provider, cwd, \
                 git_repo_root, git_branch, title, title_source, started_at, last_activity_at, \
                 hidden, bot_chat, system_prompt_hash, parent_session_id, lineage_kind, \
                 root_session_id, source_tag) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, \
                 ?11, ?12, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
                params![
                    id.to_string(),
                    profile_id,
                    new.source.as_str(),
                    origin,
                    new.model,
                    new.provider,
                    new.cwd,
                    new.git_repo_root,
                    new.git_branch,
                    title,
                    title_source.map(TitleSource::as_str),
                    now,
                    new.hidden,
                    new.bot_chat,
                    new.system_prompt_hash,
                    parent.as_ref().map(|(p, _)| p.id.to_string()),
                    parent.as_ref().map(|(_, kind)| kind.as_str()),
                    root.to_string(),
                    new.source_tag,
                ],
            )
            .map_err(sql)?;
            load(tx, id)?.ok_or_else(|| not_found(id))
        })
    }

    /// The session `id`.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.sqlite`.
    pub fn session(&self, id: SessionId) -> Result<Session, Error> {
        self.read(|conn| load(conn, id)?.ok_or_else(|| not_found(id)))
    }

    /// Ends a session unless it already ended: the first end reason wins.
    /// Returns whether this call ended it.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.schema_too_new`, `store.sqlite`.
    pub fn end_session(&self, id: SessionId, reason: EndReason) -> Result<bool, Error> {
        self.write(|tx| {
            let changed = tx
                .execute(
                    "UPDATE sessions SET ended_at = ?2, end_reason = ?3 \
                     WHERE id = ?1 AND ended_at IS NULL",
                    params![id.to_string(), now_ms(), reason.as_str()],
                )
                .map_err(sql)?;
            if changed == 0 && load(tx, id)?.is_none() {
                return Err(not_found(id));
            }
            Ok(changed == 1)
        })
    }

    /// Reopens (resumes) a session: clears the end and an automatic
    /// archive flag, keeping a user archive flag.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.schema_too_new`, `store.sqlite`.
    pub fn reopen_session(&self, id: SessionId) -> Result<Session, Error> {
        self.write(|tx| {
            tx.execute(
                "UPDATE sessions SET ended_at = NULL, end_reason = NULL, last_activity_at = ?2, \
                 archived = CASE archived WHEN 'auto' THEN 'none' ELSE archived END WHERE id = ?1",
                params![id.to_string(), now_ms()],
            )
            .map_err(sql)?;
            load(tx, id)?.ok_or_else(|| not_found(id))
        })
    }

    /// The chain from the root to `id`, followed by `id`'s compression
    /// continuations up to the compression tip (forks and other children
    /// are not continuations).
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.sqlite`.
    pub fn lineage(&self, id: SessionId) -> Result<Vec<SessionId>, Error> {
        self.read(|conn| {
            let mut chain = ancestors(conn, id)?;
            if chain.is_empty() {
                return Err(not_found(id));
            }
            let mut tip = id;
            while let Some(next) = continuation(conn, tip)? {
                chain.push(next);
                tip = next;
            }
            Ok(chain)
        })
    }

    /// The last session of `id`'s compression chain (`id` itself when it
    /// was not continued).
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.sqlite`.
    pub fn compression_tip(&self, id: SessionId) -> Result<SessionId, Error> {
        let chain = self.lineage(id)?;
        Ok(chain.last().copied().unwrap_or(id))
    }

    /// Every descendant of `root` with its relation, by depth then age.
    ///
    /// # Errors
    ///
    /// `session.not_found`, `store.sqlite`.
    pub fn tree(&self, root: SessionId) -> Result<Vec<TreeNode>, Error> {
        self.read(|conn| {
            if load(conn, root)?.is_none() {
                return Err(not_found(root));
            }
            let mut stmt = conn
                .prepare(
                    "WITH RECURSIVE down(id, parent, kind, depth, started) AS ( \
                       SELECT id, parent_session_id, lineage_kind, 1, started_at FROM sessions \
                       WHERE parent_session_id = ?1 \
                       UNION ALL \
                       SELECT s.id, s.parent_session_id, s.lineage_kind, down.depth + 1, s.started_at \
                       FROM sessions s JOIN down ON s.parent_session_id = down.id) \
                     SELECT id, parent, kind, depth FROM down ORDER BY depth, started, id",
                )
                .map_err(sql)?;
            let rows = stmt
                .query_map([root.to_string()], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, u32>(3)?,
                    ))
                })
                .map_err(sql)?;
            rows.map(|row| {
                let (id, parent, kind, depth) = row.map_err(sql)?;
                Ok(TreeNode {
                    id: SessionId::parse(&id)?,
                    parent: SessionId::parse(&parent)?,
                    kind: kind.parse()?,
                    depth,
                })
            })
            .collect()
        })
    }
}

/// `id` and its ancestors, root first; empty when `id` does not exist.
fn ancestors(conn: &Connection, id: SessionId) -> Result<Vec<SessionId>, Error> {
    let mut stmt = conn
        .prepare(
            "WITH RECURSIVE up(id, parent, depth) AS ( \
               SELECT id, parent_session_id, 0 FROM sessions WHERE id = ?1 \
               UNION ALL \
               SELECT s.id, s.parent_session_id, up.depth + 1 FROM sessions s \
               JOIN up ON s.id = up.parent) \
             SELECT id FROM up ORDER BY depth DESC",
        )
        .map_err(sql)?;
    let rows = stmt
        .query_map([id.to_string()], |row| row.get::<_, String>(0))
        .map_err(sql)?;
    rows.map(|row| SessionId::parse(&row.map_err(sql)?))
        .collect()
}

/// The newest compression child of `id`.
fn continuation(conn: &Connection, id: SessionId) -> Result<Option<SessionId>, Error> {
    conn.query_row(
        "SELECT id FROM sessions WHERE parent_session_id = ?1 AND lineage_kind = 'compression' \
         ORDER BY started_at DESC, id DESC LIMIT 1",
        [id.to_string()],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(sql)?
    .map(|id| SessionId::parse(&id))
    .transpose()
}
