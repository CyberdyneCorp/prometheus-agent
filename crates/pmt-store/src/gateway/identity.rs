//! Cross-platform identity links (spec: messaging-gateway, Cross-platform
//! identity linking): `/link` hands an authorized account a 6-digit code
//! valid for 600 seconds; the same person sending `/link <code>` from an
//! account on another platform links both to one `person_id`. An account
//! belongs to at most one person: linking an account that already has a
//! person moves that person's accounts over.

use jiff::{SignedDuration, Timestamp};
use pmt_core::error::Error;
use rusqlite::{OptionalExtension, params};

use super::millis;
use crate::error::sql;
use crate::store::Store;

/// How long a link code is valid.
pub const CODE_TTL: SignedDuration = SignedDuration::from_secs(600);

/// One linked account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityLink {
    /// Its platform.
    pub platform: String,
    /// Its sender id there.
    pub sender_id: String,
    /// The person it belongs to.
    pub person_id: String,
    /// When it was linked.
    pub linked_at: Timestamp,
}

/// What redeeming a code did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Redeemed {
    /// Both accounts now belong to this person.
    Linked(String),
    /// The code ran out (and is gone).
    Expired,
    /// No such code, or one issued to this very account.
    Unknown,
}

fn person_of(
    tx: &rusqlite::Connection,
    platform: &str,
    sender: &str,
) -> Result<Option<String>, Error> {
    tx.query_row(
        "SELECT person_id FROM gateway_identity_links WHERE platform = ?1 AND sender_id = ?2",
        params![platform, sender],
        |row| row.get(0),
    )
    .optional()
    .map_err(sql)
}

impl Store {
    /// A fresh code for the account (its earlier code is dropped).
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn link_code(
        &self,
        platform: &str,
        sender_id: &str,
        now: Timestamp,
    ) -> Result<String, Error> {
        let expires = millis(now.checked_add(CODE_TTL).unwrap_or(now));
        // Codes that ran out are kept for one more period, so a late
        // `/link <code>` is told it expired.
        let forgotten = millis(now.checked_sub(CODE_TTL).unwrap_or(now));
        self.write(|tx| {
            tx.execute(
                "DELETE FROM gateway_link_codes WHERE (platform = ?1 AND sender_id = ?2) \
                 OR expires_at < ?3",
                params![platform, sender_id, forgotten],
            )
            .map_err(sql)?;
            loop {
                let code = format!("{:06}", random_below(1_000_000));
                let inserted = tx
                    .execute(
                        "INSERT OR IGNORE INTO gateway_link_codes (code, platform, sender_id, \
                         expires_at) VALUES (?1, ?2, ?3, ?4)",
                        params![code, platform, sender_id, expires],
                    )
                    .map_err(sql)?;
                if inserted == 1 {
                    return Ok(code);
                }
            }
        })
    }

    /// Redeems `code` from the account `(platform, sender_id)`.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn redeem_link_code(
        &self,
        code: &str,
        (platform, sender_id): (&str, &str),
        now: Timestamp,
    ) -> Result<Redeemed, Error> {
        self.write(|tx| {
            let issued: Option<(String, String, i64)> = tx
                .query_row(
                    "SELECT platform, sender_id, expires_at FROM gateway_link_codes WHERE code = ?1",
                    [code],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(sql)?;
            let Some((from_platform, from_sender, expires)) = issued else {
                return Ok(Redeemed::Unknown);
            };
            if from_platform == platform && from_sender == sender_id {
                return Ok(Redeemed::Unknown);
            }
            tx.execute("DELETE FROM gateway_link_codes WHERE code = ?1", [code])
                .map_err(sql)?;
            if expires < millis(now) {
                return Ok(Redeemed::Expired);
            }
            let person = match person_of(tx, &from_platform, &from_sender)? {
                Some(person) => person,
                None => format!("per_{}", ulid::Ulid::generate().to_string().to_lowercase()),
            };
            if let Some(old) = person_of(tx, platform, sender_id)?
                && old != person
            {
                tx.execute(
                    "UPDATE gateway_identity_links SET person_id = ?1 WHERE person_id = ?2",
                    params![person, old],
                )
                .map_err(sql)?;
            }
            for (p, s) in [(from_platform.as_str(), from_sender.as_str()), (platform, sender_id)] {
                tx.execute(
                    "INSERT INTO gateway_identity_links (platform, sender_id, person_id, linked_at) \
                     VALUES (?1, ?2, ?3, ?4) ON CONFLICT (platform, sender_id) DO UPDATE SET \
                     person_id = excluded.person_id",
                    params![p, s, person, millis(now)],
                )
                .map_err(sql)?;
            }
            Ok(Redeemed::Linked(person))
        })
    }

    /// The person the account belongs to, if linked.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn person_of(&self, platform: &str, sender_id: &str) -> Result<Option<String>, Error> {
        self.read(|conn| person_of(conn, platform, sender_id))
    }

    /// Every linked account, by person then platform.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn identity_links(&self) -> Result<Vec<IdentityLink>, Error> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT platform, sender_id, person_id, linked_at FROM gateway_identity_links \
                     ORDER BY person_id, platform, sender_id",
                )
                .map_err(sql)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(IdentityLink {
                        platform: row.get(0)?,
                        sender_id: row.get(1)?,
                        person_id: row.get(2)?,
                        linked_at: super::timestamp(row.get(3)?),
                    })
                })
                .map_err(sql)?;
            rows.collect::<Result<_, _>>().map_err(sql)
        })
    }

    /// Unlinks the accounts on `platform` of `person_id`, or, with a
    /// sender, that one account; a person left with one account is
    /// dissolved. Returns the accounts removed.
    ///
    /// # Errors
    ///
    /// `store.sqlite`.
    pub fn unlink_identity(
        &self,
        person_id: &str,
        platform: &str,
        sender_id: Option<&str>,
    ) -> Result<usize, Error> {
        self.write(|tx| {
            let removed = tx
                .execute(
                    "DELETE FROM gateway_identity_links WHERE person_id = ?1 AND platform = ?2 \
                     AND (?3 IS NULL OR sender_id = ?3)",
                    params![person_id, platform, sender_id],
                )
                .map_err(sql)?;
            let left: i64 = tx
                .query_row(
                    "SELECT count(*) FROM gateway_identity_links WHERE person_id = ?1",
                    [person_id],
                    |row| row.get(0),
                )
                .map_err(sql)?;
            if left < 2 {
                tx.execute(
                    "DELETE FROM gateway_identity_links WHERE person_id = ?1",
                    [person_id],
                )
                .map_err(sql)?;
            }
            Ok(removed)
        })
    }
}

/// A uniform random number below `bound`.
fn random_below(bound: u32) -> u32 {
    let mut bytes = [0u8; 4];
    loop {
        getrandom::fill(&mut bytes).expect("the OS random generator is available");
        let n = u32::from_le_bytes(bytes);
        // Rejection sampling keeps the codes uniform.
        if n < u32::MAX - (u32::MAX % bound) {
            return n % bound;
        }
    }
}
