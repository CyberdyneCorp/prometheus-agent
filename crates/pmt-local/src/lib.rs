//! Local models (spec: local-models): open-weight models run on this
//! machine by a hash-pinned llama.cpp engine.
//!
//! - [`hardware`] probes the GPUs and RAM; [`backend`] picks the build.
//! - [`engine`] installs the pinned build; [`download`] is the one
//!   resumable, verifying downloader every file goes through.
//! - [`catalog`] prices the curated models for this machine; [`hub`]
//!   searches Hugging Face; [`pull`] downloads a model into the
//!   machine-wide [`library`]; [`gguf`] reads a model file's metadata.
//! - [`server`] finds or starts the `llama-server` behind the ordinary
//!   `llamacpp` provider, so selection, fallback and accounting are the
//!   providers' own; [`status`] reports it.
//!
//! Only `prometheus local` commands and the explicit `install`, `pull`,
//! `search` actions use the network; a session on a local model reads the
//! engine, weights and catalog from disk.

pub mod backend;
pub mod catalog;
pub mod download;
pub mod engine;
pub mod gguf;
pub mod hardware;
pub mod hub;
pub mod library;
pub mod pull;
pub mod server;
pub mod status;
#[cfg(feature = "testkit")]
pub mod testkit;
