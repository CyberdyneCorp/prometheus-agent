//! `prometheus console` (spec: cli, Safe command console): a REPL that runs
//! only an allowlist of read-mostly `prometheus` subcommands. Each line is
//! parsed by the CLI parser itself, never by a shell: a line holding shell
//! syntax (`!` escapes, pipes, redirection, command separators,
//! substitutions) is refused before anything runs, and so is any command
//! outside the allowlist, with `not available in the console`. An allowed
//! line runs as a child `prometheus` process (no shell between) on the
//! console's home and `-p` profile.

use std::io::{BufRead as _, Write as _};

use clap::Parser as _;
use pmt_core::error::{Error, codes};

use crate::cli::{Cli, Command, ConfigCommand, SessionsCommand};
use crate::context::Context;

/// The refusal of everything else.
pub const REFUSAL: &str = "not available in the console";

/// What the console offers, as `help` lists it.
pub const ALLOWED: &[&str] = &[
    "status",
    "doctor",
    "logs",
    "sessions list|show|export",
    "config show|get|validate",
    "usage",
    "version",
    "update --check",
    "daemon status|restart",
    "gateway status",
];

/// Characters a shell would act on: none of them is ever passed on.
const SHELL_SYNTAX: &[char] = &[';', '|', '&', '<', '>', '`', '$', '!', '\n', '\r'];

/// Why a line does not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// Shell syntax or a command outside the allowlist.
    NotAvailable,
    /// The words do not parse as a command (clap's message).
    Usage(String),
}

/// The words of `line` when it may run: free of shell syntax, parsed by
/// the CLI parser to an allowlisted command with no global override of
/// the console's home, profile or configuration.
///
/// # Errors
///
/// Why it may not run.
pub fn admit(line: &str) -> Result<Vec<String>, Refused> {
    if line.contains(SHELL_SYNTAX) {
        return Err(Refused::NotAvailable);
    }
    let words = shlex::split(line).ok_or(Refused::NotAvailable)?;
    let argv = std::iter::once("prometheus".to_owned()).chain(words.iter().cloned());
    let cli = Cli::try_parse_from(argv).map_err(|e| {
        let text = e.to_string();
        Refused::Usage(text.lines().next().unwrap_or_default().to_owned())
    })?;
    let overrides = cli.home.is_some() || !cli.profile.is_empty() || !cli.set.is_empty();
    if overrides || cli.safe_mode || cli.accept_hooks || !cli.chat.is_empty() {
        return Err(Refused::NotAvailable);
    }
    let allowed = cli.print_version || cli.command.as_ref().is_some_and(allowed);
    if allowed {
        Ok(words)
    } else {
        Err(Refused::NotAvailable)
    }
}

/// Whether `command` is on the allowlist.
fn allowed(command: &Command) -> bool {
    match command {
        Command::Status(_)
        | Command::Doctor(_)
        | Command::Logs(_)
        | Command::Usage(_)
        | Command::Version => true,
        Command::Sessions { command } => matches!(
            command,
            SessionsCommand::List { .. }
                | SessionsCommand::Show { .. }
                | SessionsCommand::Export(_)
        ),
        Command::Config { command } => matches!(
            command,
            ConfigCommand::Show { .. } | ConfigCommand::Get { .. } | ConfigCommand::Validate
        ),
        Command::Update(args) => update_check_only(args),
        _ => gateway_allowed(command),
    }
}

/// `update --check` and nothing else.
fn update_check_only(args: &crate::cli::UpdateArgs) -> bool {
    let crate::cli::UpdateArgs {
        check,
        status,
        channel,
        set_channel,
        allow_downgrade,
        rollback,
        restore_snapshots,
        yes,
        dry_run,
        from_bundle,
        no_restart,
        backup,
        no_backup,
    } = args;
    *check
        && !(*status
            || *allow_downgrade
            || *rollback
            || *restore_snapshots
            || *yes
            || *dry_run
            || *no_restart
            || *backup
            || *no_backup)
        && channel.is_none()
        && set_channel.is_none()
        && from_bundle.is_none()
}

#[cfg(feature = "gateway")]
fn gateway_allowed(command: &Command) -> bool {
    use crate::daemon_cmd::{DaemonCommand, GatewayCommand};
    match command {
        Command::Daemon { command } => {
            matches!(command, DaemonCommand::Status | DaemonCommand::Restart)
        }
        Command::Gateway { command } => matches!(command, GatewayCommand::Status),
        _ => false,
    }
}

#[cfg(not(feature = "gateway"))]
fn gateway_allowed(_: &Command) -> bool {
    false
}

/// Runs the console on stdin until `exit`, `quit` or the end of input.
///
/// # Errors
///
/// The binary cannot be found or the terminal fails.
pub fn run(ctx: &Context<'_>) -> Result<u8, Error> {
    let exe = std::env::current_exe()
        .map_err(|e| Error::new(&codes::OUTPUT_IO, format!("cannot find this binary: {e}")))?;
    // The console's own home and profile, resolved once: every line runs
    // on them.
    let home = ctx.home()?;
    let profile = ctx.profile_dir()?.name().to_string();
    let mut input = std::io::stdin().lock();
    let mut err = std::io::stderr();
    let _ = writeln!(
        err,
        "prometheus console on profile '{profile}' (help lists the commands, exit leaves)"
    );
    let mut status = 0;
    loop {
        let _ = write!(err, "console› ");
        let _ = err.flush();
        let mut line = String::new();
        let read = input
            .read_line(&mut line)
            .map_err(|e| Error::new(&codes::OUTPUT_IO, format!("cannot read: {e}")))?;
        let line = line.trim();
        match line {
            _ if read == 0 => break,
            "" => {}
            "exit" | "quit" => break,
            "help" => {
                let _ = writeln!(err, "commands: {}", ALLOWED.join(", "));
            }
            _ => status = run_line(&exe, (home.root(), &profile), line),
        }
    }
    Ok(status)
}

/// Runs one console line; its exit status (1 when refused).
fn run_line(exe: &std::path::Path, (home, profile): (&std::path::Path, &str), line: &str) -> u8 {
    match admit(line) {
        Ok(words) => {
            let outcome = std::process::Command::new(exe)
                .arg("--home")
                .arg(home)
                .args(["-p", profile])
                .args(&words)
                .status();
            match outcome {
                Ok(done) => done.code().and_then(|c| u8::try_from(c).ok()).unwrap_or(1),
                Err(e) => {
                    eprintln!("error: cannot run: {e}");
                    1
                }
            }
        }
        Err(Refused::NotAvailable) => {
            eprintln!("{line}: {REFUSAL}");
            1
        }
        Err(Refused::Usage(message)) => {
            eprintln!("{message}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_syntax_and_other_commands_are_refused() {
        for line in [
            "logs; rm -rf /",
            "logs | sh",
            "status > out.txt",
            "status && reboot",
            "!ls",
            "version `id`",
            "usage $(id)",
            "config set model.default x",
            "sessions delete ses_1",
            "update",
            "update --check --yes",
            "-z hello",
            "status -p other",
            "status --home /",
            "secrets list",
        ] {
            assert_eq!(admit(line), Err(Refused::NotAvailable), "{line}");
        }
    }

    #[test]
    fn allowlisted_commands_parse_into_their_words() {
        assert_eq!(admit("status").unwrap(), ["status"]);
        assert_eq!(
            admit("sessions show 'my title'").unwrap(),
            ["sessions", "show", "my title"]
        );
        for line in [
            "doctor",
            "logs",
            "usage",
            "version",
            "--json version",
            "config show",
            "config get model.default",
            "config validate",
            "sessions list",
            "update --check",
        ] {
            assert!(admit(line).is_ok(), "{line}");
        }
        assert!(matches!(admit("status --bogus"), Err(Refused::Usage(_))));
    }
}
