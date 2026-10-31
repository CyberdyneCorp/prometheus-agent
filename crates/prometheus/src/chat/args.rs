//! `prometheus chat` flags (spec: cli, Chat flags; One-shot and piped
//! mode; Seed prompt and prompt file). Every flag applies to the started
//! session only and is never saved: the flags that change configuration
//! become `--set` style overrides of this invocation.

use std::path::PathBuf;

use clap::{ArgAction, Args, ValueEnum};
use pmt_core::error::{Error, codes};

use crate::cli::EndpointArgs;

/// Approval modes `--approvals` accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ApprovalsFlag {
    /// Ask for every dangerous call.
    Manual,
    /// Pre-screen with an auxiliary model; ask when uncertain.
    Smart,
    /// No prompts (deny rules and hardline blocks still apply).
    Off,
}

impl ApprovalsFlag {
    fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Smart => "smart",
            Self::Off => "off",
        }
    }
}

/// `prometheus chat` arguments (also accepted without the subcommand).
#[derive(Debug, Clone, Default, PartialEq, Eq, Args)]
#[allow(clippy::struct_excessive_bools)] // independent switches of one command line
pub struct ChatArgs {
    /// Model for this session (`[<provider>/]<model>` or an alias).
    #[arg(short = 'm', long, value_name = "MODEL")]
    pub model: Option<String>,
    /// Provider for this session.
    #[arg(long, value_name = "ID")]
    pub provider: Option<String>,
    /// Toolsets for this session, comma-separated.
    #[arg(short = 't', long, value_name = "LIST", value_delimiter = ',')]
    pub toolsets: Vec<String>,
    /// Resume a session by id, id prefix, title or `latest`.
    #[arg(short = 'r', long, value_name = "REF", conflicts_with = "continue_")]
    pub resume: Option<String>,
    /// Resume the most recent CLI session started in this directory, or
    /// with NAME the newest session of the lineage titled NAME.
    #[arg(short = 'c', long = "continue", value_name = "NAME", num_args = 0..=1, default_missing_value = "")]
    pub continue_: Option<String>,
    /// Run one prompt non-interactively and print only the answer (the
    /// prompt from this flag, `--prompt-file` or stdin).
    #[arg(short = 'z', long, value_name = "PROMPT", num_args = 0..=1, default_missing_value = "")]
    pub oneshot: Option<String>,
    /// Start an interactive session with TEXT as its first message, sent
    /// verbatim (never run as a slash command).
    #[arg(long, value_name = "TEXT")]
    pub query: Option<String>,
    /// Read the prompt verbatim from PATH (`-` for stdin).
    #[arg(long, value_name = "PATH")]
    pub prompt_file: Option<PathBuf>,
    /// Attach a local image (PNG, JPEG, GIF, WebP; at most 20 MiB) to the
    /// first prompt (repeatable).
    #[arg(long, value_name = "PATH", action = ArgAction::Append)]
    pub image: Vec<PathBuf>,
    /// Approval mode for this session.
    #[arg(long, value_enum, value_name = "MODE")]
    pub approvals: Option<ApprovalsFlag>,
    /// Allow every approval-gated call for this session (deny rules and
    /// hardline blocks still apply).
    #[arg(long)]
    pub yolo: bool,
    /// Model calls allowed per turn.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..))]
    pub max_iterations: Option<u32>,
    /// Working directory of the session (also scopes `-c` and `latest`).
    #[arg(long, value_name = "DIR")]
    pub cwd: Option<PathBuf>,
    /// Run the session in this process (the only mode until the daemon
    /// lands).
    #[arg(long)]
    pub in_process: bool,
    /// Use the full-screen interface.
    #[arg(long, conflicts_with = "line")]
    pub tui: bool,
    /// Use the line interface.
    #[arg(long)]
    pub line: bool,
    /// Skills to load at session start, comma-separated (repeatable).
    #[arg(short = 's', long, value_name = "LIST", value_delimiter = ',', action = ArgAction::Append)]
    pub skills: Vec<String>,
    /// Integration tag recorded on the session.
    #[arg(long, value_name = "TAG")]
    pub source: Option<String>,
    /// Put the session id in the system prompt.
    #[arg(long)]
    pub pass_session_id: bool,
    /// Wall-clock budget of each turn in seconds.
    #[arg(long, value_name = "SECS", value_parser = clap::value_parser!(u64).range(1..))]
    pub run_budget: Option<u64>,
    /// Do not load project context files (AGENTS.md and friends).
    #[arg(long)]
    pub no_context_files: bool,
    /// Run with schema defaults and this command line's flags instead of
    /// the profile's config.toml (secrets still resolve).
    #[arg(long)]
    pub ignore_user_config: bool,
    /// Skip workspace rule files (AGENTS.md and friends), the identity
    /// file, the memory snapshot and preloaded skills.
    #[arg(long)]
    pub ignore_rules: bool,
    /// Write this run's usage as JSON to PATH when it ends, whatever the
    /// outcome.
    #[arg(long, value_name = "PATH")]
    pub usage_file: Option<PathBuf>,
    /// Script for the `mock` provider.
    #[arg(long, value_name = "PATH")]
    pub mock_script: Option<PathBuf>,
    /// Launch-only endpoint.
    #[command(flatten)]
    pub endpoint: EndpointArgs,
}

/// Where the first prompt comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptSource {
    /// None given.
    None,
    /// `-z <text>` or `--query <text>`.
    Text(String),
    /// `--prompt-file <path>`.
    File(PathBuf),
}

impl ChatArgs {
    /// Whether no flag was given.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Whether `-z` was given (with or without text).
    #[must_use]
    pub fn oneshot(&self) -> bool {
        self.oneshot.is_some()
    }

    /// The first prompt's source. `--query`, `--prompt-file` and `-z
    /// <text>` exclude each other; a bare `-z` reads `--prompt-file` or
    /// stdin.
    ///
    /// # Errors
    ///
    /// `cli.usage` (exit 2) for two sources.
    pub fn prompt_source(&self) -> Result<PromptSource, Error> {
        let oneshot_text = self.oneshot.as_deref().filter(|t| !t.is_empty());
        let given = [
            oneshot_text.is_some(),
            self.query.is_some(),
            self.prompt_file.is_some(),
        ];
        if given.iter().filter(|g| **g).count() > 1 || (self.query.is_some() && self.oneshot()) {
            return Err(Error::new(
                &codes::CLI_USAGE,
                "--query, --prompt-file and -z <prompt> cannot be used together",
            ));
        }
        Ok(
            match (oneshot_text.or(self.query.as_deref()), &self.prompt_file) {
                (Some(text), _) => PromptSource::Text(text.to_owned()),
                (None, Some(path)) => PromptSource::File(path.clone()),
                (None, None) => PromptSource::None,
            },
        )
    }

    /// The configuration overrides these flags stand for, as `key=value`.
    #[must_use]
    pub fn overrides(&self) -> Vec<String> {
        let mut set = Vec::new();
        if let Some(mode) = self.approvals {
            set.push(format!("approvals.mode={}", mode.as_str()));
        }
        if let Some(n) = self.max_iterations {
            set.push(format!("agent.max_iterations={n}"));
        }
        if self.ignore_rules {
            set.push("prompt.load_identity=false".to_owned());
        }
        if !self.toolsets.is_empty() {
            let list: Vec<String> = self.toolsets.iter().map(|t| format!("{t:?}")).collect();
            set.push(format!("tools.enabled_toolsets=[{}]", list.join(", ")));
        }
        set
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(Parser)]
    struct Probe {
        #[command(flatten)]
        chat: ChatArgs,
    }

    fn parse(args: &[&str]) -> ChatArgs {
        Probe::try_parse_from(std::iter::once("p").chain(args.iter().copied()))
            .unwrap()
            .chat
    }

    #[test]
    fn prompt_sources_exclude_each_other() {
        assert_eq!(
            parse(&["-z", "hi"]).prompt_source().unwrap(),
            PromptSource::Text("hi".into())
        );
        assert_eq!(
            parse(&["-z", "--prompt-file", "b.txt"])
                .prompt_source()
                .unwrap(),
            PromptSource::File("b.txt".into())
        );
        assert_eq!(parse(&["-z"]).prompt_source().unwrap(), PromptSource::None);
        for bad in [
            &["-z", "hi", "--prompt-file", "b"][..],
            &["--query", "a", "--prompt-file", "b"],
            &["--query", "a", "-z"],
        ] {
            let err = parse(bad).prompt_source().unwrap_err();
            assert_eq!(err.exit_code(), 2, "{bad:?}");
        }
    }

    #[test]
    fn flags_become_overrides_of_this_run() {
        let args = parse(&[
            "--approvals",
            "off",
            "-t",
            "web,file",
            "--max-iterations",
            "3",
        ]);
        assert_eq!(
            args.overrides(),
            [
                "approvals.mode=off",
                "agent.max_iterations=3",
                "tools.enabled_toolsets=[\"web\", \"file\"]"
            ]
        );
        assert_eq!(parse(&["-c"]).continue_.as_deref(), Some(""));
        assert_eq!(
            parse(&["-c", "Refactor"]).continue_.as_deref(),
            Some("Refactor")
        );
    }
}
