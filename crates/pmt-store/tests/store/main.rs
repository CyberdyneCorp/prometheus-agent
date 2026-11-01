//! Behaviour contracts of the session store (spec: session-storage), one
//! test binary with a module per area.

mod blobs;
mod compaction;
mod compression_state;
mod crash;
mod credential_state;
mod integrity;
mod lifecycle;
mod open;
mod prompts;
mod search;
mod sessions;
mod support;
mod titles;
mod todos;
mod tool_surface;
