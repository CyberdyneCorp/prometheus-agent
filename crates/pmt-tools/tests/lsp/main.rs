//! LSP diagnostics behaviour contracts (spec: lsp-diagnostics): the file
//! tools with a session's language servers, played by a stand-in server
//! (`fake_lsp.py`) or, when installed, the real pyright.

#![cfg(unix)] // the stand-in server is a python3 script

mod diagnostics;
mod install;
mod pyright;
mod sandbox;
mod support;
mod trust;
