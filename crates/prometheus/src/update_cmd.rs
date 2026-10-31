//! `prometheus update` (spec: distribution-and-update): `--status`,
//! `--check`, `--set-channel`, `--rollback [--restore-snapshots]` and the
//! update itself. The work lives in `pmt-update`; this module builds the
//! install description, prints results and maps them to exit codes (0, 1,
//! 2, 4, 10, 69, 75, 77).

use std::path::PathBuf;

use pmt_config::daemon::DaemonConfig;
use pmt_core::error::{Error, codes};
use pmt_net::OutboundPolicy;
use pmt_update::flow::{
    self, Checked, Install, RollbackOptions, RolledBack, Session, UpdateOptions, Updated,
};
use pmt_update::{Channel, Identity, ReleaseVersion, Releases, TrustRoot};
use serde_json::json;

use crate::cli::UpdateArgs;
use crate::context::Context;
use crate::{build_info, output, prompt};

/// Runs `prometheus update`.
///
/// # Errors
///
/// See the module documentation; every error carries its exit code.
pub fn run(args: &UpdateArgs, ctx: &Context<'_>) -> Result<u8, Error> {
    let span = tracing::info_span!("update", component = "update");
    let _entered = span.enter();
    let channel = args
        .channel
        .as_deref()
        .map(str::parse::<Channel>)
        .transpose()?;
    let set_channel = args
        .set_channel
        .as_deref()
        .map(str::parse::<Channel>)
        .transpose()?;
    let install = install(ctx)?;
    if args.status {
        return status(&install, ctx.json);
    }
    if let Some(channel) = set_channel {
        flow::set_channel(&install, channel)?;
        println!("channel: {channel}");
        return Ok(pmt_core::exit::SUCCESS);
    }
    if args.rollback {
        return rollback(&install, args);
    }
    let config = DaemonConfig::load(install.home.root())?.update;
    let releases = Releases::new(
        config.mirror_url.as_deref(),
        Identity {
            version: build_info::VERSION,
            target: build_info::TARGET,
        },
        &outbound_policy(ctx),
        TrustRoot::embedded(),
        config.require_transparency_log,
    )?;
    let progress = |line: &str| eprintln!("{line}");
    let session = Session {
        install: &install,
        config: &config,
        releases: &releases,
        progress: &progress,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| {
            Error::new(
                &codes::UPDATE_FAILED,
                format!("cannot start the runtime: {e}"),
            )
        })?;
    if args.check {
        let checked = runtime.block_on(flow::check(&session, channel))?;
        return print_check(&checked, ctx.json);
    }
    let options = UpdateOptions {
        channel,
        allow_downgrade: args.allow_downgrade,
    };
    let updated = runtime.block_on(flow::update(&session, options))?;
    print_updated(&updated);
    Ok(pmt_core::exit::SUCCESS)
}

/// The running install: this executable (symlinks such as `pmt`
/// resolved), the home, and this build's version and target.
fn install(ctx: &Context<'_>) -> Result<Install, Error> {
    let binary = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|e| {
            Error::new(
                &codes::UPDATE_FAILED,
                format!("cannot locate this binary: {e}"),
            )
        })?;
    Ok(Install {
        binary,
        home: ctx.home()?,
        version: ReleaseVersion::parse(build_info::VERSION)?,
        target: build_info::TARGET.to_owned(),
    })
}

/// The selected profile's network settings (IPv4-only, extra CA); the
/// defaults when it does not load, since an update must not depend on a
/// profile's health.
fn outbound_policy(ctx: &Context<'_>) -> OutboundPolicy {
    match ctx.scope() {
        Ok(scope) => {
            let network = &scope.config().network;
            OutboundPolicy {
                force_ipv4: network.force_ipv4,
                extra_ca_file: network.extra_ca_file.clone().map(PathBuf::from),
            }
        }
        Err(err) => {
            tracing::warn!("update uses default network settings: {err}");
            OutboundPolicy::default()
        }
    }
}

fn status(install: &Install, json: bool) -> Result<u8, Error> {
    let status = flow::status(install)?;
    if json {
        let body = json!({
            "owner": status.owner.as_str(),
            "channel": status.channel.as_str(),
            "version": status.version.to_string(),
            "target": status.target,
            "binary": status.binary,
            "update_with": status.owner.update_command(),
            "last": status.last,
        });
        return output::document("update --status", &body);
    }
    print!("{}", status.render());
    Ok(pmt_core::exit::SUCCESS)
}

fn print_check(checked: &Checked, json: bool) -> Result<u8, Error> {
    let code = checked.check_exit_code();
    if json {
        let latest = match checked {
            Checked::Available { to, .. } => *to,
            Checked::Current(v) => *v,
            Checked::Ahead { latest, .. } => *latest,
        };
        let body = json!({
            "available": code == 10,
            "latest": latest.to_string(),
            "message": checked.message(),
        });
        output::document("update --check", &body)?;
    } else {
        println!("{}", checked.message());
    }
    Ok(code)
}

fn print_updated(updated: &Updated) {
    println!("{}", updated.message());
    if let Updated::Installed { snapshots, .. } = updated {
        for taken in snapshots {
            println!(
                "pre-update snapshot of profile {}: {}",
                taken.profile, taken.id
            );
        }
    }
}

fn rollback(install: &Install, args: &UpdateArgs) -> Result<u8, Error> {
    let yes = args.yes;
    // The question states how many sessions would be lost; it is printed
    // even when --yes answers it.
    let confirm = |question: &str| -> Option<bool> {
        if yes {
            eprintln!("{question} yes (--yes)");
            return Some(true);
        }
        if !prompt::stdin_is_tty() {
            eprintln!("{question}");
            return None;
        }
        eprint!("{question} [y/N] ");
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).ok()?;
        Some(matches!(answer.trim(), "y" | "Y" | "yes"))
    };
    let options = RollbackOptions {
        restore_snapshots: args.restore_snapshots,
    };
    match flow::rollback(install, options, &confirm)? {
        RolledBack::Done { version, restored } => {
            for taken in &restored {
                println!(
                    "restored snapshot {} of profile {}",
                    taken.id, taken.profile
                );
            }
            println!("rolled back to {version}");
            Ok(pmt_core::exit::SUCCESS)
        }
        RolledBack::Declined => {
            println!("rollback cancelled; nothing changed");
            Ok(pmt_core::exit::SUCCESS)
        }
        RolledBack::NeedsConfirmation => {
            eprintln!(
                "--restore-snapshots discards sessions created since the update: rerun with --yes to confirm"
            );
            Ok(prompt::NEEDS_CONFIRMATION)
        }
    }
}
