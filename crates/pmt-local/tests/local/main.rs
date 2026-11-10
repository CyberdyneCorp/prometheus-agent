//! Local models against local fixtures: a wiremock origin serving engine
//! archives and model files (with HTTP Range), and the `pmt-llama-stub`
//! stand-in llama-server.

mod downloads;
mod engine;
mod hub;
mod library;
mod server;
mod support;
