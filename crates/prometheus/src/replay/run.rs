//! Re-executing a recording (spec: replay-and-evaluation, Replay modes;
//! Divergence detection; Replay has no side effects; scheduled-jobs, Run
//! replay).
//!
//! The replay runs in a home of its own under the output directory: its
//! profile's `config.toml` is the recorded one (with plugins, MCP servers
//! and language servers off), its secrets are placeholders that are never
//! sent (only `model-live` gets the invoking profile's values for the
//! names the config references), its working directory is the output's
//! `workspace/` (the recorded files with `--include-workspace`), and its
//! session takes its recorded id. Each recorded user message runs as one
//! turn through the same session factory as `prometheus chat`, with the
//! [`Replayer`] answering model calls, tool calls and prompt builds.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pmt_agent::turn::{TurnInput, TurnOptions};
use pmt_config::{Home, ProfileName, ProfileScope};
use pmt_core::cancel::CancelToken;
use pmt_core::env::LaunchEnv;
use pmt_core::error::{Error, codes};
use pmt_core::id::SessionId;
use pmt_core::secret::SecretString;
use pmt_eval::compare;
use pmt_eval::recording::{Recording, member};
use pmt_obs::trajectory::files;
use pmt_store::{EndReason, Store};

use super::source::{Divergence, Replayer};
use super::{Hooks, Mode, OnDivergence, State};
use crate::chat::session::model::RouteFlags;
use crate::chat::session::{Factory, Options};
use crate::context::Context;

/// What stands in for a secret in a replay that never sends one.
const PLACEHOLDER: &str = "replay-placeholder-not-a-secret";

/// Settings a replay always runs with: nothing it starts may reach out.
const ISOLATED: [&str; 3] = [
    "plugins.safe_mode=true",
    "lsp.enabled=false",
    "replay.cache.mode=off",
];

/// How a replay runs.
#[derive(Debug, Clone, Copy)]
pub struct Settings<'a> {
    /// Served or live parts.
    pub mode: Mode,
    /// Stop or fall through at a divergence.
    pub on_divergence: OnDivergence,
    /// The output directory (home, workspace, trajectory).
    pub out: &'a Path,
    /// The launch environment (its own secrets are never used).
    pub launch: &'a LaunchEnv,
    /// Where `model-live` reads the provider secrets from.
    pub secrets_from: Option<&'a ProfileScope>,
}

/// How a replay went.
#[derive(Debug, Clone)]
pub struct Report {
    /// The first divergence.
    pub divergence: Option<Divergence>,
    /// Model calls served from the recording.
    pub served_calls: usize,
    /// The replay's trajectory (`<out>/trajectory.jsonl`).
    pub trajectory: Option<PathBuf>,
    /// How its agent-loop events differ from the recording's, ids and
    /// times aside (`None`: they are the same).
    pub trajectory_diff: Option<String>,
}

impl Report {
    /// A strict replay reproduced its recording: no divergence and the
    /// same trajectory.
    #[must_use]
    pub fn matched(&self) -> bool {
        self.divergence.is_none() && self.trajectory_diff.is_none()
    }

    /// What `replay run` and `cron replay` print.
    #[must_use]
    pub fn text(&self) -> String {
        if let Some(divergence) = &self.divergence {
            return format!("{}\n{}", divergence.headline(), divergence.diff);
        }
        let mut text = format!("replay matched: {} calls\n", self.served_calls);
        if let Some(diff) = &self.trajectory_diff {
            text.push_str("the trajectory differs from the recording:\n");
            text.push_str(diff);
        }
        text
    }
}

fn io_error(path: &Path, err: &std::io::Error) -> Error {
    Error::io(&codes::REPLAY_IO, path, err)
}

fn malformed(why: &str) -> Error {
    Error::new(
        &codes::REPLAY_MALFORMED,
        format!("not a replayable recording: {why}"),
    )
}

/// The user messages that started each turn of a trajectory, grouped by
/// the process that ran them (a resumed session starts again with
/// `session.start`; what a process measured lives with it, so the replay
/// opens the session once per group).
fn inputs(trajectory: &str) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut turn_open = false;
    for line in trajectory.lines() {
        let Ok(event) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match event["type"].as_str() {
            Some("session.start") => out.push(Vec::new()),
            Some("turn.start") => turn_open = true,
            Some("user.message") if turn_open => {
                let text = event["payload"]["text"].as_str().unwrap_or_default();
                if out.is_empty() {
                    out.push(Vec::new());
                }
                if let Some(group) = out.last_mut() {
                    group.push(text.to_owned());
                }
                turn_open = false;
            }
            _ => {}
        }
    }
    out
}

fn trajectory_text(recording: &Recording, session: &str) -> String {
    recording
        .get(&member::trajectory(session))
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .unwrap_or_default()
}

/// Replays `recording` with `settings`.
///
/// # Errors
///
/// `replay.malformed`, `replay.io`, and the errors of starting the
/// replay's session.
pub fn run(recording: &Recording, settings: &Settings<'_>) -> Result<Report, Error> {
    let state: State = recording
        .json(member::STATE)?
        .ok_or_else(|| malformed("no state.json"))?;
    let root = recording
        .root()
        .ok_or_else(|| malformed("no session"))?
        .to_owned();
    let workspace = prepare(recording, &state, settings)?;
    let stop = CancelToken::new();
    let source = Arc::new(Replayer::new(
        recording,
        (settings.mode, settings.on_divergence),
        stop.clone(),
    )?);
    let launch = launch(&state, settings);
    let home = settings.out.join("home");
    let set: Vec<String> = ISOLATED.iter().map(|s| (*s).to_owned()).collect();
    let ctx = Context {
        home_flag: Some(&home),
        launch: &launch,
        profile: None,
        set: &set,
        json: false,
    };
    let mock = settings.out.join("mock.toml");
    let flags = RouteFlags {
        provider: state.provider.as_deref(),
        model: state.model.as_deref(),
        mock_script: state.mock_script.is_some().then_some(mock.as_path()),
    };
    let options = Options {
        cwd: workspace,
        no_context_files: true,
        source_tag: state.source_tag.clone(),
        replay: Some(Arc::new(Hooks {
            tools: Arc::clone(&source) as _,
            pins: Arc::clone(&source) as _,
        })),
        ..Options::default()
    };
    let mut factory = Factory::new(ctx.scope()?, &launch, &flags, None, options)?;
    factory.models.replay_from(Arc::clone(&source) as _);
    factory.store.plan_session_ids([SessionId::parse(&root)?]);
    let original = trajectory_text(recording, &root);
    crate::model_cmd::block_on(session(factory, &flags, inputs(&original), &stop))??;
    let trajectory = copy_trajectory(&home, &root, settings.out)?;
    let replayed = match &trajectory {
        Some(path) => std::fs::read_to_string(path).map_err(|e| io_error(path, &e))?,
        None => String::new(),
    };
    let (recorded, replayed) = (
        compare::loop_events(&original),
        compare::loop_events(&replayed),
    );
    // Only a strict replay must reproduce the trajectory; the live modes
    // exist to differ.
    let trajectory_diff = (settings.mode == Mode::Strict && recorded != replayed).then(|| {
        compare::unified(
            &compare::pretty(&serde_json::Value::Array(recorded)),
            &compare::pretty(&serde_json::Value::Array(replayed)),
        )
    });
    Ok(Report {
        divergence: source.divergence(),
        served_calls: source.served_calls(),
        trajectory,
        trajectory_diff,
    })
}

/// Creates the output's home (the recorded config), workspace and mock
/// script; returns the workspace.
fn prepare(
    recording: &Recording,
    state: &State,
    settings: &Settings<'_>,
) -> Result<PathBuf, Error> {
    let out = settings.out;
    if settings.mode == Mode::ToolsLive && !state.workspace_included {
        return Err(Error::new(
            &codes::REPLAY_MALFORMED,
            "tools-live replays need a recording exported with --include-workspace",
        ));
    }
    let home = Home::open(&out.join("home"))?;
    let profile = home.profile(&ProfileName::default_profile())?;
    let config = profile.path().join("config.toml");
    pmt_core::private_fs::write_atomic(&config, state.config.as_bytes())
        .map_err(|e| io_error(&config, &e))?;
    let workspace = out.join("workspace");
    pmt_core::private_fs::create_dir(&workspace).map_err(|e| io_error(&workspace, &e))?;
    for (name, bytes) in recording.members() {
        if let Some(relative) = name.strip_prefix("workspace/files/") {
            unpack(&workspace, relative, bytes)?;
        }
    }
    if let Some(script) = &state.mock_script {
        let path = out.join("mock.toml");
        std::fs::write(&path, script).map_err(|e| io_error(&path, &e))?;
    }
    Ok(workspace)
}

/// Writes one recorded workspace file under `workspace`.
fn unpack(workspace: &Path, relative: &str, bytes: &[u8]) -> Result<(), Error> {
    let safe = Path::new(relative)
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)));
    if !safe {
        return Err(malformed(&format!("unsafe workspace path {relative}")));
    }
    let path = workspace.join(relative);
    if let Some(dir) = path.parent() {
        pmt_core::private_fs::create_dir(dir).map_err(|e| io_error(dir, &e))?;
    }
    std::fs::write(&path, bytes).map_err(|e| io_error(&path, &e))
}

/// The replay's launch environment: placeholders for every secret the
/// config names, or (model-live) the invoking profile's values.
fn launch(state: &State, settings: &Settings<'_>) -> LaunchEnv {
    let live = settings.mode == Mode::ModelLive;
    let mut secrets = Vec::new();
    for name in &state.secret_names {
        let value = match settings.secrets_from.filter(|_| live) {
            Some(scope) => scope.secrets().resolve(name).ok(),
            None => Some(SecretString::new(PLACEHOLDER)),
        };
        if let Some(value) = value {
            secrets.push((name.clone(), value));
        }
    }
    settings.launch.clone().with_secrets(secrets)
}

/// Runs each input as one turn, opening the session once per recorded
/// process (later ones resume it). How a turn ends (an error, a budget,
/// an interruption) is part of the trajectory the replay is compared by.
async fn session(
    factory: Factory,
    flags: &RouteFlags<'_>,
    groups: Vec<Vec<String>>,
    stop: &CancelToken,
) -> Result<(), Error> {
    let target = factory.models.primary(flags).await?;
    let id = factory.create_session(None, &target)?;
    for (n, turns) in groups.into_iter().enumerate() {
        if stop.is_cancelled() {
            break;
        }
        if n > 0 {
            factory.store.reopen_session(id)?;
        }
        let live = factory.open(id, &target).await?;
        for text in turns {
            if stop.is_cancelled() {
                break;
            }
            let input = TurnInput::text(text);
            if let Err(err) = live
                .agent
                .run_turn(Some(&live.scope), id, input, TurnOptions::new(stop))
                .await
            {
                tracing::warn!(error = %err, "replayed turn failed");
            }
        }
        live.close(&factory.store, EndReason::UserExit).await;
    }
    Ok(())
}

/// Copies the replayed root session's trajectory to
/// `<out>/trajectory.jsonl`.
fn copy_trajectory(home: &Path, root: &str, out: &Path) -> Result<Option<PathBuf>, Error> {
    let dir = home.join("profiles").join("default").join(files::DIR);
    let Some(path) = files::find(&dir, root) else {
        return Ok(None);
    };
    let mut text = Vec::new();
    files::open(&path)
        .and_then(|mut reader| std::io::Read::read_to_end(&mut reader, &mut text))
        .map_err(|e| io_error(&path, &e))?;
    let target = out.join("trajectory.jsonl");
    std::fs::write(&target, text).map_err(|e| io_error(&target, &e))?;
    Ok(Some(target))
}

/// `prometheus cron replay <run>` without `--live` (spec: scheduled-jobs,
/// Run replay): the run's session replayed strictly from the profile's
/// recording, in a scratch directory under the profile's cache; nothing
/// is sent and nothing is delivered.
///
/// # Errors
///
/// `replay.not_recorded` for a run without a recorded session, and the
/// replay's errors.
pub fn cron(
    scope: &ProfileScope,
    store: &Store,
    launch: &LaunchEnv,
    run: &pmt_store::cron::RunRow,
) -> Result<u8, Error> {
    let session = run
        .session_id
        .as_deref()
        .map(SessionId::parse)
        .transpose()?
        .ok_or_else(|| {
            Error::new(
                &codes::REPLAY_NOT_RECORDED,
                format!("run {} has no agent session to replay", run.id),
            )
        })?;
    let options = super::export::Options::default();
    let recording = super::export::recording(scope, store, session, options)?;
    let scratch = scope.path("cache/replay").join(&run.id);
    let _ = std::fs::remove_dir_all(&scratch);
    pmt_core::private_fs::create_dir(&scratch).map_err(|e| io_error(&scratch, &e))?;
    let report = self::run(
        &recording,
        &Settings {
            mode: Mode::Strict,
            on_divergence: OnDivergence::Stop,
            out: &scratch,
            launch,
            secrets_from: None,
        },
    );
    let _ = std::fs::remove_dir_all(&scratch);
    let report = report?;
    crate::output::print_raw(&report.text())?;
    Ok(if report.matched() {
        0
    } else {
        pmt_core::exit::FAILURE
    })
}
