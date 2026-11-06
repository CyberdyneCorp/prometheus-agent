//! `prometheus demo [--output DIR] [--keep]` (spec: replay-and-evaluation,
//! Offline smoke demo): with the scripted mock provider and no network,
//! the agent inspects a small ledger repository, runs its balance script
//! through the terminal tool, finds the sign bug, patches the script,
//! re-runs it and reports `Total: 85.15`.
//!
//! It runs one real one-shot chat session (the same factory, turn loop,
//! tools and trajectory writer as `prometheus chat -z`) in a home of its
//! own under the demo's directory, so the user's homes and profiles are
//! never read or written. Afterwards it prints the session's trajectory in
//! the canonical rendering and exits 0 only when the final answer
//! reports the expected total.

mod place;
mod scenario;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use pmt_agent::turn::{ExitReason, TurnInput, TurnOptions, TurnResult};
use pmt_core::cancel::CancelToken;
use pmt_core::env::LaunchEnv;
use pmt_core::error::Error;
use pmt_core::id::SessionId;
use pmt_obs::trajectory::render::{RenderOptions, render};
use pmt_obs::trajectory::{files, read};
use pmt_store::EndReason;

pub use self::place::Place;
pub use self::scenario::EXPECTED;

use crate::chat::session::model::RouteFlags;
use crate::chat::session::{Factory, Options};
use crate::cli::DemoArgs;
use crate::context::Context;

/// The demo's time limit (the spec's bound on the reference runner); the
/// turn is stopped when it runs longer.
pub const TIME_LIMIT: Duration = Duration::from_secs(30);

/// The source tag of the demo's session.
const SOURCE_TAG: &str = "demo";

/// Runs `prometheus demo`.
///
/// # Errors
///
/// `demo.output_not_empty` (2), `demo.io` (74) and the errors of starting
/// the session; a session that ran reports its own verdict instead.
pub fn run(args: &DemoArgs, launch: &LaunchEnv) -> Result<u8, Error> {
    let place = Place::prepare(args.output.as_deref())?;
    let started = Instant::now();
    let outcome = scenario::write(&place.repository(), &place.mock_script())
        .and_then(|()| run_session(&place, launch));
    let status = match outcome {
        Ok(session) => {
            let status = report(&session, &place.home(), started.elapsed());
            if args.keep {
                println!(
                    "kept {}; replay the steps with: prometheus --home {} trajectory show {}",
                    place.root().display(),
                    place.home().display(),
                    session.id
                );
            } else if !matches!(status, Ok(0)) {
                println!("hint: rerun with --keep to inspect the demo's home and repository");
            }
            status
        }
        Err(err) => Err(err),
    };
    place.finish(args.keep);
    status
}

/// Whether this platform has an OS sandbox backend (pmt-sandbox: Linux,
/// macOS, Windows).
const HAS_OS_SANDBOX: bool = cfg!(any(target_os = "linux", target_os = "macos", windows));

/// On a platform without an OS sandbox (Android/Termux), commands only run
/// with `security.sandbox.allow_degraded = ["all"]`. The demo's own
/// throwaway profile takes that downgrade for its two scripted commands,
/// and says so: it is never silent, and the user's profiles are untouched.
fn unconfined_override() -> Vec<String> {
    if HAS_OS_SANDBOX {
        return Vec::new();
    }
    println!(
        "note: {} has no OS sandbox; the demo's scripted commands run unconfined in its temporary directory",
        std::env::consts::OS
    );
    vec![r#"security.sandbox.allow_degraded=["all"]"#.to_owned()]
}

/// What the demo's session produced.
struct Session {
    id: SessionId,
    turn: Result<TurnResult, Error>,
}

/// The demo must stay offline and the same everywhere, but its ledger
/// repository may sit inside a git tree (a checkout holding the temporary
/// directory), where a session would start a language server for the
/// patched script and, with `lsp.install_strategy = "auto"`, install it
/// over the network first. The demo's profile starts (and so installs) no
/// language server.
const OFFLINE: &str = "lsp.enabled=false";

/// Runs the scenario's one turn in the demo's home.
fn run_session(place: &Place, launch: &LaunchEnv) -> Result<Session, Error> {
    let home = place.home();
    let mut set = unconfined_override();
    set.push(OFFLINE.to_owned());
    let ctx = Context {
        home_flag: Some(&home),
        launch,
        profile: None,
        set: &set,
        json: false,
    };
    let scope = ctx.scope()?;
    let mock_script = place.mock_script();
    let flags = RouteFlags {
        provider: Some(pmt_providers::mock::PROVIDER_ID),
        model: Some(pmt_providers::mock::DEFAULT_MODEL),
        mock_script: Some(&mock_script),
    };
    let options = Options {
        interactive: false,
        yolo: false,
        cwd: place.repository(),
        skills: Vec::new(),
        pass_session_id: false,
        // Nothing from the machine shapes the prompt: the demo is the same
        // everywhere.
        no_context_files: true,
        source_tag: Some(SOURCE_TAG.to_owned()),
        ignore_user_config: false,
        ignore_rules: false,
        egress: crate::egress::Overrides::default(),
    };
    let factory = Factory::new(scope, launch, &flags, None, options)?;
    crate::model_cmd::block_on(session(factory, &flags))?
}

async fn session(factory: Factory, flags: &RouteFlags<'_>) -> Result<Session, Error> {
    let target = factory.models.primary(flags).await?;
    let id = factory.create_session(None, &target)?;
    let live = factory.open(id, &target).await?;
    let mut options = TurnOptions::new(&CancelToken::new());
    options.run_budget = Some(TIME_LIMIT);
    let turn = live
        .agent
        .run_turn(
            Some(&live.scope),
            id,
            TurnInput::text(scenario::PROMPT),
            options,
        )
        .await;
    let reason = match &turn {
        Ok(result) if result.exit_reason == ExitReason::Completed => EndReason::UserExit,
        _ => EndReason::Error,
    };
    live.close(&factory.store, reason).await;
    Ok(Session { id, turn })
}

/// Prints the trajectory and the verdict; returns the exit status.
fn report(session: &Session, home: &Path, elapsed: Duration) -> Result<u8, Error> {
    if let Some(path) = files::find(&trajectories(home), &session.id.to_string()) {
        let lines = read::read_file(&path)?;
        print!("{}", render(&lines, &RenderOptions::default())?);
    }
    println!();
    match verdict(&session.turn) {
        Ok(()) => {
            println!("demo passed in {:.1} s", elapsed.as_secs_f64());
            Ok(0)
        }
        Err(reason) => {
            println!("demo failed: {reason}");
            Ok(pmt_core::exit::FAILURE)
        }
    }
}

/// Passes when the turn completed and its final answer reports
/// [`EXPECTED`].
fn verdict(turn: &Result<TurnResult, Error>) -> Result<(), String> {
    let result = turn.as_ref().map_err(|err| err.message().to_owned())?;
    if result.exit_reason != ExitReason::Completed {
        let detail = result
            .error
            .as_ref()
            .map(|err| format!(": {}", err.message()))
            .unwrap_or_default();
        return Err(format!(
            "the turn ended with {}{detail}",
            result.exit_reason.as_str()
        ));
    }
    match result.final_response.as_deref() {
        Some(answer) if answer.contains(EXPECTED) => Ok(()),
        _ => Err(format!("the final answer does not report {EXPECTED}")),
    }
}

/// The trajectory directory of the demo home's (default) profile.
fn trajectories(home: &Path) -> PathBuf {
    home.join("profiles").join("default").join(files::DIR)
}

#[cfg(test)]
mod tests {
    use pmt_agent::turn::{ExitReason, TurnId, TurnResult, TurnUsage};
    use pmt_core::error::{Error, codes};

    use super::verdict;

    fn turn(exit_reason: ExitReason, answer: &str) -> TurnResult {
        TurnResult {
            turn_id: TurnId::new(),
            final_response: Some(answer.to_owned()),
            exit_reason,
            messages_appended: Vec::new(),
            usage: TurnUsage::default(),
            iterations: 6,
            interrupt_issuer: None,
            notices: Vec::new(),
            error: None,
        }
    }

    #[test]
    fn only_a_completed_turn_reporting_the_total_passes() {
        assert_eq!(
            verdict(&Ok(turn(ExitReason::Completed, "Fixed.\n\nTotal: 85.15"))),
            Ok(())
        );
        assert_eq!(
            verdict(&Ok(turn(ExitReason::Completed, "Total: 235.35"))),
            Err("the final answer does not report Total: 85.15".to_owned())
        );
        let stopped = verdict(&Ok(turn(ExitReason::IterationLimit, "Total: 85.15"))).unwrap_err();
        assert!(stopped.starts_with("the turn ended with"), "{stopped}");
        let failed = verdict(&Err(Error::new(&codes::DEMO_IO, "disk full"))).unwrap_err();
        assert_eq!(failed, "disk full");
    }
}
