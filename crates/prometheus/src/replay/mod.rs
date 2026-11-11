//! Session recordings and their replay (spec: replay-and-evaluation,
//! Session recordings; Replay modes; Divergence detection; Replay has no
//! side effects; Golden recording regression gate).
//!
//! A session is recorded as it runs ([`pmt_obs::recording`]): its prompt
//! builds, every model call with its canonical request and response, and
//! every tool result. [`export`] turns that and the trajectory into a
//! `.pmtrec` file; [`run`] re-executes it in a home of its own under the
//! output directory, where a [`source::Replayer`] answers model calls,
//! tool calls and prompt builds from the recording and reports the first
//! divergence. `cron replay <run>` replays a scheduled run the same way,
//! straight from the profile.

pub mod export;
pub mod gate;
pub mod run;
pub mod source;
mod state;
mod trust;

use std::sync::Arc;

use pmt_agent::turn::model::RequestedCall;
use pmt_tools::ToolResult;

pub use state::State;

/// What a replay serves from the recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Mode {
    /// Model responses and tool results from the recording.
    #[default]
    Strict,
    /// Model responses from the recording; tools run for real in a scratch
    /// copy of the recorded workspace.
    ToolsLive,
    /// User inputs and tool results from the recording; the model calls
    /// go to the configured provider.
    ModelLive,
}

impl Mode {
    /// The flag spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::ToolsLive => "tools-live",
            Self::ModelLive => "model-live",
        }
    }
}

/// What a divergence does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum OnDivergence {
    /// End the replay at the first divergence.
    #[default]
    Stop,
    /// Fall through to the live provider and live tools for the rest.
    Continue,
}

/// How a replay answers one tool call.
#[derive(Debug, Clone)]
pub enum ToolAnswer {
    /// Return the recorded result; the tool does not run.
    Serve(ToolResult),
    /// Run the tool for real.
    Live,
}

/// Answers a replayed session's tool calls from the recording.
pub trait ToolReplay: Send + Sync {
    /// The answer to `call`.
    fn answer(&self, call: &RequestedCall) -> ToolAnswer;

    /// The tool schemas the recording's first model request offered, in
    /// order.
    fn recorded_tools(&self) -> Vec<pmt_providers::request::Tool>;
}

/// What a replay plugs into the sessions it opens.
pub struct Hooks {
    /// Tool calls.
    pub tools: Arc<dyn ToolReplay>,
    /// Prompt builds.
    pub pins: Arc<dyn pmt_agent::prompt::pins::PromptPins>,
}

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Hooks")
    }
}
