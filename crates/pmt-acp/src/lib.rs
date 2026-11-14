//! The ACP adapter (spec: acp-adapter): `prometheus acp` exposes the agent
//! to editors (Zed, VS Code, `JetBrains` and other Agent Client Protocol
//! clients) as newline-delimited JSON-RPC on stdio.
//!
//! The adapter is a viewer of the profile's gateway daemon, never an
//! execution owner: an ACP session is a daemon session (created with
//! source `acp`), prompts are daemon admissions, approvals and clarify
//! questions are the daemon's server requests shown as editor cards, and
//! the session's events come back as `session/update`s. Closing the
//! editor only detaches; the daemon keeps the session and any running
//! turn, and every other surface sees the same session.
//!
//! - [`serve`] runs the server over any reader and writer (the binary
//!   hands it stdin and a duplicate of stdout);
//! - [`Connector`] makes the daemon connection (ticket for the launch
//!   profile, `rpc.hello` as an `acp` client);
//! - [`Auth`] and [`Fetcher`] give it the profile's credentials and the
//!   egress-controlled image fetcher.

mod adapter;
mod bridge;
mod content;
mod frames;
mod kept;
mod link;
mod options;
mod project;
mod prompting;
mod sessions;
mod slash;

pub use adapter::{Auth, AuthMethod, Launch, Options, serve};
pub use content::Fetcher;
pub use link::{ConnectFuture, Connector};
