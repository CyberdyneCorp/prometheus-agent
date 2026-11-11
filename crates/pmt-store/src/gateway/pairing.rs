//! Pairing requests and approved senders (spec: messaging-gateway,
//! Documented pairing flow). A code is shown once to the sender and kept
//! only as a salted SHA-256; approvals that fail are counted per platform
//! and five within an hour lock that platform's pairing for an hour.

use jiff::{SignedDuration, Timestamp};
use pmt_core::error::{Error, codes};
use rusqlite::{OptionalExtension, Row, params};
use sha2::{Digest, Sha256};

use super::{millis, timestamp};
use crate::error::sql;
use crate::store::Store;

/// The characters a code is drawn from (no `0`, `1`, `I`, `O`).
pub const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
/// Characters in a code.
pub const CODE_LEN: usize = 8;
/// How long a code can be approved.
pub const CODE_TTL: SignedDuration = SignedDuration::from_secs(3600);
/// Least time between two codes for one sender.
pub const REQUEST_INTERVAL: SignedDuration = SignedDuration::from_secs(600);
/// Pending codes per platform.
pub const MAX_PENDING: usize = 3;
/// Failed approvals that lock a platform's pairing.
pub const LOCK_AFTER: usize = 5;
/// How long a lock lasts, and the window failures are counted in.
pub const LOCK_FOR: SignedDuration = SignedDuration::from_secs(3600);

/// Who asks to pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Requester<'a> {
    /// Platform name.
    pub platform: &'a str,
    /// Canonical sender id.
    pub sender_id: &'a str,
    /// The DM to answer in.
    pub chat_id: &'a str,
    /// Display name.
    pub sender_name: Option<&'a str>,
}

/// What a pairing request got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Requested {
    /// A new code, to show the sender once.
    Issued(String),
    /// The sender got a code less than ten minutes ago.
    TooSoon,
    /// The platform already has three pending codes.
    TooManyPending,
    /// Pairing is locked for the platform.
    Locked(Timestamp),
    /// The sender is already approved.
    AlreadyApproved,
}

/// A pending or approved entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pairing {
    /// Platform name.
    pub platform: String,
    /// Canonical sender id.
    pub sender_id: String,
    /// The sender's DM.
    pub chat_id: String,
    /// Display name.
    pub sender_name: Option<String>,
    /// Approved (else pending).
    pub approved: bool,
    /// When the code was issued.
    pub requested_at: Timestamp,
    /// When a pending code expires.
    pub expires_at: Option<Timestamp>,
    /// When it was approved.
    pub approved_at: Option<Timestamp>,
}

/// Upper-cases a typed code and drops everything outside the alphabet
/// (`"abcd efgh"` is `ABCDEFGH`).
#[must_use]
pub fn normalize_code(typed: &str) -> String {
    typed
        .chars()
        .map(|c| c.to_ascii_uppercase())
        .filter(|c| c.is_ascii() && CODE_ALPHABET.contains(&(*c as u8)))
        .collect()
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).expect("the OS random generator is available");
    bytes
}

fn new_code() -> String {
    let bytes: [u8; CODE_LEN] = random_bytes();
    // 32 symbols: the low five bits of each byte pick one without bias.
    bytes
        .iter()
        .map(|b| char::from(CODE_ALPHABET[usize::from(b & 0x1f)]))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn hash(salt: &str, code: &str) -> String {
    hex(&Sha256::digest(format!("{salt}:{code}").as_bytes()))
}

/// One stored pairing entry, verbatim (see [`Store::pairing_row`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingRow {
    chat_id: String,
    sender_name: Option<String>,
    state: String,
    code_salt: Option<String>,
    code_hash: Option<String>,
    requested_at: i64,
    expires_at: Option<i64>,
    approved_at: Option<i64>,
}

fn pairing_from_row(row: &Row<'_>) -> rusqlite::Result<Pairing> {
    Ok(Pairing {
        platform: row.get(0)?,
        sender_id: row.get(1)?,
        chat_id: row.get(2)?,
        sender_name: row.get(3)?,
        approved: row.get::<_, String>(4)? == "approved",
        requested_at: timestamp(row.get(5)?),
        expires_at: row.get::<_, Option<i64>>(6)?.map(timestamp),
        approved_at: row.get::<_, Option<i64>>(7)?.map(timestamp),
    })
}

const COLUMNS: &str =
    "platform, sender_id, chat_id, sender_name, state, requested_at, expires_at, approved_at";

/// When the platform's pairing is locked until, if it is at `now`.
fn locked_until(
    conn: &rusqlite::Connection,
    platform: &str,
    now: Timestamp,
) -> Result<Option<Timestamp>, Error> {
    let since = millis(now) - LOCK_FOR.as_secs() * 1000;
    let mut stmt = conn
        .prepare(
            "SELECT at FROM gateway_pairing_failures WHERE platform = ?1 AND at > ?2 \
             ORDER BY at DESC",
        )
        .map_err(sql)?;
    let recent: Vec<i64> = stmt
        .query_map(params![platform, since], |row| row.get(0))
        .map_err(sql)?
        .collect::<Result<_, _>>()
        .map_err(sql)?;
    Ok((recent.len() >= LOCK_AFTER).then(|| timestamp(recent[0]) + LOCK_FOR))
}

fn locked_error(platform: &str, until: Timestamp) -> Error {
    Error::new(
        &codes::GATEWAY_PAIRING_LOCKED,
        format!(
            "pairing locked for {platform} until {}",
            until.strftime("%Y-%m-%d %H:%M:%S UTC")
        ),
    )
}

impl Store {
    /// Issues a pairing code to an unauthorized sender, within the
    /// request rate, pending and lock limits.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    ///
    /// # Panics
    ///
    /// When the OS random generator is unavailable.
    pub fn request_pairing(&self, who: &Requester<'_>, now: Timestamp) -> Result<Requested, Error> {
        let code = new_code();
        let salt = hex(&random_bytes::<16>());
        self.write(|tx| {
            if let Some(until) = locked_until(tx, who.platform, now)? {
                return Ok(Requested::Locked(until));
            }
            let at = millis(now);
            tx.execute(
                "DELETE FROM gateway_pairing WHERE state = 'pending' AND expires_at <= ?1",
                [at],
            )
            .map_err(sql)?;
            let existing: Option<(String, i64)> = tx
                .query_row(
                    "SELECT state, requested_at FROM gateway_pairing \
                     WHERE platform = ?1 AND sender_id = ?2",
                    params![who.platform, who.sender_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(sql)?;
            match existing {
                Some((state, _)) if state == "approved" => return Ok(Requested::AlreadyApproved),
                Some((_, requested)) if at - requested < REQUEST_INTERVAL.as_secs() * 1000 => {
                    return Ok(Requested::TooSoon);
                }
                _ => {}
            }
            let pending: i64 = tx
                .query_row(
                    "SELECT count(*) FROM gateway_pairing WHERE platform = ?1 \
                     AND state = 'pending' AND sender_id != ?2",
                    params![who.platform, who.sender_id],
                    |row| row.get(0),
                )
                .map_err(sql)?;
            if usize::try_from(pending).unwrap_or(usize::MAX) >= MAX_PENDING {
                return Ok(Requested::TooManyPending);
            }
            tx.execute(
                "INSERT INTO gateway_pairing (platform, sender_id, chat_id, sender_name, state, \
                 code_salt, code_hash, requested_at, expires_at) \
                 VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?7, ?8) \
                 ON CONFLICT (platform, sender_id) DO UPDATE SET chat_id = excluded.chat_id, \
                 sender_name = excluded.sender_name, code_salt = excluded.code_salt, \
                 code_hash = excluded.code_hash, requested_at = excluded.requested_at, \
                 expires_at = excluded.expires_at",
                params![
                    who.platform,
                    who.sender_id,
                    who.chat_id,
                    who.sender_name,
                    salt,
                    hash(&salt, &code),
                    at,
                    at + CODE_TTL.as_secs() * 1000
                ],
            )
            .map_err(sql)?;
            Ok(Requested::Issued(code.clone()))
        })
    }

    /// Approves the pending request whose code `typed` is (formatting is
    /// tolerated). A code that matches nothing counts as a failure.
    ///
    /// # Errors
    ///
    /// `gateway.pairing_locked` while locked (even for a valid code),
    /// `gateway.pairing_unknown` for no match, `store.sqlite`.
    pub fn approve_pairing(
        &self,
        platform: &str,
        typed: &str,
        now: Timestamp,
    ) -> Result<Pairing, Error> {
        let code = normalize_code(typed);
        self.write(|tx| {
            if let Some(until) = locked_until(tx, platform, now)? {
                return Ok(Err(locked_error(platform, until)));
            }
            let at = millis(now);
            let mut stmt = tx
                .prepare(
                    "SELECT sender_id, code_salt, code_hash FROM gateway_pairing \
                     WHERE platform = ?1 AND state = 'pending' AND expires_at > ?2",
                )
                .map_err(sql)?;
            let pending: Vec<(String, String, String)> = stmt
                .query_map(params![platform, at], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .map_err(sql)?
                .collect::<Result<_, _>>()
                .map_err(sql)?;
            drop(stmt);
            let found = pending
                .into_iter()
                .find(|(_, salt, digest)| code.len() == CODE_LEN && hash(salt, &code) == *digest);
            let Some((sender, _, _)) = found else {
                tx.execute(
                    "INSERT INTO gateway_pairing_failures (platform, at) VALUES (?1, ?2)",
                    params![platform, at],
                )
                .map_err(sql)?;
                return Ok(Err(Error::new(
                    &codes::GATEWAY_PAIRING_UNKNOWN,
                    format!("no pending {platform} pairing request has code {code}"),
                )));
            };
            tx.execute(
                "UPDATE gateway_pairing SET state = 'approved', code_salt = NULL, \
                 code_hash = NULL, expires_at = NULL, approved_at = ?3 \
                 WHERE platform = ?1 AND sender_id = ?2",
                params![platform, sender, at],
            )
            .map_err(sql)?;
            tx.query_row(
                &format!(
                    "SELECT {COLUMNS} FROM gateway_pairing WHERE platform = ?1 AND sender_id = ?2"
                ),
                params![platform, sender],
                pairing_from_row,
            )
            .map(Ok)
            .map_err(sql)
        })?
    }

    /// Removes a pending request by its code (`pairing deny`).
    ///
    /// # Errors
    ///
    /// `gateway.pairing_unknown`, `store.sqlite`.
    pub fn deny_pairing(
        &self,
        platform: &str,
        typed: &str,
        now: Timestamp,
    ) -> Result<Pairing, Error> {
        let code = normalize_code(typed);
        let entries = self.pairings(Some(platform))?;
        let salts = self.read(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT sender_id, code_salt, code_hash FROM gateway_pairing \
                     WHERE platform = ?1 AND state = 'pending' AND expires_at > ?2",
                )
                .map_err(sql)?;
            stmt.query_map(params![platform, millis(now)], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(sql)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(sql)
        })?;
        let sender = salts
            .into_iter()
            .find(|(_, salt, digest)| hash(salt, &code) == *digest)
            .map(|(sender, _, _)| sender)
            .ok_or_else(|| {
                Error::new(
                    &codes::GATEWAY_PAIRING_UNKNOWN,
                    format!("no pending {platform} pairing request has code {code}"),
                )
            })?;
        self.remove_pairing(platform, &sender)?;
        entries
            .into_iter()
            .find(|p| p.sender_id == sender)
            .ok_or_else(|| not_found(platform, &sender))
    }

    /// Removes an approved sender (`pairing revoke`).
    ///
    /// # Errors
    ///
    /// `gateway.pairing_not_found`, `store.sqlite`.
    pub fn revoke_pairing(&self, platform: &str, sender_id: &str) -> Result<(), Error> {
        let removed = self.write(|tx| {
            tx.execute(
                "DELETE FROM gateway_pairing WHERE platform = ?1 AND sender_id = ?2 \
                 AND state = 'approved'",
                params![platform, sender_id],
            )
            .map_err(sql)
        })?;
        if removed == 0 {
            return Err(not_found(platform, sender_id));
        }
        Ok(())
    }

    fn remove_pairing(&self, platform: &str, sender_id: &str) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "DELETE FROM gateway_pairing WHERE platform = ?1 AND sender_id = ?2",
                params![platform, sender_id],
            )
            .map_err(sql)?;
            Ok(())
        })
    }

    /// Records `sender_id` on `platform` as approved without a code (a
    /// pairing approved in another agent, imported by a migration); an
    /// existing entry of the sender becomes approved. Returns whether it
    /// was not approved before.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn adopt_pairing(
        &self,
        platform: &str,
        sender_id: &str,
        sender_name: Option<&str>,
        approved_at: Timestamp,
    ) -> Result<bool, Error> {
        let at = millis(approved_at);
        self.write(|tx| {
            let before: Option<String> = tx
                .query_row(
                    "SELECT state FROM gateway_pairing WHERE platform = ?1 AND sender_id = ?2",
                    params![platform, sender_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(sql)?;
            if before.as_deref() == Some("approved") {
                return Ok(false);
            }
            tx.execute(
                "INSERT INTO gateway_pairing (platform, sender_id, chat_id, sender_name, state, \
                 requested_at, approved_at) VALUES (?1, ?2, ?2, ?3, 'approved', ?4, ?4) \
                 ON CONFLICT (platform, sender_id) DO UPDATE SET state = 'approved', \
                 code_salt = NULL, code_hash = NULL, expires_at = NULL, approved_at = ?4",
                params![platform, sender_id, sender_name, at],
            )
            .map_err(sql)?;
            Ok(true)
        })
    }

    /// The whole entry of `sender_id` on `platform` as stored, to put
    /// back with [`Store::restore_pairing`] (a migration undoing the
    /// senders it adopted).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn pairing_row(
        &self,
        platform: &str,
        sender_id: &str,
    ) -> Result<Option<PairingRow>, Error> {
        self.read(|conn| {
            conn.query_row(
                "SELECT chat_id, sender_name, state, code_salt, code_hash, requested_at, \
                 expires_at, approved_at FROM gateway_pairing WHERE platform = ?1 AND sender_id = ?2",
                params![platform, sender_id],
                |row| {
                    Ok(PairingRow {
                        chat_id: row.get(0)?,
                        sender_name: row.get(1)?,
                        state: row.get(2)?,
                        code_salt: row.get(3)?,
                        code_hash: row.get(4)?,
                        requested_at: row.get(5)?,
                        expires_at: row.get(6)?,
                        approved_at: row.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(sql)
        })
    }

    /// Puts the entry of `sender_id` on `platform` back as `row` held it
    /// (`None`: no entry).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn restore_pairing(
        &self,
        platform: &str,
        sender_id: &str,
        row: Option<&PairingRow>,
    ) -> Result<(), Error> {
        self.write(|tx| {
            tx.execute(
                "DELETE FROM gateway_pairing WHERE platform = ?1 AND sender_id = ?2",
                params![platform, sender_id],
            )
            .map_err(sql)?;
            if let Some(r) = row {
                tx.execute(
                    "INSERT INTO gateway_pairing (platform, sender_id, chat_id, sender_name, state, \
                     code_salt, code_hash, requested_at, expires_at, approved_at) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                    params![
                        platform,
                        sender_id,
                        r.chat_id,
                        r.sender_name,
                        r.state,
                        r.code_salt,
                        r.code_hash,
                        r.requested_at,
                        r.expires_at,
                        r.approved_at
                    ],
                )
                .map_err(sql)?;
            }
            Ok(())
        })
    }

    /// Pending (unexpired) and approved entries, of one platform or all.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn pairings(&self, platform: Option<&str>) -> Result<Vec<Pairing>, Error> {
        let now = millis(Timestamp::now());
        self.read(|conn| {
            let mut stmt = conn
                .prepare(&format!(
                    "SELECT {COLUMNS} FROM gateway_pairing WHERE (?1 IS NULL OR platform = ?1) \
                     AND (state = 'approved' OR expires_at > ?2) \
                     ORDER BY platform, state, requested_at"
                ))
                .map_err(sql)?;
            stmt.query_map(params![platform, now], pairing_from_row)
                .map_err(sql)?
                .collect::<Result<_, _>>()
                .map_err(sql)
        })
    }

    /// Whether `sender_id` is an approved sender on `platform`.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn is_paired(&self, platform: &str, sender_id: &str) -> Result<bool, Error> {
        self.read(|conn| {
            conn.query_row(
                "SELECT 1 FROM gateway_pairing WHERE platform = ?1 AND sender_id = ?2 \
                 AND state = 'approved'",
                params![platform, sender_id],
                |_| Ok(()),
            )
            .optional()
            .map(|found| found.is_some())
            .map_err(sql)
        })
    }
}

fn not_found(platform: &str, sender: &str) -> Error {
    Error::new(
        &codes::GATEWAY_PAIRING_NOT_FOUND,
        format!("no {platform} pairing entry for sender {sender}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_codes_are_normalized() {
        assert_eq!(normalize_code("abcd efgh"), "ABCDEFGH");
        assert_eq!(normalize_code("AB-CD_EF.GH"), "ABCDEFGH");
    }

    #[test]
    fn codes_use_the_alphabet() {
        for _ in 0..50 {
            let code = new_code();
            assert_eq!(code.len(), CODE_LEN);
            assert!(code.bytes().all(|b| CODE_ALPHABET.contains(&b)), "{code}");
        }
    }
}
