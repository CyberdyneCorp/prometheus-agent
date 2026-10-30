//! Typed ids: sessions `ses_<ULID>` and messages `msg_<ULID>` (design.md,
//! Shared contracts).
//!
//! Each id is its own type, so a message id cannot be passed where a session
//! id is expected. Ids made by one process are strictly increasing (a
//! monotonic ULID generator), so sorting ids sorts by creation order.

use std::fmt;
use std::str::FromStr;
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use ulid::{Generator, Ulid};

use crate::error::{Error, codes};

/// Length of a ULID in its canonical Crockford base-32 text form.
const ULID_LEN: usize = 26;

static GENERATOR: Mutex<Generator> = Mutex::new(Generator::new());

/// A fresh ULID, greater than every ULID this process generated before.
fn next_ulid() -> Ulid {
    // The generator holds only the last id, so a poisoned lock is still sound.
    let mut generator = GENERATOR.lock().unwrap_or_else(PoisonError::into_inner);
    // Overflow needs 2^80 ids in one millisecond; moving into the next
    // millisecond keeps the sequence increasing.
    generator
        .generate()
        .unwrap_or_else(ulid::Overflow::commit_overflow_random)
}

/// Parses `<prefix>_<ULID>`, accepting only the canonical upper-case form so
/// ids compare equal as strings wherever they are stored.
fn parse_prefixed(prefix: &str, text: &str) -> Result<Ulid, Error> {
    let malformed = || {
        Error::new(
            &codes::ID_MALFORMED,
            format!("malformed id '{text}': expected '{prefix}_' followed by a 26-character ULID"),
        )
    };
    let body = text
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('_'))
        .ok_or_else(malformed)?;
    if body.len() != ULID_LEN {
        return Err(malformed());
    }
    let ulid = Ulid::from_string(body).map_err(|_| malformed())?;
    if ulid.to_string() != body {
        return Err(malformed());
    }
    Ok(ulid)
}

macro_rules! prefixed_id {
    ($(#[$attr:meta])* $name:ident, $prefix:literal) => {
        $(#[$attr])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(Ulid);

        impl $name {
            /// The text prefix before the underscore.
            pub const PREFIX: &'static str = $prefix;

            /// A new id, greater than every id of this type made earlier by this process.
            #[must_use]
            pub fn new() -> Self {
                Self(next_ulid())
            }

            /// Parses the canonical text form.
            ///
            /// # Errors
            ///
            /// `id.malformed` when the prefix, length or ULID body is wrong.
            pub fn parse(text: &str) -> Result<Self, Error> {
                parse_prefixed(Self::PREFIX, text).map(Self)
            }

            /// The underlying ULID (its timestamp is the creation time).
            #[must_use]
            pub fn ulid(self) -> Ulid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}_{}", Self::PREFIX, self.0)
            }
        }

        impl FromStr for $name {
            type Err = Error;
            fn from_str(text: &str) -> Result<Self, Error> {
                Self::parse(text)
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let text = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
                Self::parse(&text).map_err(serde::de::Error::custom)
            }
        }
    };
}

prefixed_id!(
    /// A session id, `ses_<ULID>`.
    SessionId,
    "ses"
);

prefixed_id!(
    /// A message id, `msg_<ULID>`.
    MessageId,
    "msg"
);

#[cfg(test)]
mod tests {
    use super::{MessageId, SessionId};
    use crate::error::ErrorKind;

    #[test]
    fn ids_have_their_prefix_and_a_ulid() {
        let ses = SessionId::new().to_string();
        let msg = MessageId::new().to_string();
        assert!(ses.starts_with("ses_") && ses.len() == 30, "{ses}");
        assert!(msg.starts_with("msg_") && msg.len() == 30, "{msg}");
    }

    #[test]
    fn text_form_round_trips() {
        let id = SessionId::new();
        assert_eq!(SessionId::parse(&id.to_string()).unwrap(), id);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(serde_json::from_str::<SessionId>(&json).unwrap(), id);
    }

    #[test]
    fn ids_from_one_process_are_strictly_increasing() {
        let ids: Vec<_> = (0..1000).map(|_| MessageId::new()).collect();
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
        assert!(ids.windows(2).all(|w| w[0].to_string() < w[1].to_string()));
    }

    #[test]
    fn wrong_prefix_or_shape_is_rejected() {
        let msg = MessageId::new().to_string();
        let body = &msg[4..];
        for bad in [
            msg.clone(),                                 // message id where a session id is expected
            format!("ses{body}"),                        // missing underscore
            format!("ses_{}", &body[1..]),               // too short
            format!("ses_{body}0"),                      // too long
            format!("ses_{}", body.to_lowercase()),      // not canonical
            "ses_UUUUUUUUUUUUUUUUUUUUUUUUUU".to_owned(), // not Crockford base-32
            String::new(),
        ] {
            let err = SessionId::parse(&bad).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::InvalidInput, "{bad}");
            assert_eq!(err.code().code, "id.malformed");
        }
    }

    #[test]
    fn deserializing_a_bad_id_fails() {
        assert!(serde_json::from_str::<SessionId>("\"ses_nope\"").is_err());
    }
}
