//! `prometheus local` (spec: local-models, Local models command): the
//! llama.cpp engine, the hardware-priced catalog, downloads, the model
//! library and the managed llama-server. The work is `pmt_local`'s; this
//! module parses, prints and wires the process (Ctrl+C, the daemon).
//!
//! Debug builds read test fixtures from `PMT_TEST_LOCAL_FIXTURES=<dir>`:
//! `hardware.json` (the probed machine), `catalog.json`, `engine.json`
//! (pins) and `hub_url` (the Hugging Face origin), each optional.

mod args;
mod engine;
mod models;
pub(crate) mod quickstart;
pub mod rpc;
pub mod runtime;

use std::path::PathBuf;

use pmt_config::daemon::DaemonConfig;
use pmt_config::daemon_local::LocalRuntimeConfig;
use pmt_core::error::Error;
use pmt_local::backend::{self, Selection};
use pmt_local::catalog::Catalog;
use pmt_local::download::{Control, Downloader, Jobs, Progress};
use pmt_local::engine::{Engine, EngineIndex};
use pmt_local::hardware::{self, Hardware};

pub use args::LocalCommand;

use crate::context::Context;
use crate::model_cmd::block_on;

/// Runs one `local` subcommand.
///
/// # Errors
///
/// The command's failure: unsupported backend 69, digest mismatch 74,
/// unknown model or job 66, server down 69, download 69.
pub fn run(command: &LocalCommand, ctx: &Context<'_>) -> Result<u8, Error> {
    let local = Local::open(ctx)?;
    match command {
        LocalCommand::Status => engine::status(&local),
        LocalCommand::Install => engine::install(&local),
        LocalCommand::Update => engine::update(&local),
        LocalCommand::Start => engine::start(&local, ctx),
        LocalCommand::Stop => engine::stop(&local),
        LocalCommand::Supervise => runtime::supervise(&local.root),
        LocalCommand::Catalog => models::catalog(&local),
        LocalCommand::Pull { model } => models::pull(&local, model),
        LocalCommand::Pause { job } => models::pause(&local, job),
        LocalCommand::Resume { job } => models::resume(&local, job),
        LocalCommand::Jobs => models::jobs(&local),
        LocalCommand::Search { query } => models::search(&local, query),
        LocalCommand::Add { path } => models::add(&local, path),
        LocalCommand::Rm { model } => models::rm(&local, model),
        LocalCommand::Use { model, global: _ } => models::use_model(&local, ctx, model),
    }
}

/// What every `local` command works with.
pub(crate) struct Local {
    pub(crate) root: PathBuf,
    pub(crate) config: LocalRuntimeConfig,
    pub(crate) json: bool,
    policy: pmt_net::OutboundPolicy,
    fixtures: Option<PathBuf>,
}

impl Local {
    pub(crate) fn open(ctx: &Context<'_>) -> Result<Self, Error> {
        let root = ctx.home()?.root().to_path_buf();
        let mut local = Self::for_home(&root, crate::update_cmd::outbound_policy(ctx))?;
        local.json = ctx.json;
        Ok(local)
    }

    /// The local models of `root` downloading under `policy`.
    pub(crate) fn for_home(
        root: &std::path::Path,
        policy: pmt_net::OutboundPolicy,
    ) -> Result<Self, Error> {
        Ok(Self {
            root: root.to_path_buf(),
            config: DaemonConfig::load(root)?.local_runtime,
            json: false,
            policy,
            fixtures: fixtures(),
        })
    }

    /// The probed machine (or the fixture).
    pub(crate) fn hardware(&self) -> Hardware {
        self.fixture("hardware.json")
            .and_then(|text| Hardware::from_json(&text).ok())
            .unwrap_or_else(hardware::probe)
    }

    /// The backend and GPU budget of this machine.
    pub(crate) fn selection(&self, hardware: &Hardware) -> Selection {
        backend::select(self.config.backend, hardware)
    }

    pub(crate) fn catalog(&self) -> Catalog {
        self.fixture("catalog.json")
            .and_then(|text| Catalog::from_json(&text).ok())
            .unwrap_or_else(Catalog::embedded)
    }

    pub(crate) fn engine(&self) -> Engine {
        match self
            .fixture("engine.json")
            .and_then(|text| serde_json::from_str::<EngineIndex>(&text).ok())
        {
            Some(index) => Engine::new(self.root.join("runtimes").join("llamacpp"), index),
            None => Engine::open(&self.root),
        }
    }

    pub(crate) fn hub_url(&self) -> String {
        self.fixture("hub_url")
            .map_or_else(|| pmt_local::hub::HUB.to_owned(), |s| s.trim().to_owned())
    }

    pub(crate) fn jobs(&self) -> Jobs {
        Jobs::open(&self.root)
    }

    pub(crate) fn library(&self) -> pmt_local::library::Library {
        pmt_local::library::Library::open(&self.root)
    }

    /// An outbound client under the profile's network settings.
    pub(crate) fn client(&self) -> Result<reqwest::Client, Error> {
        pmt_net::client(&self.policy)
    }

    pub(crate) fn downloader(&self) -> Result<Downloader, Error> {
        Ok(Downloader::new(self.client()?))
    }

    fn fixture(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.fixtures.as_ref()?.join(name)).ok()
    }
}

#[cfg(debug_assertions)]
fn fixtures() -> Option<PathBuf> {
    std::env::var_os("PMT_TEST_LOCAL_FIXTURES").map(PathBuf::from)
}

#[cfg(not(debug_assertions))]
fn fixtures() -> Option<PathBuf> {
    None
}

/// Runs a download under Ctrl+C: the first Ctrl+C pauses the job at its
/// next chunk (it is never deleted). `run` gets the job's control and a
/// progress printer for stderr.
pub(crate) fn interruptible<T, F>(
    label: &str,
    run: impl FnOnce(Control, Printer) -> F,
) -> Result<T, Error>
where
    F: Future<Output = Result<T, Error>>,
{
    block_on(async {
        let control = Control::default();
        let watcher = {
            let control = control.clone();
            pmt_core::spawn::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    control.pause();
                }
            })
        };
        let result = run(control, Printer::new(label)).await;
        watcher.abort();
        result
    })?
}

/// Prints whole-job progress to stderr when the percentage changes.
pub(crate) struct Printer {
    label: String,
    last: Option<u64>,
}

impl Printer {
    fn new(label: &str) -> Self {
        Self {
            label: label.to_owned(),
            last: None,
        }
    }

    pub(crate) fn show(&mut self, progress: Progress) {
        let percent = progress.percent();
        if self.last == Some(percent) {
            return;
        }
        self.last = Some(percent);
        eprintln!(
            "{} {percent}% ({} of {})",
            self.label,
            size(progress.done),
            size(progress.total)
        );
    }
}

/// `1.2 MB`.
pub(crate) fn size(bytes: u64) -> String {
    pmt_update::runtimes::lazy::human_size(bytes)
}
