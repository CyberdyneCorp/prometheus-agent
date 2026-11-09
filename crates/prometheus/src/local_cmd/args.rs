//! `prometheus local` arguments (spec: local-models, Local models command).

use std::path::PathBuf;

use clap::Subcommand;

/// `prometheus local` subcommands.
#[derive(Debug, Subcommand)]
pub enum LocalCommand {
    /// Engine version and backend, server state and port, resident models,
    /// GPU and RAM use.
    Status,
    /// Download, verify and install the pinned llama.cpp engine for this
    /// machine's backend.
    Install,
    /// Install this build's engine pin and remove older engine builds.
    Update,
    /// The curated models priced for this machine: memory fit, windows,
    /// quantisation, download size, the recommendation.
    Catalog,
    /// Download a catalog model (or a Hugging Face `owner/repo[:quant]`)
    /// into the library; Ctrl+C pauses it.
    Pull {
        /// Catalog id or `owner/repo[:quant or file]`.
        model: String,
    },
    /// Pause a download job at its next chunk.
    Pause {
        /// Job id (`prometheus local jobs`).
        job: String,
    },
    /// Resume a paused or interrupted download job; Ctrl+C pauses it again.
    Resume {
        /// Job id.
        job: String,
    },
    /// Download jobs and their progress.
    Jobs,
    /// Search Hugging Face for GGUF models, with a fit check per file.
    Search {
        /// Search words.
        query: String,
    },
    /// Link an existing GGUF file into the library without copying it.
    Add {
        /// The `.gguf` file.
        path: PathBuf,
    },
    /// Remove a model from the library (a linked file is left in place).
    Rm {
        /// Library id.
        model: String,
    },
    /// Make `llamacpp/<model>` the profile's model (`/local use` in a chat
    /// switches the session only, unless given `--global`).
    Use {
        /// Library id.
        model: String,
        /// Save it in the profile config; a command-line `use` always does.
        #[arg(long)]
        global: bool,
    },
    /// Run the managed llama-server (in the daemon when it runs, else in
    /// the foreground until Ctrl+C).
    Start,
    /// Stop the managed llama-server.
    Stop,
    /// Supervise the managed server for a client process (internal: runs
    /// until its stdin closes).
    #[command(hide = true)]
    Supervise,
}
