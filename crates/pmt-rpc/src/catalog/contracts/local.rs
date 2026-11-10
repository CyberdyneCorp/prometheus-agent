//! Contracts of local models (spec: local-models, Local models command):
//! the `local.*` methods the desktop and dashboard use, one per
//! `prometheus local` subcommand. A method answers a one-line message and
//! the same document the matching command prints with `--json` (for
//! example `local.catalog` answers the `prometheus.local_catalog.v1`
//! body); long work (`install`, `update`, `pull`, `resume`) runs in the
//! daemon and is followed with `local.jobs`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Method;

/// Params of every `local.*` method; each uses the fields its command
/// takes (`model` for pull, rm and use; `job` for pause and resume;
/// `query` for search; `path` for add).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LocalParams {
    /// The profile (the connection's only one when omitted); `local.use`
    /// sets its model.
    #[serde(default)]
    pub profile: Option<String>,
    /// Catalog or library id, or `owner/repo[:quant]` for `local.pull`.
    #[serde(default)]
    pub model: Option<String>,
    /// A download job id.
    #[serde(default)]
    pub job: Option<String>,
    /// Search words.
    #[serde(default)]
    pub query: Option<String>,
    /// A `.gguf` file on the daemon's machine.
    #[serde(default)]
    pub path: Option<String>,
}

/// Result of every `local.*` method.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LocalReply {
    /// What happened, in one line.
    pub message: String,
    /// The command's `--json` document (without `schema`), or `null`.
    #[serde(default)]
    pub data: Value,
}

method!(/// `local.status`: engine, server, resident models, GPU and RAM use,
    /// the library.
    LocalStatus = "local.status" (LocalParams) -> LocalReply);
method!(/// `local.install`: installs the pinned engine (in the background).
    LocalInstall = "local.install" (LocalParams) -> LocalReply);
method!(/// `local.update`: installs this build's engine pin, removes older builds.
    LocalUpdate = "local.update" (LocalParams) -> LocalReply);
method!(/// `local.catalog`: the catalog priced for the daemon's machine.
    LocalCatalog = "local.catalog" (LocalParams) -> LocalReply);
method!(/// `local.pull`: starts downloading `model` (in the background).
    LocalPull = "local.pull" (LocalParams) -> LocalReply);
method!(/// `local.pause`: pauses download `job` at its next chunk.
    LocalPause = "local.pause" (LocalParams) -> LocalReply);
method!(/// `local.resume`: resumes download `job` (in the background).
    LocalResume = "local.resume" (LocalParams) -> LocalReply);
method!(/// `local.jobs`: download jobs and their progress.
    LocalJobs = "local.jobs" (LocalParams) -> LocalReply);
method!(/// `local.search`: Hugging Face GGUF models matching `query`.
    LocalSearch = "local.search" (LocalParams) -> LocalReply);
method!(/// `local.add`: links the GGUF file at `path` into the library.
    LocalAdd = "local.add" (LocalParams) -> LocalReply);
method!(/// `local.rm`: removes `model` from the library.
    LocalRm = "local.rm" (LocalParams) -> LocalReply);
method!(/// `local.use`: makes `llamacpp/<model>` the profile's model.
    LocalUse = "local.use" (LocalParams) -> LocalReply);
method!(/// `local.start`: the daemon starts and supervises the managed server.
    LocalStart = "local.start" (LocalParams) -> LocalReply);
method!(/// `local.stop`: stops the managed server.
    LocalStop = "local.stop" (LocalParams) -> LocalReply);
