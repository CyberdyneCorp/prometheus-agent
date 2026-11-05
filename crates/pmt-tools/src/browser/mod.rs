//! Browser automation: the `browser` toolset's `browser_exec`,
//! `browser_cdp` and `browser_dialog` (spec: execution-environments,
//! Browser session resolution; Browser URL safety; Browser supervision,
//! dialogs and cleanup; `browser_exec` tool; `browser_cdp` and
//! `browser_dialog` tools; Local browser options and headed mode;
//! First-party cloud browser providers; Private URLs route to a local
//! browser; Real-profile browsing; Connecting to the user's browser).
//!
//! [`session`] resolves and keeps browser sessions ([`launch`] starts a
//! local Chromium with the [`options`] of `[browser]`, [`local`] owns its
//! process, [`records`] lets a later process reap a crashed owner's
//! browser, [`cleanup`] kills a browser's process group and removes its
//! directories, [`cloud`] creates provider browsers, [`connect`] reaches
//! the user's own browser, [`real_profile`] copies the user's profile,
//! [`status`] tells later processes what runs), [`cdp`] speaks the
//! DevTools protocol, [`supervisor`] answers dialogs and reconnects,
//! [`safety`] vets every navigation, [`tabs`] are the page helpers,
//! [`exec`] runs the model's Python against them and [`cdp_tools`] sends
//! raw commands and answers dialogs.

pub mod cdp;
pub mod cdp_tools;
pub mod cleanup;
pub mod cloud;
pub mod confine;
pub mod connect;
pub mod exec;
pub mod launch;
mod local;
pub mod options;
pub mod profile;
pub mod real_profile;
pub mod records;
pub mod safety;
pub mod session;
pub mod status;
pub mod supervisor;
pub mod tabs;
pub mod vault;
mod vault_js;

use std::sync::Arc;
use std::time::Duration;

use pmt_config::schema::BrowserConfig;
use pmt_core::capability::Capability;
use pmt_core::error::{Error, codes};
use serde_json::{Value, json};

pub use exec::{BrowserExec, ExecArgs, ExecSetup};
pub use launch::{NoRuntimes, RuntimePolicy, Runtimes};
pub use safety::UrlSafety;
pub use session::{BrowserManager, BrowserSession, BrowserSetup, CloudProvider, Origin, Request};

use crate::descriptor::{
    CallContext, HandlerFuture, Owner, RegisteredTool, ResourceClaims, ToolDescriptor, ToolHandler,
};
use crate::environments::{Environments, part};
use crate::result::ToolResult;

/// The toolset of the browser tools.
pub const TOOLSET: &str = "browser";

/// Checks `browser.provider`: `local`, a first-party cloud provider
/// ([`cloud::MANIFESTS`]) or an installed provider plugin (`providers`).
///
/// # Errors
///
/// `config.invalid` (exit 78) naming the unknown provider.
pub fn validate_provider(config: &BrowserConfig, providers: &[&str]) -> Result<(), Error> {
    let first_party = cloud::names();
    if config.provider == "local"
        || first_party.contains(&config.provider.as_str())
        || providers.contains(&config.provider.as_str())
    {
        return Ok(());
    }
    let mut known = vec!["local"];
    known.extend(first_party);
    known.extend_from_slice(providers);
    Err(Error::new(
        &codes::CONFIG_INVALID,
        format!(
            "browser.provider: unknown browser provider '{}' (expected {})",
            config.provider,
            known.join(", ")
        ),
    ))
}

struct Handler(Arc<Environments>);

/// `browser_exec`, `browser_cdp`, `browser_dialog` and `browser_vault`.
#[must_use]
pub fn tools(environments: &Arc<Environments>) -> Vec<RegisteredTool> {
    let mut tools = vec![
        RegisteredTool::new(
            descriptor(),
            Owner::Builtin,
            Arc::new(Handler(Arc::clone(environments))),
        )
        .with_parameters(Arc::new(local_parameter)),
    ];
    tools.extend(cdp_tools::tools(environments));
    tools.push(vault::tool(environments));
    tools
}

/// `browser_exec`'s schema without `local` unless real-profile browsing is
/// enabled (spec: execution-environments, `browser_exec` tool).
fn local_parameter(schema: &Value, config: &pmt_config::schema::Config) -> Value {
    let mut schema = schema.clone();
    if !config.browser.use_real_profile
        && let Some(properties) = schema["properties"].as_object_mut()
    {
        properties.remove("local");
    }
    schema
}

impl ToolHandler for Handler {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let browser = part(&self.0, context, "browser", |env| env.browser.clone())?;
            let args = ExecArgs {
                code: arguments["code"].as_str().unwrap_or_default().to_owned(),
                session: arguments["session"].as_str().map(str::to_owned),
                timeout_secs: arguments["timeout_secs"].as_u64(),
                local: arguments["local"].as_bool().unwrap_or(false),
            };
            Ok::<ToolResult, _>(browser.exec(&context.session_id, &args).await)
        })
    }
}

fn descriptor() -> ToolDescriptor {
    let mut d = ToolDescriptor::new(
        "browser_exec",
        TOOLSET,
        "Drive a real browser by running Python 3. Helpers are pre-imported: new_tab(url=None), \
         goto_url(url), wait_for_load(timeout=30), page_info(), js(expression), \
         fill_input(selector, text), click_at_xy(x, y), capture_screenshot(path=None), \
         cdp(method, params=None), ensure_real_tab(). Batch several steps in one call and \
         print what you need; the result is what the code prints. Tabs, cookies and the \
         current page persist across calls, Python variables do not; files written to the \
         working directory (workspace) persist. A named session gets its own browser.",
        json!({
            "type": "object",
            "properties": {
                "code": {"type": "string", "description": "Python 3 source."},
                "session": {"type": "string", "description": "Named browser session (default: default)."},
                "timeout_secs": {"type": "integer", "minimum": exec::TIMEOUT_RANGE.0, "maximum": exec::TIMEOUT_RANGE.1, "description": "Bound on the call (default 300)."},
                "local": {"type": "boolean", "description": "Use the local browser on your real profile snapshot even when a cloud browser is configured."}
            },
            "required": ["code"],
            "additionalProperties": false
        }),
    );
    d.capabilities = ["process.spawn:*", "net.connect:*"]
        .iter()
        .map(|c| Capability::parse(c).expect("valid capability"))
        .collect();
    d.claims = Some(ResourceClaims::exclusive(TOOLSET));
    d.requires = vec!["terminal".to_owned()];
    d.timeout = Some(Duration::from_secs(exec::TIMEOUT_RANGE.1 + 30));
    d
}
