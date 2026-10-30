//! Provider integration tests against local mock servers: recorded stream
//! fixtures per wire format, prompt caching markers and ledger rows, and
//! fault injection for retries and the fallback chain.

mod caching;
mod faults;
mod session;
mod support;
mod wire;
