//! The credential vault (spec: security-boundaries, Credential vault
//! storage; `browser_vault` fills by handle; Vault codes, cards and
//! addresses).
//!
//! Each profile keeps logins, payment cards and addresses in
//! `<profile>/vault/vault.enc`, sealed with a key held in `vault.key` or
//! the OS keychain ([`key`]). Only metadata ([`items::Meta`]) ever leaves
//! this crate for the model or a client: secret values go from the vault
//! straight into a page by the browser tools.

pub mod items;
pub mod key;
pub mod origin;
pub mod store;
pub mod totp;

pub use items::{Address, Card, Item, Kind, Login, Meta, Secret};
pub use key::Location;
pub use store::Vault;
