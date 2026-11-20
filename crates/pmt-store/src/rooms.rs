//! Group chats (spec: bot-mode, Group chats; Durable room driver; Hosted
//! rooms across gateways; Room authority transfer). A home keeps its rooms
//! in the default profile's store: the room, its members, its log (in
//! sequence order), the driver lease (one driver per room for every
//! process sharing the home), the grants this home issued to other
//! gateways' room authorities (hashed) and the authority runs of its
//! members.

use pmt_core::error::{Error, codes};
use rusqlite::{OptionalExtension as _, Row, Transaction, params};
use sha2::{Digest as _, Sha256};

use crate::error::sql;
use crate::sessions::now_ms;
use crate::store::Store;

/// A room member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberRow {
    /// Its handle in the room.
    pub member: String,
    /// Its profile on its machine.
    pub profile: String,
    /// The peer gateway it lives on (`None`: this home).
    pub gateway: Option<String>,
    /// Held by a stop directive.
    pub held: bool,
    /// The last log entry its turns saw.
    pub seen_seq: u64,
    /// `ok` or `unreachable`.
    pub state: String,
    /// Why it is unreachable.
    pub reason: Option<String>,
    /// A remote authority run still running on the member's machine.
    pub pending_run: Option<String>,
}

/// A room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomRow {
    /// Stable id.
    pub id: String,
    /// Unique name.
    pub name: String,
    /// Picture.
    pub picture: Option<String>,
    /// Stop directives hold members.
    pub detect_stop_directives: bool,
    /// `authority`, `replica` or `demoted`.
    pub role: String,
    /// The authority's gateway id.
    pub authority_gateway: String,
    /// The authority's lineage.
    pub authority_epoch: u64,
    /// The peer a replica copies from.
    pub source_peer: Option<String>,
    /// The authority's last entry, as last reported (replicas).
    pub latest_seq: u64,
    /// A member asked for the user.
    pub needs_you: bool,
    /// The members, in order.
    pub members: Vec<MemberRow>,
}

/// One log entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryRow {
    /// Sequence number (from 1).
    pub seq: u64,
    /// What it is.
    pub kind: String,
    /// Who.
    pub author: Option<String>,
    /// Its text.
    pub text: Option<String>,
    /// Details (JSON).
    pub data: Option<String>,
    /// Unix ms.
    pub at: i64,
}

/// An authority run's state, reply and failure reason.
pub type RunState = (String, Option<String>, Option<String>);

/// A member change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemberChange {
    /// Hold or release.
    pub held: Option<bool>,
    /// What its turns saw.
    pub seen_seq: Option<u64>,
    /// Reachability and why.
    pub state: Option<(String, Option<String>)>,
    /// The remote run (`Some(None)` clears it).
    pub pending_run: Option<Option<String>>,
}

fn not_found(id: &str) -> Error {
    Error::new(&codes::ROOM_NOT_FOUND, format!("no room {id}"))
}

fn taken(name: &str) -> Error {
    Error::new(
        &codes::ROOM_NAME_TAKEN,
        format!("a room named '{name}' already exists"),
    )
}

fn u(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

fn i(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn member(row: &Row<'_>) -> rusqlite::Result<MemberRow> {
    Ok(MemberRow {
        member: row.get(0)?,
        profile: row.get(1)?,
        gateway: row.get(2)?,
        held: row.get::<_, i64>(3)? != 0,
        seen_seq: u(row.get(4)?),
        state: row.get(5)?,
        reason: row.get(6)?,
        pending_run: row.get(7)?,
    })
}

fn room(row: &Row<'_>) -> rusqlite::Result<RoomRow> {
    Ok(RoomRow {
        id: row.get(0)?,
        name: row.get(1)?,
        picture: row.get(2)?,
        detect_stop_directives: row.get::<_, i64>(3)? != 0,
        role: row.get(4)?,
        authority_gateway: row.get(5)?,
        authority_epoch: u(row.get(6)?),
        source_peer: row.get(7)?,
        latest_seq: u(row.get(8)?),
        needs_you: row.get::<_, i64>(9)? != 0,
        members: Vec::new(),
    })
}

const ROOM_COLUMNS: &str = "id, name, picture, detect_stop_directives, role, authority_gateway, \
    authority_epoch, source_peer, latest_seq, needs_you";

fn load(conn: &rusqlite::Connection, id: &str) -> Result<Option<RoomRow>, Error> {
    let found = conn
        .query_row(
            &format!("SELECT {ROOM_COLUMNS} FROM rooms WHERE id = ?1"),
            [id],
            room,
        )
        .optional()
        .map_err(sql)?;
    let Some(mut found) = found else {
        return Ok(None);
    };
    let mut stmt = conn
        .prepare(
            "SELECT member, profile, gateway, held, seen_seq, state, reason, pending_run \
             FROM room_members WHERE room_id = ?1 ORDER BY position",
        )
        .map_err(sql)?;
    found.members = stmt
        .query_map([id], member)
        .map_err(sql)?
        .collect::<Result<_, _>>()
        .map_err(sql)?;
    Ok(Some(found))
}

fn last_seq(tx: &Transaction<'_>, id: &str) -> Result<u64, Error> {
    tx.query_row(
        "SELECT COALESCE(MAX(seq), 0) FROM room_log WHERE room_id = ?1",
        [id],
        |row| row.get::<_, i64>(0),
    )
    .map(u)
    .map_err(sql)
}

impl Store {
    /// This home's gateway id (made on first use).
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn gateway_id(&self) -> Result<String, Error> {
        if let Some(id) = self.pref("rooms.gateway_id")? {
            return Ok(id);
        }
        let id = format!("gw_{}", ulid::Ulid::generate().to_string().to_lowercase());
        self.set_pref("rooms.gateway_id", &id)?;
        Ok(id)
    }

    /// Creates `room` (its members in order) as this gateway's.
    ///
    /// # Errors
    ///
    /// `room.name_taken`; store errors.
    pub fn room_create(&self, room: &RoomRow) -> Result<(), Error> {
        let now = now_ms();
        self.write(|tx| {
            let taken_by: Option<String> = tx
                .query_row("SELECT id FROM rooms WHERE name = ?1", [&room.name], |r| {
                    r.get(0)
                })
                .optional()
                .map_err(sql)?;
            if taken_by.is_some() {
                return Err(taken(&room.name));
            }
            tx.execute(
                "INSERT INTO rooms (id, name, picture, detect_stop_directives, role, \
                 authority_gateway, authority_epoch, source_peer, latest_seq, created_at, \
                 updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
                params![
                    room.id,
                    room.name,
                    room.picture,
                    i64::from(room.detect_stop_directives),
                    room.role,
                    room.authority_gateway,
                    i(room.authority_epoch),
                    room.source_peer,
                    i(room.latest_seq),
                    now
                ],
            )
            .map_err(sql)?;
            for (position, m) in room.members.iter().enumerate() {
                tx.execute(
                    "INSERT INTO room_members (room_id, member, profile, gateway, position) \
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        room.id,
                        m.member,
                        m.profile,
                        m.gateway,
                        i64::try_from(position).unwrap_or(0)
                    ],
                )
                .map_err(sql)?;
            }
            Ok(())
        })
    }

    /// Room `id` with its members.
    ///
    /// # Errors
    ///
    /// `room.not_found`; store errors.
    pub fn room(&self, id: &str) -> Result<RoomRow, Error> {
        self.read(|conn| load(conn, id))?
            .ok_or_else(|| not_found(id))
    }

    /// Every room, by name.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn rooms(&self) -> Result<Vec<RoomRow>, Error> {
        let ids: Vec<String> = self.read(|conn| {
            let mut stmt = conn
                .prepare("SELECT id FROM rooms ORDER BY name")
                .map_err(sql)?;
            stmt.query_map([], |r| r.get(0))
                .map_err(sql)?
                .collect::<Result<_, _>>()
                .map_err(sql)
        })?;
        ids.iter().map(|id| self.room(id)).collect()
    }

    /// Renames room `id`, changes its picture or directive detection.
    ///
    /// # Errors
    ///
    /// `room.not_found`, `room.name_taken`; store errors.
    pub fn room_update(
        &self,
        id: &str,
        name: Option<&str>,
        picture: Option<&str>,
        detect: Option<bool>,
    ) -> Result<(), Error> {
        let now = now_ms();
        self.write(|tx| {
            if let Some(name) = name {
                let other: Option<String> = tx
                    .query_row(
                        "SELECT id FROM rooms WHERE name = ?1 AND id != ?2",
                        params![name, id],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(sql)?;
                if other.is_some() {
                    return Err(taken(name));
                }
            }
            let changed = tx
                .execute(
                    "UPDATE rooms SET name = COALESCE(?2, name), picture = COALESCE(?3, picture), \
                     detect_stop_directives = COALESCE(?4, detect_stop_directives), updated_at = ?5 \
                     WHERE id = ?1",
                    params![id, name, picture, detect.map(i64::from), now],
                )
                .map_err(sql)?;
            if changed == 0 {
                return Err(not_found(id));
            }
            Ok(())
        })
    }

    /// Deletes room `id` and its log.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn room_delete(&self, id: &str) -> Result<(), Error> {
        self.write(|tx| {
            for table in ["room_log", "room_members", "room_leases"] {
                tx.execute(&format!("DELETE FROM {table} WHERE room_id = ?1"), [id])
                    .map_err(sql)?;
            }
            tx.execute("DELETE FROM rooms WHERE id = ?1", [id])
                .map(drop)
                .map_err(sql)
        })
    }

    /// Appends one entry to room `id`'s log; its sequence number.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn room_append(
        &self,
        id: &str,
        kind: &str,
        author: Option<&str>,
        (text, data): (Option<&str>, Option<&str>),
    ) -> Result<u64, Error> {
        let now = now_ms();
        self.write(|tx| {
            let seq = last_seq(tx, id)? + 1;
            tx.execute(
                "INSERT INTO room_log (room_id, seq, kind, author, text, data, at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![id, i(seq), kind, author, text, data, now],
            )
            .map_err(sql)?;
            tx.execute(
                "UPDATE rooms SET latest_seq = MAX(latest_seq, ?2), updated_at = ?3 WHERE id = ?1",
                params![id, i(seq), now],
            )
            .map_err(sql)?;
            Ok(seq)
        })
    }

    /// Up to `limit` entries of room `id` after `after`.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn room_log(&self, id: &str, after: u64, limit: u32) -> Result<Vec<EntryRow>, Error> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT seq, kind, author, text, data, at FROM room_log \
                     WHERE room_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
                )
                .map_err(sql)?;
            stmt.query_map(params![id, i(after), i64::from(limit)], |row| {
                Ok(EntryRow {
                    seq: u(row.get(0)?),
                    kind: row.get(1)?,
                    author: row.get(2)?,
                    text: row.get(3)?,
                    data: row.get(4)?,
                    at: row.get(5)?,
                })
            })
            .map_err(sql)?
            .collect::<Result<_, _>>()
            .map_err(sql)
        })
    }

    /// The last entry this gateway holds of room `id`.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn room_last_seq(&self, id: &str) -> Result<u64, Error> {
        self.write(|tx| last_seq(tx, id))
    }

    /// Changes member `member` of room `id`.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn room_member(&self, id: &str, member: &str, change: &MemberChange) -> Result<(), Error> {
        let (state, reason) = change.state.clone().unzip();
        let reason = reason.flatten();
        let (clear_run, run) = match &change.pending_run {
            Some(run) => (1, run.clone()),
            None => (0, None),
        };
        self.write(|tx| {
            tx.execute(
                "UPDATE room_members SET held = COALESCE(?3, held), \
                 seen_seq = COALESCE(?4, seen_seq), state = COALESCE(?5, state), \
                 reason = CASE WHEN ?5 IS NULL THEN reason ELSE ?6 END, \
                 pending_run = CASE WHEN ?7 = 1 THEN ?8 ELSE pending_run END \
                 WHERE room_id = ?1 AND member = ?2",
                params![
                    id,
                    member,
                    change.held.map(i64::from),
                    change.seen_seq.map(i),
                    state,
                    reason,
                    clear_run,
                    run
                ],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// Sets or clears room `id`'s needs-you flag.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn room_needs_you(&self, id: &str, on: bool) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "UPDATE rooms SET needs_you = ?2 WHERE id = ?1",
                params![id, i64::from(on)],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// Takes or renews room `id`'s driver lease for `holder` until
    /// `ttl_ms` from now; `false` when another live holder has it.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn room_lease(&self, id: &str, holder: &str, ttl_ms: i64) -> Result<bool, Error> {
        let now = now_ms();
        self.write(|tx| {
            let changed = tx
                .execute(
                    "INSERT INTO room_leases (room_id, holder, expires_at) VALUES (?1, ?2, ?3) \
                     ON CONFLICT (room_id) DO UPDATE SET holder = ?2, expires_at = ?3 \
                     WHERE room_leases.holder = ?2 OR room_leases.expires_at < ?4",
                    params![id, holder, now + ttl_ms, now],
                )
                .map_err(sql)?;
            Ok(changed == 1)
        })
    }

    /// Gives up `holder`'s lease of room `id`.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn room_release(&self, id: &str, holder: &str) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "DELETE FROM room_leases WHERE room_id = ?1 AND holder = ?2",
                params![id, holder],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// Keeps `entries` of a replicated room (existing ones stay) and the
    /// authority's lineage and latest entry as it reported them.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn room_replicate(&self, room: &RoomRow, entries: &[EntryRow]) -> Result<(), Error> {
        if self.read(|conn| load(conn, &room.id))?.is_none() {
            self.room_create(room)?;
        }
        let now = now_ms();
        self.write(|tx| {
            for e in entries {
                tx.execute(
                    "INSERT OR IGNORE INTO room_log (room_id, seq, kind, author, text, data, at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![room.id, i(e.seq), e.kind, e.author, e.text, e.data, e.at],
                )
                .map_err(sql)?;
            }
            tx.execute(
                "UPDATE rooms SET authority_gateway = ?2, authority_epoch = ?3, latest_seq = ?4, \
                 updated_at = ?5 WHERE id = ?1 AND role = 'replica'",
                params![
                    room.id,
                    room.authority_gateway,
                    i(room.authority_epoch),
                    i(room.latest_seq),
                    now
                ],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// Records room `id`'s authority now: `role`, gateway and epoch.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn room_authority(
        &self,
        id: &str,
        role: &str,
        (gateway, epoch): (&str, u64),
    ) -> Result<(), Error> {
        let now = now_ms();
        self.write(|tx| {
            tx.execute(
                "UPDATE rooms SET role = ?2, authority_gateway = ?3, authority_epoch = ?4, \
                 updated_at = ?5 WHERE id = ?1",
                params![id, role, gateway, i(epoch), now],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// Issues a grant for `authority` to drive `member` in room `id`; the
    /// token (only its hash is kept).
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn room_grant(
        &self,
        id: &str,
        authority: &str,
        member: &str,
        scopes: &[String],
    ) -> Result<String, Error> {
        let mut bytes = [0u8; 24];
        getrandom::fill(&mut bytes)
            .map_err(|e| Error::new(&codes::STORE_IO, format!("no randomness: {e}")))?;
        let token = format!("rg_{}", crate::blobs::hex(&bytes));
        let now = now_ms();
        self.write(|tx| {
            tx.execute(
                "INSERT INTO room_grants (token_hash, room_id, authority, member, scopes, \
                 issued_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![hash(&token), id, authority, member, scopes.join(","), now],
            )
            .map(drop)
            .map_err(sql)
        })?;
        Ok(token)
    }

    /// The member and scopes `token` grants in room `id`, unless revoked.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn room_grant_of(
        &self,
        id: &str,
        token: &str,
    ) -> Result<Option<(String, Vec<String>)>, Error> {
        let found: Option<(String, String)> = self.read(|conn| {
            conn.query_row(
                "SELECT member, scopes FROM room_grants WHERE token_hash = ?1 AND room_id = ?2 \
                 AND revoked_at IS NULL",
                params![hash(token), id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(sql)
        })?;
        Ok(found.map(|(member, scopes)| (member, scopes.split(',').map(str::to_owned).collect())))
    }

    /// Revokes `authority`'s grants of room `id`.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn room_revoke(&self, id: &str, authority: &str) -> Result<(), Error> {
        let now = now_ms();
        self.write(|tx| {
            tx.execute(
                "UPDATE room_grants SET revoked_at = ?3 WHERE room_id = ?1 AND authority = ?2 \
                 AND revoked_at IS NULL",
                params![id, authority, now],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// Records or settles an authority run of a member of this home:
    /// `state` is `running`, `done` or `failed`.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn room_run(
        &self,
        run: &str,
        (room, member): (&str, &str),
        (state, reply, reason): (&str, Option<&str>, Option<&str>),
    ) -> Result<(), Error> {
        let now = now_ms();
        self.write(|tx| {
            tx.execute(
                "INSERT INTO room_member_runs (run_id, room_id, member, state, reply, reason, \
                 created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7) \
                 ON CONFLICT (run_id) DO UPDATE SET state = ?4, reply = ?5, reason = ?6, \
                 updated_at = ?7",
                params![run, room, member, state, reply, reason, now],
            )
            .map(drop)
            .map_err(sql)
        })
    }

    /// An authority run: its state, reply and reason.
    ///
    /// # Errors
    ///
    /// Store errors.
    pub fn room_run_state(&self, run: &str) -> Result<Option<RunState>, Error> {
        self.read(|conn| {
            conn.query_row(
                "SELECT state, reply, reason FROM room_member_runs WHERE run_id = ?1",
                [run],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(sql)
        })
    }
}

fn hash(token: &str) -> String {
    crate::blobs::hex(&Sha256::digest(token.as_bytes()))
}
