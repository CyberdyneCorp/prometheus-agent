//! `computer-use-backend` plugins as computer-use backends (spec:
//! computer-use, Backend selection and availability).
//!
//! A loaded plugin exporting the world provides one backend under the name
//! its `name()` returns; `computer_use.backend = "<name>"` selects it. The
//! backend implements the tool's driver seam ([`Launcher`] and [`Driver`]),
//! so the `computer_use` tool keeps its whole contract (approvals, hard
//! blocks, budgets, retention) whatever drives the desktop. Plugin calls
//! block, so they run on blocking threads.

use std::sync::Arc;

use pmt_config::schema::CUA_BACKEND;
use pmt_core::error::Error;
use pmt_tools::computer_use::{Driver, DriverReply, Launcher, RuntimeSpec};
use pmt_tools::dispatch::SeamFuture;
use serde_json::{Map, Value};

use crate::host::computer_use_world::{self, Reply};
use crate::host::{CallInfo, Instance};
use crate::load::Loaded;

/// The world as `plugin.toml` names it.
pub const WORLD: &str = "computer-use-backend";

/// One plugin's computer-use backend.
#[derive(Debug, Clone)]
pub struct PluginDesktop {
    /// The name `computer_use.backend` selects.
    pub name: String,
    instance: Arc<Instance>,
    call: CallInfo,
}

impl PluginDesktop {
    /// The plugin providing the backend.
    #[must_use]
    pub fn plugin(&self) -> &str {
        self.instance.id()
    }

    /// The backend's readiness: what is in use, or why it is unavailable.
    ///
    /// # Errors
    ///
    /// The plugin's own answer, or the failed call.
    pub fn check(&self) -> Result<String, String> {
        self.instance
            .desktop_check(&self.call)
            .map_err(|err| err.message().to_owned())?
    }

    /// The backend as the tool's driver seam.
    #[must_use]
    pub fn launcher(&self) -> Arc<dyn Launcher> {
        Arc::new(self.clone())
    }
}

/// The computer-use backends of `loaded`'s plugins, in load order, read
/// as `call`. A plugin whose name cannot be read, or that names the
/// built-in `cua`, provides none (logged).
#[must_use]
pub fn backends(loaded: &Loaded, call: &CallInfo) -> Vec<PluginDesktop> {
    loaded
        .plugins
        .iter()
        .filter(|p| p.instance.implements(WORLD))
        .filter_map(|p| match p.instance.desktop_name(call) {
            Ok(name) if name == CUA_BACKEND => {
                tracing::warn!(plugin = %p.manifest.id, "computer-use backend name 'cua' is reserved");
                None
            }
            Ok(name) => Some(PluginDesktop {
                name,
                instance: Arc::clone(&p.instance),
                call: call.clone(),
            }),
            Err(err) => {
                tracing::warn!(plugin = %p.manifest.id, error = %err, "computer-use backend name unreadable");
                None
            }
        })
        .collect()
}

/// Runs a plugin call on a blocking thread.
async fn blocking<R: Send + 'static>(
    f: impl FnOnce() -> Result<Result<R, String>, Error> + Send + 'static,
) -> Result<R, String> {
    pmt_core::spawn::blocking(f)
        .await
        .map_err(|e| e.to_string())?
        .map_err(|err| err.message().to_owned())?
}

impl Launcher for PluginDesktop {
    fn launch<'a>(
        &'a self,
        spec: &'a RuntimeSpec,
    ) -> SeamFuture<'a, Result<Arc<dyn Driver>, String>> {
        Box::pin(async move {
            let manifest = match &spec.manifest {
                Some(path) => Some(
                    std::fs::read_to_string(path)
                        .map_err(|e| format!("capability manifest {}: {e}", path.display()))?,
                ),
                None => None,
            };
            let wit = computer_use_world::RuntimeSpec {
                mode: spec.mode.as_str().to_owned(),
                manifest,
                session_label: spec.session_label.clone(),
                overlay: spec.overlay,
            };
            let instance = Arc::clone(&self.instance);
            let call = self.call.clone();
            let (runtime, tools) = blocking(move || {
                let runtime = match instance.desktop_launch(&call, &wit)? {
                    Ok(id) => id,
                    Err(why) => return Ok(Err(why)),
                };
                let tools = instance.desktop_tools(&call, &runtime)?;
                Ok(Ok((runtime, tools)))
            })
            .await?;
            Ok(Arc::new(Runtime {
                instance: Arc::clone(&self.instance),
                call: self.call.clone(),
                id: runtime,
                tools,
            }) as Arc<dyn Driver>)
        })
    }
}

/// One launched runtime of a plugin backend; dropping it stops it.
struct Runtime {
    instance: Arc<Instance>,
    call: CallInfo,
    id: String,
    tools: Vec<String>,
}

impl Driver for Runtime {
    fn call<'a>(
        &'a self,
        tool: &'a str,
        arguments: Map<String, Value>,
    ) -> SeamFuture<'a, Result<DriverReply, String>> {
        let instance = Arc::clone(&self.instance);
        let call = self.call.clone();
        let runtime = self.id.clone();
        let tool = tool.to_owned();
        Box::pin(async move {
            let arguments = Value::Object(arguments).to_string();
            let reply =
                blocking(move || instance.desktop_tool_call(&call, &runtime, &tool, &arguments))
                    .await?;
            Ok(driver_reply(reply))
        })
    }

    fn tools(&self) -> Vec<String> {
        self.tools.clone()
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        let instance = Arc::clone(&self.instance);
        let call = self.call.clone();
        let id = std::mem::take(&mut self.id);
        let stop = move || {
            if let Err(err) = instance.desktop_stop(&call, &id) {
                tracing::warn!(plugin = %instance.id(), error = %err, "computer-use runtime stop failed");
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => drop(pmt_core::spawn::blocking_on(&handle, stop)),
            Err(_) => stop(),
        }
    }
}

/// The plugin's reply as the driver seam's; structured content that is
/// not a JSON object is dropped (the text still carries the answer).
fn driver_reply(reply: Reply) -> DriverReply {
    let structured = serde_json::from_str::<Value>(&reply.structured)
        .ok()
        .filter(Value::is_object)
        .unwrap_or(Value::Null);
    DriverReply {
        is_error: reply.is_error,
        text: reply.text,
        structured,
        images: reply
            .images
            .into_iter()
            .map(|image| (image.mime, image.data))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::{Reply, driver_reply};
    use crate::host::computer_use_world::Image;

    #[test]
    fn a_reply_keeps_text_images_and_object_content_only() {
        let reply = driver_reply(Reply {
            is_error: false,
            text: "ok".into(),
            structured: r#"{"message":"clicked"}"#.into(),
            images: vec![Image {
                mime: "image/png".into(),
                data: vec![1, 2],
            }],
        });
        assert_eq!(reply.message(), "clicked");
        assert_eq!(reply.images, vec![("image/png".to_owned(), vec![1, 2])]);
        let plain = driver_reply(Reply {
            is_error: true,
            text: "no window".into(),
            structured: "[1]".into(),
            images: Vec::new(),
        });
        assert!(plain.is_error);
        assert_eq!(plain.structured, serde_json::Value::Null);
        assert_eq!(plain.message(), "no window");
    }
}
