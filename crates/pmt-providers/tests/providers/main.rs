//! Provider integration tests against local mock servers: recorded stream
//! fixtures per wire format, prompt caching markers and ledger rows, and
//! fault injection for retries and the fallback chain.

mod accounts;
mod auth;
mod auxiliary;
mod azure;
mod borrowed;
mod bundled;
mod caching;
mod call_cache;
mod external;
mod faults;
mod idp;
mod local;
mod metrics;
mod minting;
mod moa;
mod moa_accounting;
mod mock;
mod pool_seeding;
mod pools;
mod portals;
mod refresh;
mod request_controls;
mod routing;
mod server_compaction;
mod session;
mod support;
mod tool_gateway;
mod wire;
