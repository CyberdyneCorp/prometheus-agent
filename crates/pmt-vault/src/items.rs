//! What the vault holds: logins, payment cards and addresses. Labels,
//! origins and identifiers are metadata ([`Meta`]); passwords, card
//! numbers, CVCs and TOTP seeds never leave the vault except into a page.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

/// The kind of an item; its handle starts with the kind's prefix, so a
/// handle alone says whether filling it needs an approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A website login.
    Login,
    /// A payment card.
    Card,
    /// A postal address.
    Address,
}

impl Kind {
    /// `login`, `card` or `address`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Login => "login",
            Self::Card => "card",
            Self::Address => "address",
        }
    }

    /// The kind a handle names (`card_1a2b...` is a card).
    #[must_use]
    pub fn of_handle(handle: &str) -> Option<Self> {
        let (prefix, rest) = handle.split_once('_')?;
        if rest.is_empty() {
            return None;
        }
        [Self::Login, Self::Card, Self::Address]
            .into_iter()
            .find(|k| k.as_str() == prefix)
    }

    /// A new random handle of this kind.
    ///
    /// # Panics
    ///
    /// When the OS random generator is unavailable.
    #[must_use]
    pub fn new_handle(self) -> String {
        let mut bytes = [0u8; 6];
        getrandom::fill(&mut bytes).expect("the OS random generator is available");
        bytes
            .iter()
            .fold(format!("{}_", self.as_str()), |mut out, b| {
                let _ = write!(out, "{b:02x}");
                out
            })
    }
}

/// A login.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Login {
    /// The origins it may be filled on (`https://github.com`), exactly.
    pub origins: Vec<String>,
    /// The user name or email.
    pub identifier: String,
    /// The password.
    pub password: String,
    /// A base32 TOTP seed, when the site uses one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totp: Option<String>,
}

/// A payment card.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Card {
    /// The cardholder's name.
    pub holder: String,
    /// The card number.
    pub number: String,
    /// `MM/YY`.
    pub expiry: String,
    /// The security code.
    pub cvc: String,
}

/// A postal address.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    /// Full name.
    pub name: String,
    /// Street and number.
    pub line1: String,
    /// Apartment, suite...
    #[serde(default)]
    pub line2: String,
    /// City.
    pub city: String,
    /// State or region.
    #[serde(default)]
    pub region: String,
    /// Postal code.
    pub postal_code: String,
    /// Country.
    pub country: String,
}

/// What an item holds.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Secret {
    /// A login.
    Login(Login),
    /// A card.
    Card(Card),
    /// An address.
    Address(Address),
}

/// One stored item.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// `login_...`, `card_...` or `address_...`.
    pub handle: String,
    /// A name the user chose.
    pub label: String,
    /// The values.
    #[serde(flatten)]
    pub secret: Secret,
}

impl std::fmt::Debug for Item {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Item")
            .field("meta", &self.meta())
            .finish_non_exhaustive()
    }
}

/// An item's metadata: what lists, the model and RPC clients see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Meta {
    /// The handle.
    pub handle: String,
    /// The kind.
    pub kind: Kind,
    /// The label.
    pub label: String,
    /// The origins a login fills on (cards and addresses: any page).
    pub origins: Vec<String>,
    /// A login's identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    /// Whether a login has a TOTP seed.
    pub totp: bool,
}

impl Item {
    /// The item's kind.
    #[must_use]
    pub fn kind(&self) -> Kind {
        match self.secret {
            Secret::Login(_) => Kind::Login,
            Secret::Card(_) => Kind::Card,
            Secret::Address(_) => Kind::Address,
        }
    }

    /// Its metadata.
    #[must_use]
    pub fn meta(&self) -> Meta {
        let (origins, identifier, totp) = match &self.secret {
            Secret::Login(login) => (
                login.origins.clone(),
                Some(login.identifier.clone()),
                login.totp.is_some(),
            ),
            Secret::Card(_) | Secret::Address(_) => (Vec::new(), None, false),
        };
        Meta {
            handle: self.handle.clone(),
            kind: self.kind(),
            label: self.label.clone(),
            origins,
            identifier,
            totp,
        }
    }

    /// The values a fill types, by field role, which become redactions.
    #[must_use]
    pub fn secret_values(&self) -> Vec<&str> {
        match &self.secret {
            Secret::Login(login) => vec![login.password.as_str()],
            Secret::Card(card) => vec![card.number.as_str(), card.cvc.as_str()],
            Secret::Address(_) => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Kind;

    #[test]
    fn handles_name_their_kind() {
        let handle = Kind::Card.new_handle();
        assert!(
            handle.starts_with("card_") && handle.len() == 17,
            "{handle}"
        );
        assert_eq!(Kind::of_handle(&handle), Some(Kind::Card));
        assert_eq!(Kind::of_handle("login_00ff"), Some(Kind::Login));
        assert_eq!(Kind::of_handle("cardx_1"), None);
        assert_eq!(Kind::of_handle("card_"), None);
    }
}
