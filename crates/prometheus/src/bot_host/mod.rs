//! Bot Mode (spec: bot-mode): a bot is exactly a profile. This module
//! holds what every surface shares: the bot metadata kept in the
//! profile's store, the roster and how a `message_agent` target resolves,
//! the sending side of `message_agent`, the protocol of canonical Bot
//! Chats with its turn notices, typed failure reasons, and creating,
//! duplicating and deleting bots. The daemon runs deliveries and rooms
//! (`daemon_cmd::host::bots`).

pub mod create;
pub mod meta;
pub mod protocol;
pub mod reasons;
pub mod rooms;
pub mod roster;
pub mod send;
