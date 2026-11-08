//! Provider integration tests against local mock servers: recorded stream
//! fixtures per wire format, prompt caching markers and ledger rows, and
//! fault injection for retries and the fallback chain.

mod accounts;
mod auth;
mod auxiliary;
mod borrowed;
mod caching;
mod faults;
mod idp;
mod local;
mod minting;
mod mock;
mod pools;
mod portals;
mod refresh;
mod routing;
mod server_compaction;
mod session;
mod support;
mod wire;
