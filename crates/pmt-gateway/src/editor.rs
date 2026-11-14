//! `session.editor`, `session.settings` and `session.configure` (spec:
//! acp-adapter): an editor's MCP servers bound to the session, and the
//! session's model, effort, edit mode and working directory as an editor
//! shows and changes them. Binding and changing need a driver; reading
//! the settings any attachment.

use pmt_rpc::contracts as c;
use pmt_rpc::{Catalog, RpcError};

use crate::methods::{Need, attached_profile, rpc};
use crate::router::Router;
use crate::server::Call;

/// One editor request, by method.
#[derive(Debug, Clone)]
pub enum EditorCall {
    /// `session.editor`.
    Bind(c::EditorBindParams),
    /// `session.settings`.
    Settings(c::SessionRef),
    /// `session.configure`.
    Configure(c::ConfigureParams),
}

impl EditorCall {
    /// The session it names.
    #[must_use]
    pub fn session_id(&self) -> &str {
        match self {
            Self::Bind(p) => &p.session_id,
            Self::Settings(p) => &p.session_id,
            Self::Configure(p) => &p.session_id,
        }
    }

    fn need(&self) -> Need {
        match self {
            Self::Settings(_) => Need::Attached,
            Self::Bind(_) | Self::Configure(_) => Need::Driver,
        }
    }
}

/// Its answer.
#[derive(Debug, Clone)]
pub enum EditorReply {
    /// `session.editor`.
    Bound(c::EditorBound),
    /// `session.settings`.
    Settings(c::SessionSettings),
    /// `session.configure`.
    Configured(c::Configured),
}

/// Registers the editor methods.
///
/// # Errors
///
/// `unregistered method name` for a method missing from `catalog`.
pub fn register(router: &mut Router, catalog: &Catalog) -> Result<(), String> {
    router
        .register::<c::SessionEditor, _, _>(catalog, |call, p| async move {
            match run(&call, EditorCall::Bind(p)).await? {
                EditorReply::Bound(bound) => Ok(bound),
                _ => Err(unexpected()),
            }
        })?
        .register::<c::SessionSettingsCall, _, _>(catalog, |call, p| async move {
            match run(&call, EditorCall::Settings(p)).await? {
                EditorReply::Settings(settings) => Ok(settings),
                _ => Err(unexpected()),
            }
        })?
        .register::<c::SessionConfigure, _, _>(catalog, |call, p| async move {
            match run(&call, EditorCall::Configure(p)).await? {
                EditorReply::Configured(done) => Ok(done),
                _ => Err(unexpected()),
            }
        })?;
    Ok(())
}

async fn run(call: &Call, request: EditorCall) -> Result<EditorReply, RpcError> {
    let profile = attached_profile(call, request.session_id(), request.need())?;
    let link = call.server.link(request.session_id());
    call.server
        .host()
        .editor(&profile, request, link)
        .await
        .map_err(|e| rpc(&e))
}

fn unexpected() -> RpcError {
    rpc(&pmt_core::error::Error::new(
        &pmt_core::error::codes::RPC_NOT_SERVED,
        "the host answered a different editor request",
    ))
}
