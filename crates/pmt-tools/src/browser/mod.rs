//! Browser automation: the `browser` toolset's `browser_exec` (spec:
//! execution-environments, Browser session resolution; Browser URL
//! safety; Browser supervision, dialogs and cleanup; `browser_exec` tool).
//!
//! [`session`] resolves and keeps browser sessions ([`launch`] starts a
//! local Chromium, [`records`] lets a later process reap a crashed owner's
//! browser), [`cdp`] speaks the DevTools protocol, [`supervisor`] answers
//! dialogs and reconnects, [`safety`] vets every navigation, [`tabs`] are
//! the page helpers and [`exec`] runs the model's Python against them.

pub mod cdp;
pub mod exec;
pub mod launch;
pub mod records;
pub mod safety;
pub mod session;
pub mod supervisor;
pub mod tabs;

use std::sync::Arc;
use std::time::Duration;

use pmt_config::schema::BrowserConfig;
use pmt_core::capability::Capability;
use pmt_core::error::{Error, codes};
use serde_json::{Value, json};

pub use exec::{BrowserExec, ExecArgs, ExecSetup};
pub use launch::{NoRuntimes, RuntimePolicy, Runtimes};
pub use safety::UrlSafety;
pub use session::{BrowserManager, BrowserSession, BrowserSetup, CloudProvider, Origin};

use crate::descriptor::{
    CallContext, HandlerFuture, Owner, RegisteredTool, ToolDescriptor, ToolHandler,
};
use crate::environments::{Environments, part};
use crate::result::ToolResult;

/// The toolset of the browser tools.
pub const TOOLSET: &str = "browser";

/// Checks `browser.provider`: `local` or an installed cloud provider
/// plugin (`providers`).
///
/// # Errors
///
/// `config.invalid` (exit 78) naming the unknown provider.
pub fn validate_provider(config: &BrowserConfig, providers: &[&str]) -> Result<(), Error> {
    if config.provider == "local" || providers.contains(&config.provider.as_str()) {
        return Ok(());
    }
    let mut known = vec!["local"];
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

/// `browser_exec`.
#[must_use]
pub fn tools(environments: &Arc<Environments>) -> Vec<RegisteredTool> {
    vec![RegisteredTool::new(
        descriptor(),
        Owner::Builtin,
        Arc::new(Handler(Arc::clone(environments))),
    )]
}

impl ToolHandler for Handler {
    fn call<'a>(&'a self, context: &'a CallContext, arguments: Value) -> HandlerFuture<'a> {
        Box::pin(async move {
            let browser = part(&self.0, context, "browser", |env| env.browser.clone())?;
            let args = ExecArgs {
                code: arguments["code"].as_str().unwrap_or_default().to_owned(),
                session: arguments["session"].as_str().map(str::to_owned),
                timeout_secs: arguments["timeout_secs"].as_u64(),
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
                "timeout_secs": {"type": "integer", "minimum": exec::TIMEOUT_RANGE.0, "maximum": exec::TIMEOUT_RANGE.1, "description": "Bound on the call (default 300)."}
            },
            "required": ["code"],
            "additionalProperties": false
        }),
    );
    d.capabilities = ["process.spawn:*", "net.connect:*"]
        .iter()
        .map(|c| Capability::parse(c).expect("valid capability"))
        .collect();
    d.claims.exclusive = vec![TOOLSET.to_owned()];
    d.requires = vec!["terminal".to_owned()];
    d.timeout = Some(Duration::from_secs(exec::TIMEOUT_RANGE.1 + 30));
    d
}
