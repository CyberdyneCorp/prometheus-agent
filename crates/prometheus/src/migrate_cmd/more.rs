//! `prometheus migrate sync`, `migrate cleanup openclaw` and `migrate
//! sessions` (spec: hermes-migration, Keeping imports in sync; Source
//! archiving after import; Session transcripts from other agent CLIs).

use std::fmt::Write as _;
use std::io::{BufRead as _, Write as _};
use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum};
use pmt_core::error::{Error, codes};
use pmt_migrate::agents::{cleanup, sync as imports, transcripts};

use super::{confirm, target_profile, user_home};
use crate::context::Context;
use crate::output::{json, print_raw};
use crate::prompt;

/// `migrate sync` options.
#[derive(Debug, Clone, Args)]
pub struct SyncArgs {
    /// Show what would sync; write nothing.
    #[arg(long)]
    pub dry_run: bool,
}

/// What `migrate cleanup` archives.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CleanupAgent {
    /// `~/.openclaw` (and the legacy `~/.clawdbot`, `~/.moltbot`).
    Openclaw,
}

/// `migrate cleanup` options.
#[derive(Debug, Clone, Args)]
pub struct CleanupArgs {
    /// The agent whose directories are archived.
    #[arg(value_enum)]
    pub agent: CleanupAgent,
    /// The directory to archive (default: every OpenClaw directory).
    #[arg(long, value_name = "PATH")]
    pub source: Option<PathBuf>,
    /// Do not ask first (required without a terminal).
    #[arg(long)]
    pub yes: bool,
}

/// `--from`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum FromCli {
    /// Claude Code session logs (`~/.claude/projects/**/*.jsonl`).
    ClaudeCode,
    /// Codex CLI rollouts (`~/.codex/sessions/**/rollout-*.jsonl`).
    Codex,
}

/// `migrate sessions` options.
#[derive(Debug, Clone, Args)]
pub struct SessionsArgs {
    /// The CLI the log comes from.
    #[arg(long, value_enum)]
    pub from: FromCli,
    /// The log file; without it a picker lists the logs.
    #[arg(value_name = "FILE", conflicts_with = "pick")]
    pub file: Option<PathBuf>,
    /// Pick the log from a list (newest first, with a preview).
    #[arg(long)]
    pub pick: bool,
    /// The CLI's config directory (default `~/.claude` or `~/.codex`).
    #[arg(long, value_name = "PATH")]
    pub source: Option<PathBuf>,
}

/// `migrate sync`.
///
/// # Errors
///
/// An invalid sync registry, home errors.
pub(super) fn sync(args: &SyncArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let user = user_home(ctx)?;
    let home = pmt_config::home::resolve(ctx.home_flag, ctx.launch)?;
    let profile = target_profile(ctx, &home)?;
    let host = super::host::MigrateHost::new(ctx.launch, super::interrupt::Interrupt::none());
    let results = imports::sync(&home, &profile, &user, args.dry_run, &host)?;
    let failed = results.iter().any(|r| r.status == "failed");
    if ctx.json {
        json(&serde_json::json!({ "imports": results }))?;
    } else if results.is_empty() {
        print_raw("no imports are registered for sync\n")?;
    } else {
        let mut out = String::new();
        for r in &results {
            let _ = writeln!(out, "{:<12} {}  {}", r.agent, r.source, r.status);
            if let Some(report) = &r.report {
                out.push_str(&report.items());
            }
        }
        print_raw(&out)?;
    }
    Ok(if failed {
        pmt_core::exit::FAILURE
    } else {
        pmt_core::exit::SUCCESS
    })
}

/// `migrate cleanup openclaw`.
///
/// # Errors
///
/// `migrate.source_busy` (75), `migrate.destination`.
pub(super) fn cleanup(args: &CleanupArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let CleanupAgent::Openclaw = args.agent;
    let user = user_home(ctx)?;
    let dirs = cleanup::targets(args.source.as_deref(), &user);
    if dirs.is_empty() {
        return print_raw("no OpenClaw directory to archive\n");
    }
    let mut listing = String::from("These directories will be renamed (nothing is deleted):\n");
    for d in &dirs {
        let _ = writeln!(
            listing,
            "  {} -> {}",
            d.display(),
            cleanup::archived(d).display()
        );
    }
    eprint!("{listing}");
    if let Some(code) = confirm("Archive them?", args.yes, ctx.json)? {
        return Ok(code);
    }
    let done = cleanup::archive(&dirs)?;
    if ctx.json {
        let renamed: Vec<_> = done
            .iter()
            .map(|(from, to)| serde_json::json!({"from": from, "to": to}))
            .collect();
        return json(&serde_json::json!({ "renamed": renamed }));
    }
    let mut out = String::new();
    for (from, to) in &done {
        let _ = writeln!(out, "renamed {} to {}", from.display(), to.display());
    }
    print_raw(&out)
}

/// `migrate sessions`.
///
/// # Errors
///
/// `migrate.source_missing` (66) for a missing log, `migrate.usage` (2)
/// without a file and a terminal, store errors.
pub(super) fn sessions(args: &SessionsArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let cli = match args.from {
        FromCli::ClaudeCode => transcripts::Cli::ClaudeCode,
        FromCli::Codex => transcripts::Cli::Codex,
    };
    let file = if let Some(f) = &args.file {
        f.clone()
    } else {
        let config_dir = match &args.source {
            Some(dir) => dir.clone(),
            None => user_home(ctx)?.join(match cli {
                transcripts::Cli::ClaudeCode => ".claude",
                transcripts::Cli::Codex => ".codex",
            }),
        };
        let Some(f) = pick(cli, &cli.logs_dir(&config_dir))? else {
            return Ok(pmt_core::exit::FAILURE);
        };
        f
    };
    let transcript = transcripts::read(cli, &file)?;
    let scope = ctx.scope()?;
    let store = pmt_store::Store::open(&scope)?;
    let imported = transcripts::import(&store, cli, &transcript)?;
    if ctx.json {
        return json(&imported);
    }
    let verb = if imported.created {
        "imported"
    } else {
        "already imported as"
    };
    print_raw(&format!(
        "{verb} {}\n{}\nresume it with: prometheus chat --resume {}\n",
        file.display(),
        imported.session_id,
        imported.session_id
    ))
}

/// The interactive picker: the logs newest first with a read-only preview
/// of their first message; `None` when the user picks nothing.
fn pick(cli: transcripts::Cli, dir: &Path) -> Result<Option<PathBuf>, Error> {
    if !prompt::stdin_is_tty() {
        return Err(Error::new(
            &codes::MIGRATE_USAGE,
            "give the log FILE (the picker needs a terminal)",
        ));
    }
    let logs: Vec<(PathBuf, String)> = transcripts::logs(cli, dir)
        .into_iter()
        .filter_map(|p| {
            let t = transcripts::read(cli, &p).ok()?;
            let preview: String = t
                .first_user()
                .unwrap_or_default()
                .chars()
                .take(70)
                .collect();
            Some((p, preview.replace('\n', " ")))
        })
        .take(30)
        .collect();
    if logs.is_empty() {
        return Err(Error::new(
            &codes::MIGRATE_SOURCE_MISSING,
            format!("no {} session logs under {}", cli.label(), dir.display()),
        ));
    }
    for (n, (path, preview)) in logs.iter().enumerate() {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        eprintln!("{:>3}  {name}  {preview}", n + 1);
    }
    eprint!("import which log? [1-{}, empty to cancel] ", logs.len());
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|e| prompt::read_error(&e))?;
    Ok(answer
        .trim()
        .parse::<usize>()
        .ok()
        .and_then(|n| logs.get(n.checked_sub(1)?))
        .map(|(p, _)| p.clone()))
}
